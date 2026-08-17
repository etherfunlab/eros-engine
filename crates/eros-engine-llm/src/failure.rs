// SPDX-License-Identifier: AGPL-3.0-only
//! Structured LLM failure records: what the provider said, and where our path
//! to it broke.

use serde::{Deserialize, Serialize};

/// One attempt where the **provider spoke** — it returned a status line, or a
/// `200` body carrying an error envelope. `http_status` is a raw `u16` and never
/// an enum: OpenRouter reports overload as `529` while Venice uses `429` and
/// `503`, and the next provider will differ again. Classification is by HTTP
/// convention (below), so an unrecognised code still behaves sensibly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamAttempt {
    /// The `[tasks.*]` key for this call.
    pub task: String,
    /// Full config slug of the attempted model, `@provider` suffix retained.
    pub model: String,
    /// What the provider actually returned. `200` for a mid-stream error.
    pub http_status: u16,
    /// OpenRouter `error.code` / Venice `error.code`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<String>,
    /// OpenRouter `metadata.error_type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    /// OpenRouter `metadata.provider_code` — the provider's own upstream code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_provider_code: Option<String>,
    /// Parsed from the `Retry-After` response header. Recorded and passed on;
    /// the engine never acts on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_s: Option<u32>,
    /// Scrubbed, flattened, capped. Never carries prompt text.
    pub message: String,
}

/// Where the engine's path to the provider broke. Named for the gateway *role*,
/// not the process: a panic in the affinity math is an engine error and does not
/// belong here; a TLS reset while reaching OpenRouter does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayError {
    pub task: String,
    /// Absent when the failure precedes model selection (a config error), and on
    /// the chain-scoped `ChainExhausted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub kind: GatewayKind,
    pub message: String,
}

/// Gateway-layer failure modes, uniform across every table.
///
/// The three timeouts stay distinct: issue #188 split them apart because folding
/// them together made idle timeouts invisible in dashboards. Here the
/// distinction becomes SQL-queryable rather than log-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayKind {
    /// Connect / queue / response-headers timeout.
    OpenTimeout,
    /// One attempt's whole generation exceeded its cap.
    TotalTimeout,
    /// Byte-level idle watchdog fired mid-stream.
    IdleTimeout,
    /// Connection reset, TLS failure, SSE body interrupted.
    Transport,
    /// A response arrived but could not be parsed.
    Decode,
    /// Local misconfiguration (empty model slug, unresolvable provider).
    Config,
    /// Chain-scoped: every candidate failed. Carries no `model`.
    ChainExhausted,
}

/// One failed attempt, routed to whichever column owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttemptFailure {
    Upstream(UpstreamAttempt),
    Gateway(GatewayError),
}

/// `5xx` is the provider's fault. Deliberately a range test and not a list —
/// an unrecognised `570` must classify without a code change.
pub fn is_upstream_status(status: u16) -> bool {
    (500..600).contains(&status)
}

/// Every `5xx`, plus the two `4xx` that HTTP defines as try-again:
/// `408 Request Timeout` and `429 Too Many Requests`.
pub fn is_retryable_status(status: u16) -> bool {
    is_upstream_status(status) || status == 408 || status == 429
}

/// The status the engine returns downstream for this failure. An upstream
/// status passes through verbatim; a gateway timeout becomes `504`; everything
/// else becomes `502`.
pub fn response_status_for(f: &AttemptFailure) -> u16 {
    match f {
        AttemptFailure::Upstream(a) => a.http_status,
        AttemptFailure::Gateway(g) => match g.kind {
            GatewayKind::OpenTimeout | GatewayKind::TotalTimeout | GatewayKind::IdleTimeout => 504,
            _ => 502,
        },
    }
}

impl std::fmt::Display for AttemptFailure {
    /// The inner `message` is already bounded, scrubbed and single-line — it is
    /// what belongs in a user-facing body, not a struct dump.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttemptFailure::Upstream(a) => f.write_str(&a.message),
            AttemptFailure::Gateway(g) => f.write_str(&g.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_5xx_classifies_as_upstream_and_retryable() {
        // No allow-list anywhere: an unrecognised 5xx must behave like a known
        // one, because a third provider will invent codes we have not seen.
        for s in [500u16, 502, 503, 529, 570, 599] {
            assert!(is_upstream_status(s), "{s} should be upstream");
            assert!(is_retryable_status(s), "{s} should be retryable");
        }
    }

    #[test]
    fn unknown_4xx_classifies_as_client_and_not_retryable() {
        for s in [400u16, 402, 403, 418, 451] {
            assert!(!is_upstream_status(s), "{s} should not be upstream");
            assert!(!is_retryable_status(s), "{s} should not be retryable");
        }
    }

    #[test]
    fn timeout_and_rate_limit_are_the_two_retryable_4xx() {
        for s in [408u16, 429] {
            assert!(!is_upstream_status(s), "{s} is still a 4xx");
            assert!(is_retryable_status(s), "{s} must be retryable");
        }
    }

    #[test]
    fn response_status_uses_the_upstream_value_verbatim() {
        let f = AttemptFailure::Upstream(UpstreamAttempt {
            task: "chat_companion".into(),
            model: "x-ai/grok-4.20".into(),
            http_status: 529,
            provider_code: Some("529".into()),
            error_type: Some("overloaded".into()),
            upstream_provider_code: None,
            retry_after_s: Some(30),
            message: "code=529: Overloaded".into(),
        });
        assert_eq!(response_status_for(&f), 529);
    }

    #[test]
    fn gateway_timeouts_map_to_504_and_everything_else_to_502() {
        let mk = |kind| {
            AttemptFailure::Gateway(GatewayError {
                task: "chat_companion".into(),
                model: Some("m".into()),
                kind,
                message: "x".into(),
            })
        };
        assert_eq!(response_status_for(&mk(GatewayKind::OpenTimeout)), 504);
        assert_eq!(response_status_for(&mk(GatewayKind::TotalTimeout)), 504);
        assert_eq!(response_status_for(&mk(GatewayKind::IdleTimeout)), 504);
        assert_eq!(response_status_for(&mk(GatewayKind::Transport)), 502);
        assert_eq!(response_status_for(&mk(GatewayKind::Decode)), 502);
        assert_eq!(response_status_for(&mk(GatewayKind::Config)), 502);
        assert_eq!(response_status_for(&mk(GatewayKind::ChainExhausted)), 502);
    }

    #[test]
    fn gateway_kind_serialises_snake_case() {
        let json = serde_json::to_value(GatewayKind::ChainExhausted).unwrap();
        assert_eq!(json, serde_json::json!("chain_exhausted"));
        let json = serde_json::to_value(GatewayKind::OpenTimeout).unwrap();
        assert_eq!(json, serde_json::json!("open_timeout"));
    }

    #[test]
    fn optional_fields_are_omitted_not_nulled() {
        let a = UpstreamAttempt {
            task: "pde_decision".into(),
            model: "m".into(),
            http_status: 502,
            provider_code: None,
            error_type: None,
            upstream_provider_code: None,
            retry_after_s: None,
            message: "boom".into(),
        };
        let v = serde_json::to_value(&a).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("provider_code"), "absent, not null: {v}");
        assert!(!obj.contains_key("retry_after_s"), "absent, not null: {v}");
        assert_eq!(obj["http_status"], 502);
    }

    #[test]
    fn gateway_error_omits_model_when_absent() {
        let g = GatewayError {
            task: "chat_companion".into(),
            model: None,
            kind: GatewayKind::ChainExhausted,
            message: "all candidates failed".into(),
        };
        let v = serde_json::to_value(&g).unwrap();
        assert!(!v.as_object().unwrap().contains_key("model"), "{v}");
    }

    #[test]
    fn upstream_displays_as_its_message_and_nothing_else() {
        let f = AttemptFailure::Upstream(UpstreamAttempt {
            task: "chat_companion".into(),
            model: "x-ai/grok-4.20".into(),
            http_status: 529,
            provider_code: None,
            error_type: None,
            upstream_provider_code: None,
            retry_after_s: None,
            message: "code=529: Overloaded".into(),
        });
        assert_eq!(f.to_string(), "code=529: Overloaded");
    }
}

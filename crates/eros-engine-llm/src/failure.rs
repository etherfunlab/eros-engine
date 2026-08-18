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
///
/// Verbatim has one bound, and it is not an allow-list. A status below `400`
/// is not an error status at all, so forwarding it would tell the consumer the
/// call SUCCEEDED — `LlmError::Provider` is exactly that case, a `200` body
/// that carried an error envelope. Those become `502`; the real value is still
/// on the `upstream_status` body field and in `llm_attempts`, which is where
/// the fact belongs. Anything `>= 400` still passes through untouched,
/// including codes we have never seen.
pub fn response_status_for(f: &AttemptFailure) -> u16 {
    match f {
        AttemptFailure::Upstream(a) if a.http_status >= 400 => a.http_status,
        AttemptFailure::Upstream(_) => 502,
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

use crate::openrouter::STREAM_IDLE_TIMEOUT_MSG;
use crate::LlmError;

impl AttemptFailure {
    /// The single place an `LlmError` is classified into a home. The `match` is
    /// exhaustive on purpose: a new `LlmError` variant fails to compile here
    /// until someone decides which layer owns it.
    pub fn from_llm_error(task: &str, model: &str, e: &LlmError) -> Self {
        let gateway = |kind: GatewayKind| {
            AttemptFailure::Gateway(GatewayError {
                task: task.to_string(),
                model: Some(model.to_string()),
                kind,
                message: e.to_string(),
            })
        };
        match e {
            LlmError::Status(status, body, retry_after) => {
                AttemptFailure::Upstream(UpstreamAttempt {
                    task: task.to_string(),
                    model: model.to_string(),
                    http_status: status.as_u16(),
                    provider_code: body.code.clone(),
                    error_type: body.error_type.clone(),
                    upstream_provider_code: body.provider_code.clone(),
                    retry_after_s: *retry_after,
                    message: body.message.clone(),
                })
            }
            // A 200 body that carried an error envelope, or a stream that ended
            // with finish_reason=error. The provider spoke; the status line just
            // was not where it said so.
            LlmError::Provider(body) => AttemptFailure::Upstream(UpstreamAttempt {
                task: task.to_string(),
                model: model.to_string(),
                http_status: 200,
                provider_code: body.code.clone(),
                error_type: body.error_type.clone(),
                upstream_provider_code: body.provider_code.clone(),
                retry_after_s: None,
                message: body.message.clone(),
            }),
            LlmError::Stream(msg) if msg.contains(STREAM_IDLE_TIMEOUT_MSG) => {
                gateway(GatewayKind::IdleTimeout)
            }
            LlmError::Stream(_) | LlmError::Http(_) => gateway(GatewayKind::Transport),
            // `StreamParse` carries up to 256 raw bytes of the `data:` frame
            // that failed to decode. That frame is provider output: it can hold
            // generated reply text, and on an echoing provider, the user's own
            // prompt. It was only ever a log line before; a column and an HTTP
            // error body are a different blast radius, so the payload stays in
            // the log and the recorded message says only what broke.
            LlmError::StreamParse(_) => AttemptFailure::Gateway(GatewayError {
                task: task.to_string(),
                model: Some(model.to_string()),
                kind: GatewayKind::Decode,
                message: "openrouter stream parse error: undecodable delta envelope".to_string(),
            }),
            LlmError::Decode(_) => gateway(GatewayKind::Decode),
            LlmError::Config(_) | LlmError::TomlDecode(_) | LlmError::Io(_) => {
                gateway(GatewayKind::Config)
            }
            // A garble is a content verdict: the call succeeded and was billed.
            // It belongs to the table's coarse marker, not to either column —
            // but this `match` is exhaustive, so it must still return
            // something. `should_record` is what keeps it out of the columns.
            LlmError::Garbled { .. } => gateway(GatewayKind::Decode),
            LlmError::Chain { failures } => failures
                .last()
                .cloned()
                .unwrap_or_else(|| gateway(GatewayKind::ChainExhausted)),
        }
    }

    /// Whether an `LlmError` belongs in either audit column at all.
    ///
    /// One variant does not. A byte-BPE garble is a **content verdict**
    /// (spec §2): the call succeeded and was billed, so the table's coarse
    /// marker owns it and neither column may carry it. `from_llm_error` still
    /// has to classify it — the `match` is exhaustive on purpose — so this is
    /// the guard every chain walk applies before pushing, and the one place
    /// the rule is written down.
    pub fn should_record(e: &LlmError) -> bool {
        !matches!(e, LlmError::Garbled { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openrouter::ParsedErrorBody;
    use crate::LlmError;

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
    fn a_non_error_upstream_status_never_reaches_the_wire() {
        // Codex review: `LlmError::Provider` records `http_status: 200` because
        // that is what the provider actually answered. Forwarding it verbatim
        // would hand the consumer a `200` whose body is an error — every client
        // that branches on `res.ok` would read the failure as a success.
        for status in [100u16, 200, 204, 302, 399] {
            let f = AttemptFailure::Upstream(UpstreamAttempt {
                task: "chat_image_prompt_compose".into(),
                model: "x-ai/grok-4.20".into(),
                http_status: status,
                provider_code: Some("upstream_error".into()),
                error_type: None,
                upstream_provider_code: None,
                retry_after_s: None,
                message: "provider blew up".into(),
            });
            assert_eq!(
                response_status_for(&f),
                502,
                "{status} is not an error status and must not pass through"
            );
        }
        // The bound is a range, not an allow-list: 400 and an unheard-of 599
        // both still pass through untouched.
        for status in [400u16, 418, 429, 529, 599] {
            let f = AttemptFailure::Upstream(UpstreamAttempt {
                task: "chat_companion".into(),
                model: "m".into(),
                http_status: status,
                provider_code: None,
                error_type: None,
                upstream_provider_code: None,
                retry_after_s: None,
                message: "x".into(),
            });
            assert_eq!(response_status_for(&f), status);
        }
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

    #[test]
    fn status_error_becomes_an_upstream_attempt_with_every_field() {
        let body = ParsedErrorBody {
            code: Some("529".into()),
            error_type: Some("overloaded".into()),
            provider_code: Some("anthropic:overloaded_error".into()),
            message: "code=529: Overloaded".into(),
        };
        let e = LlmError::Status(reqwest::StatusCode::from_u16(529).unwrap(), body, Some(30));
        match AttemptFailure::from_llm_error("chat_companion", "x-ai/grok-4.20", &e) {
            AttemptFailure::Upstream(a) => {
                assert_eq!(a.task, "chat_companion");
                assert_eq!(a.model, "x-ai/grok-4.20");
                assert_eq!(a.http_status, 529);
                assert_eq!(a.provider_code.as_deref(), Some("529"));
                assert_eq!(a.error_type.as_deref(), Some("overloaded"));
                assert_eq!(
                    a.upstream_provider_code.as_deref(),
                    Some("anthropic:overloaded_error")
                );
                assert_eq!(a.retry_after_s, Some(30));
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn mid_stream_provider_error_is_upstream_at_http_200() {
        // The provider spoke — just not in the status line. It belongs in
        // llm_attempts, not gateway_errors.
        let e = LlmError::Provider(ParsedErrorBody {
            code: Some("\"upstream_error\"".into()),
            message: "code=\"upstream_error\": provider blew up".into(),
            ..Default::default()
        });
        match AttemptFailure::from_llm_error("chat_companion", "m", &e) {
            AttemptFailure::Upstream(a) => {
                assert_eq!(a.http_status, 200, "mid-stream error rides a 200");
                assert_eq!(a.provider_code.as_deref(), Some("\"upstream_error\""));
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[test]
    fn stream_parse_records_no_provider_payload() {
        // Codex review: the variant carries up to 256 raw bytes of the `data:`
        // frame. That is provider output — reply text, and on an echoing
        // provider the user's own prompt. It may reach a log, never a column
        // and never an HTTP body.
        let secret = "sk-or-v1-deadbeef and the user said something private";
        let e = LlmError::StreamParse(format!("{{\"choices\":[{{\"x\":\"{secret}\"}}]}}"));
        assert!(
            e.to_string().contains(secret),
            "the log line still carries the payload"
        );
        match AttemptFailure::from_llm_error("chat_companion", "m", &e) {
            AttemptFailure::Gateway(g) => {
                assert_eq!(g.kind, GatewayKind::Decode);
                assert!(
                    !g.message.contains(secret),
                    "recorded message leaked the payload: {}",
                    g.message
                );
            }
            other => panic!("expected Gateway, got {other:?}"),
        }
    }

    #[test]
    fn transport_and_decode_failures_are_gateway_not_upstream() {
        let cases: Vec<(LlmError, GatewayKind)> = vec![
            (
                LlmError::Stream("connection reset by peer".into()),
                GatewayKind::Transport,
            ),
            (
                LlmError::StreamParse("not a delta envelope".into()),
                GatewayKind::Decode,
            ),
            (
                LlmError::Config("no models configured".into()),
                GatewayKind::Config,
            ),
        ];
        for (e, want) in cases {
            match AttemptFailure::from_llm_error("chat_companion", "m", &e) {
                AttemptFailure::Gateway(g) => assert_eq!(g.kind, want, "for {e}"),
                other => panic!("expected Gateway for {e}, got {other:?}"),
            }
        }
    }

    #[test]
    fn idle_timeout_is_distinguished_from_a_plain_transport_drop() {
        // Issue #188: folding these together made idle timeouts uncountable.
        let e = LlmError::Stream(format!(
            "{} after 30s",
            crate::openrouter::STREAM_IDLE_TIMEOUT_MSG
        ));
        match AttemptFailure::from_llm_error("chat_companion", "m", &e) {
            AttemptFailure::Gateway(g) => assert_eq!(g.kind, GatewayKind::IdleTimeout),
            other => panic!("expected Gateway, got {other:?}"),
        }
    }

    #[test]
    fn a_garble_is_recordable_by_nobody_but_still_classifies() {
        // Spec §2: the call succeeded and was billed, so it is a content
        // verdict and belongs to the table's coarse marker, not to either
        // column. `from_llm_error` must still answer (the match is
        // exhaustive); `should_record` is what keeps it out.
        let e = LlmError::Garbled {
            model: "m".into(),
            raw: "HiĠthere".into(),
            finish_reason: None,
        };
        assert!(!AttemptFailure::should_record(&e));
        assert!(matches!(
            AttemptFailure::from_llm_error("chat_companion", "m", &e),
            AttemptFailure::Gateway(_)
        ));
    }

    #[test]
    fn every_other_error_is_recordable() {
        let cases = [
            LlmError::Stream("reset".into()),
            LlmError::StreamParse("junk".into()),
            LlmError::Config("no models".into()),
            LlmError::Provider(ParsedErrorBody::default()),
            LlmError::Status(
                reqwest::StatusCode::from_u16(529).unwrap(),
                ParsedErrorBody::default(),
                None,
            ),
            LlmError::Chain { failures: vec![] },
        ];
        for e in cases {
            assert!(AttemptFailure::should_record(&e), "{e} must be recorded");
        }
    }

    #[test]
    fn retry_after_header_parses_delay_seconds_and_ignores_junk() {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("30"));
        assert_eq!(crate::openrouter::retry_after_secs(&h), Some(30));

        // HTTP-date form is legal but we do not resolve it against a clock.
        let mut h = HeaderMap::new();
        h.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(crate::openrouter::retry_after_secs(&h), None);

        assert_eq!(crate::openrouter::retry_after_secs(&HeaderMap::new()), None);
    }
}

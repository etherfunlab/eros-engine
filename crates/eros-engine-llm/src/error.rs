// SPDX-License-Identifier: AGPL-3.0-only
//! Crate-wide error type for the LLM/embedding HTTP clients and TOML config loader.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("http transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("non-success status {0}: {1}")]
    Status(
        reqwest::StatusCode,
        crate::openrouter::ParsedErrorBody,
        /// `Retry-After` in seconds, when the provider sent one. Recorded and
        /// passed downstream; the engine never acts on it.
        Option<u32>,
    ),

    #[error("response decode error: {0}")]
    Decode(#[from] serde_json::Error),

    #[error("toml parse error: {0}")]
    TomlDecode(#[from] toml::de::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("provider error: {0}")]
    Provider(crate::openrouter::ParsedErrorBody),

    /// Wraps a mid-stream parse failure (`data:` line that did not decode as
    /// an OpenRouter-compatible delta envelope). The string is the raw
    /// payload trimmed to a reasonable size for logs.
    #[error("openrouter stream parse error: {0}")]
    StreamParse(String),

    /// Wraps a transport-level interruption while reading the SSE body
    /// (connection reset, TLS error after the response headers arrived).
    #[error("openrouter stream transport error: {0}")]
    Stream(String),

    /// A completion came back as byte-level-BPE garble (issue #84). Carries the
    /// model id, the raw text (so the candidate-walk can repair it as a last
    /// resort once the whole chain is exhausted), and the upstream
    /// `finish_reason` so a safety signal (e.g. `"content_filter"`) survives the
    /// salvage and is not stripped before downstream validity gates see it.
    #[error("openrouter: model {model} returned byte-BPE garbled output")]
    Garbled {
        model: String,
        raw: String,
        finish_reason: Option<String>,
    },

    /// Every candidate in the chain failed. Carries the whole walk.
    ///
    /// Display renders the LAST failure alongside the count: a chain error is
    /// what `tracing::warn!("...{e}")` prints at every call site in the server,
    /// and a bare count would tell an operator nothing about why the turn died.
    #[error(
        "openrouter: all {} candidates failed; last: {}",
        failures.len(),
        failures.last().map(ToString::to_string).unwrap_or_else(|| "(none)".into())
    )]
    Chain {
        failures: Vec<crate::failure::AttemptFailure>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_parse_variant_renders_message() {
        let e = LlmError::StreamParse("bad delta envelope".into());
        assert_eq!(
            e.to_string(),
            "openrouter stream parse error: bad delta envelope"
        );
    }

    #[test]
    fn garbled_variant_renders_message() {
        let e = LlmError::Garbled {
            model: "thedrummer/cydonia-24b-v4.1".into(),
            raw: "HelloĠthere".into(),
            finish_reason: None,
        };
        assert_eq!(
            e.to_string(),
            "openrouter: model thedrummer/cydonia-24b-v4.1 returned byte-BPE garbled output"
        );
    }

    #[test]
    fn chain_error_display_names_the_last_failure_not_just_the_count() {
        // A chain error is what every server-side tracing::warn!("...{e}")
        // prints. A bare count tells an operator nothing about why the turn
        // died.
        use crate::failure::{AttemptFailure, UpstreamAttempt};
        let e = LlmError::Chain {
            failures: vec![
                AttemptFailure::Upstream(UpstreamAttempt {
                    task: "chat_companion".into(),
                    model: "a/m".into(),
                    http_status: 503,
                    provider_code: None,
                    error_type: None,
                    upstream_provider_code: None,
                    retry_after_s: None,
                    message: "code=503: no provider".into(),
                }),
                AttemptFailure::Upstream(UpstreamAttempt {
                    task: "chat_companion".into(),
                    model: "b/m".into(),
                    http_status: 529,
                    provider_code: None,
                    error_type: None,
                    upstream_provider_code: None,
                    retry_after_s: None,
                    message: "code=529: Overloaded".into(),
                }),
            ],
        };
        let s = e.to_string();
        assert!(s.contains('2'), "must carry the attempt count: {s}");
        assert!(
            s.contains("code=529: Overloaded"),
            "must carry the LAST failure, not the first: {s}"
        );
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Crate-wide error type for the LLM/embedding HTTP clients and TOML config loader.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("http transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("non-success status {0}: {1}")]
    Status(reqwest::StatusCode, String),

    #[error("response decode error: {0}")]
    Decode(#[from] serde_json::Error),

    #[error("toml parse error: {0}")]
    TomlDecode(#[from] toml::de::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("provider error: {0}")]
    Provider(String),

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
    /// model id and the raw text so the candidate-walk can repair it as a last
    /// resort once the whole chain is exhausted.
    #[error("openrouter: model {model} returned byte-BPE garbled output")]
    Garbled { model: String, raw: String },
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
        };
        assert_eq!(
            e.to_string(),
            "openrouter: model thedrummer/cydonia-24b-v4.1 returned byte-BPE garbled output"
        );
    }
}

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
}

// SPDX-License-Identifier: AGPL-3.0-only
//! HTTP clients for external LLM + embedding providers, and the TOML task→model config.

pub mod error;
pub mod model_config;
pub mod openrouter;
pub mod voyage;

pub use error::LlmError;

// SPDX-License-Identifier: AGPL-3.0-only
//! HTTP clients for external LLM + embedding providers, and the TOML task→model config.

pub mod byte_bpe;
pub mod error;
pub mod model_config;
pub mod openrouter;
pub mod stream_scrub;
pub mod voyage;

pub use error::LlmError;

// SPDX-License-Identifier: AGPL-3.0-only
//! TOML-driven model orchestration config.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::LlmError;

const FALLBACK_MODEL: &str = "x-ai/grok-4-mini";
const FALLBACK_TEMPERATURE: f64 = 0.5;
const FALLBACK_MAX_TOKENS: u32 = 200;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DefaultConfig {
    #[serde(default)]
    pub fallback_model: Option<String>,
    #[serde(default)]
    pub fallback_temperature: Option<f64>,
    #[serde(default)]
    pub fallback_max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskConfig {
    pub model: String,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub description: String,
    /// Secondary model identifier used if the primary fails.
    #[serde(default)]
    pub fallback: Option<String>,
    /// Embedding-only: vector dimensions.
    #[serde(default)]
    pub dimensions: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelConfig {
    #[serde(default)]
    pub defaults: DefaultConfig,
    #[serde(default)]
    pub tasks: HashMap<String, TaskConfig>,
}

/// Resolved model parameters for an LLM call.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub fallback_model: Option<String>,
    pub temperature: f64,
    pub max_tokens: u32,
}

impl ModelConfig {
    pub fn from_toml_str(text: &str) -> Result<Self, LlmError> {
        Ok(toml::from_str(text)?)
    }

    /// Load the config from `MODEL_CONFIG_PATH` (default `config/model_config.toml`).
    pub fn load() -> Result<Arc<Self>, LlmError> {
        let path = std::env::var("MODEL_CONFIG_PATH")
            .unwrap_or_else(|_| "config/model_config.toml".to_string());
        let text = std::fs::read_to_string(&path)?;
        let cfg = Self::from_toml_str(&text)?;
        Ok(Arc::new(cfg))
    }

    /// Resolve a task's model. Priority: persona_override > task config > defaults.
    pub fn resolve(&self, task: &str, persona_override: Option<&str>) -> ResolvedModel {
        let task_cfg = self.tasks.get(task);
        if task_cfg.is_none() {
            tracing::warn!(task, "model_config: unknown task, using defaults");
        }

        let model = persona_override
            .map(String::from)
            .or_else(|| task_cfg.map(|t| t.model.clone()))
            .or_else(|| self.defaults.fallback_model.clone())
            .unwrap_or_else(|| FALLBACK_MODEL.to_string());

        let fallback_model = task_cfg
            .and_then(|t| t.fallback.clone())
            .or_else(|| self.defaults.fallback_model.clone());

        let temperature = task_cfg
            .and_then(|t| t.temperature)
            .or(self.defaults.fallback_temperature)
            .unwrap_or(FALLBACK_TEMPERATURE);

        let max_tokens = task_cfg
            .and_then(|t| t.max_tokens)
            .or(self.defaults.fallback_max_tokens)
            .unwrap_or(FALLBACK_MAX_TOKENS);

        ResolvedModel {
            model,
            fallback_model,
            temperature,
            max_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[defaults]
fallback_model = "x-ai/grok-4-mini"
fallback_temperature = 0.5
fallback_max_tokens = 200

[tasks.chat_companion]
model = "deepseek/deepseek-v3.2"
temperature = 0.85
max_tokens = 200
description = "AI companion chat"
"#;

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[tasks.chat_companion]
model = "deepseek/chat"
temperature = 0.85
max_tokens = 600
        "#;
        let cfg: ModelConfig = toml::from_str(toml).expect("valid TOML");
        let task = cfg
            .tasks
            .get("chat_companion")
            .expect("chat_companion task present");
        assert_eq!(task.model, "deepseek/chat");
    }

    #[test]
    fn test_parses_full_config() {
        let cfg = ModelConfig::from_toml_str(SAMPLE).expect("parse failed");
        assert_eq!(
            cfg.defaults.fallback_model.as_deref(),
            Some("x-ai/grok-4-mini")
        );
        assert_eq!(cfg.tasks.len(), 1);
    }

    #[test]
    fn test_resolve_uses_task_model() {
        let cfg = ModelConfig::from_toml_str(SAMPLE).unwrap();
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.model, "deepseek/deepseek-v3.2");
        assert_eq!(r.temperature, 0.85);
    }

    #[test]
    fn test_resolve_persona_override_wins() {
        let cfg = ModelConfig::from_toml_str(SAMPLE).unwrap();
        let r = cfg.resolve("chat_companion", Some("x-ai/grok-4-fast"));
        assert_eq!(r.model, "x-ai/grok-4-fast");
        // temp comes from task config, not override
        assert_eq!(r.temperature, 0.85);
    }

    #[test]
    fn test_resolve_unknown_task_uses_defaults() {
        let cfg = ModelConfig::from_toml_str(SAMPLE).unwrap();
        let r = cfg.resolve("nonexistent_task", None);
        assert_eq!(r.model, "x-ai/grok-4-mini");
        assert_eq!(r.temperature, 0.5);
        assert_eq!(r.max_tokens, 200);
    }

    #[test]
    fn test_resolve_override_with_unknown_task_uses_defaults_for_params() {
        let cfg = ModelConfig::from_toml_str(SAMPLE).unwrap();
        let r = cfg.resolve("nonexistent_task", Some("x-ai/grok-4-fast"));
        assert_eq!(r.model, "x-ai/grok-4-fast"); // override wins
        assert_eq!(r.temperature, 0.5); // defaults
        assert_eq!(r.max_tokens, 200); // defaults
    }
}

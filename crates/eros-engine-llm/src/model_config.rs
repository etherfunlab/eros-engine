// SPDX-License-Identifier: AGPL-3.0-only
//! TOML-driven model orchestration config.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::error::LlmError;

const FALLBACK_MODEL: &str = "x-ai/grok-4-mini";
const FALLBACK_TEMPERATURE: f64 = 0.5;
const FALLBACK_MAX_TOKENS: u32 = 200;

/// Per-task fallback shape — accepts either a single model id (legacy)
/// or an ordered array. Normalised to `Vec<String>` via `into_vec()`
/// in the resolver; empty entries are filtered out.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum FallbackSpec {
    Single(String),
    Multiple(Vec<String>),
}

impl FallbackSpec {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            FallbackSpec::Single(s) if s.is_empty() => Vec::new(),
            FallbackSpec::Single(s) => vec![s],
            FallbackSpec::Multiple(v) => v.into_iter().filter(|s| !s.is_empty()).collect(),
        }
    }
}

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
    /// Secondary model(s) tried in order on primary failure. Accepts a
    /// single string (legacy) or an array. Empty (`""` or `[]`) is an
    /// explicit opt-out and suppresses `defaults.fallback_model`.
    #[serde(default)]
    pub fallback: Option<FallbackSpec>,
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
///
/// `fallback_model` is intentionally singular-named even though it's a
/// `Vec<String>`: semantically only ONE fallback is ever used per call
/// (the chain is tried sequentially, first success wins). Plural naming
/// would mislead readers into thinking the candidates run in parallel.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
}

impl ModelConfig {
    pub fn from_toml_str(text: &str) -> Result<Self, LlmError> {
        Ok(toml::from_str(text)?)
    }

    /// Library-side convenience: load the config from `MODEL_CONFIG_PATH`,
    /// or fall back to `examples/model_config.toml` to match the
    /// `eros-engine-server` boot default. The server binary itself reads
    /// the file inline via `from_toml_str` rather than calling this; this
    /// method is provided for embedders who want the same behaviour in
    /// one call.
    pub fn load() -> Result<Arc<Self>, LlmError> {
        let path = std::env::var("MODEL_CONFIG_PATH")
            .unwrap_or_else(|_| "examples/model_config.toml".to_string());
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

        // Precedence: explicit per-task `fallback` (even when empty)
        // wins over defaults. Only an absent per-task value inherits
        // the singleton from `[defaults] fallback_model`.
        let fallback_model: Vec<String> = match task_cfg.and_then(|t| t.fallback.as_ref()) {
            Some(spec) => spec.clone().into_vec(),
            None => self.defaults.fallback_model.iter().cloned().collect(),
        };

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
        assert_eq!(
            r.fallback_model,
            vec!["x-ai/grok-4-mini".to_string()],
            "unknown task must inherit defaults.fallback_model as a singleton"
        );
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

    // ─── Public schema compat fixture ─────────────────────────────────
    //
    // This test locks the full set of fields and task names that the
    // OSS engine commits to supporting in 0.x — see
    // `docs/model-config.md` § "Stability commitments".
    //
    // Adding optional fields / new task names is fine. Renaming or
    // removing a field, or making an existing field required, will
    // break this test.

    const COMPAT_FIXTURE: &str = r#"
[defaults]
fallback_model       = "x-ai/grok-4-mini"
fallback_temperature = 0.5
fallback_max_tokens  = 200

[tasks.chat_companion]
model        = "x-ai/grok-4-fast"
fallback     = "deepseek/deepseek-chat-v3.2"
temperature  = 0.85
max_tokens   = 600
description  = "AI companion chat"

[tasks.insight_extraction]
model        = "x-ai/grok-4-mini"
fallback     = "deepseek/deepseek-chat-v3.2"
temperature  = 0.3
max_tokens   = 400
description  = "extract user facts from a chat turn"

[tasks.pde_decision]
model        = "x-ai/grok-4-mini"
temperature  = 0.5
max_tokens   = 200
description  = "reserved — current PDE is rule-based"

[tasks.embedding]
model        = "voyage-3-lite"
dimensions   = 512
description  = "reserved — Voyage hard-codes its own model"
"#;

    #[test]
    fn compat_fixture_locks_full_schema() {
        let cfg = ModelConfig::from_toml_str(COMPAT_FIXTURE).expect("fixture must parse");

        // [defaults] — all fields preserved.
        assert_eq!(
            cfg.defaults.fallback_model.as_deref(),
            Some("x-ai/grok-4-mini")
        );
        assert_eq!(cfg.defaults.fallback_temperature, Some(0.5));
        assert_eq!(cfg.defaults.fallback_max_tokens, Some(200));

        // All four committed task names are present.
        for name in [
            "chat_companion",
            "insight_extraction",
            "pde_decision",
            "embedding",
        ] {
            assert!(
                cfg.tasks.contains_key(name),
                "compat fixture missing task `{name}`"
            );
        }

        // chat_companion — every field round-trips.
        let chat = cfg.tasks.get("chat_companion").unwrap();
        assert_eq!(chat.model, "x-ai/grok-4-fast");
        assert_eq!(
            chat.fallback.clone().expect("fallback present").into_vec(),
            vec!["deepseek/deepseek-chat-v3.2".to_string()]
        );
        assert_eq!(chat.temperature, Some(0.85));
        assert_eq!(chat.max_tokens, Some(600));
        assert_eq!(chat.description, "AI companion chat");

        // insight_extraction — same shape.
        let insight = cfg.tasks.get("insight_extraction").unwrap();
        assert_eq!(insight.model, "x-ai/grok-4-mini");
        assert_eq!(
            insight
                .fallback
                .clone()
                .expect("fallback present")
                .into_vec(),
            vec!["deepseek/deepseek-chat-v3.2".to_string()]
        );
        assert_eq!(insight.temperature, Some(0.3));
        assert_eq!(insight.max_tokens, Some(400));

        // pde_decision — reserved, partial fields.
        let pde = cfg.tasks.get("pde_decision").unwrap();
        assert_eq!(pde.model, "x-ai/grok-4-mini");
        assert!(pde.fallback.is_none());
        assert_eq!(pde.temperature, Some(0.5));

        // embedding — reserved, with `dimensions` set.
        let emb = cfg.tasks.get("embedding").unwrap();
        assert_eq!(emb.model, "voyage-3-lite");
        assert_eq!(emb.dimensions, Some(512));

        // Resolution behaviour on the live tasks.
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.model, "x-ai/grok-4-fast");
        assert_eq!(
            r.fallback_model,
            vec!["deepseek/deepseek-chat-v3.2".to_string()]
        );
        assert_eq!(r.temperature, 0.85);
        assert_eq!(r.max_tokens, 600);

        // Persona override only touches `model`; temperature / max_tokens
        // stay on the task config.
        let r = cfg.resolve("chat_companion", Some("anthropic/claude-sonnet-4"));
        assert_eq!(r.model, "anthropic/claude-sonnet-4");
        assert_eq!(r.temperature, 0.85);
        assert_eq!(r.max_tokens, 600);
    }

    #[test]
    fn fallback_spec_deserializes_from_string() {
        let toml = r#"
[tasks.chat_companion]
model = "x"
fallback = "y"
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse ok");
        let t = cfg.tasks.get("chat_companion").unwrap();
        let v = t.fallback.clone().expect("fallback present").into_vec();
        assert_eq!(v, vec!["y".to_string()]);
    }

    #[test]
    fn fallback_spec_deserializes_from_array() {
        let toml = r#"
[tasks.chat_companion]
model = "x"
fallback = ["a", "b"]
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse ok");
        let t = cfg.tasks.get("chat_companion").unwrap();
        let v = t.fallback.clone().expect("fallback present").into_vec();
        assert_eq!(v, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn fallback_spec_skips_empty_entries() {
        let toml = r#"
[tasks.chat_companion]
model = "x"
fallback = ["", "a", ""]
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse ok");
        let t = cfg.tasks.get("chat_companion").unwrap();
        let v = t.fallback.clone().expect("fallback present").into_vec();
        assert_eq!(v, vec!["a".to_string()]);
    }

    #[test]
    fn fallback_spec_empty_string_collapses_to_empty_vec() {
        let toml = r#"
[tasks.chat_companion]
model = "x"
fallback = ""
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse ok");
        let t = cfg.tasks.get("chat_companion").unwrap();
        let v = t.fallback.clone().expect("fallback present").into_vec();
        assert!(v.is_empty());
    }

    #[test]
    fn resolve_returns_empty_fallback_when_no_task_fallback_no_defaults() {
        let toml = r#"
[tasks.chat_companion]
model = "x"
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse ok");
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.model, "x");
        assert!(r.fallback_model.is_empty());
    }

    #[test]
    fn resolve_returns_defaults_fallback_when_task_has_none() {
        let toml = r#"
[defaults]
fallback_model = "default-fb"

[tasks.chat_companion]
model = "x"
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse ok");
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.fallback_model, vec!["default-fb".to_string()]);
    }

    #[test]
    fn resolve_task_array_overrides_defaults() {
        let toml = r#"
[defaults]
fallback_model = "default-fb"

[tasks.chat_companion]
model = "x"
fallback = ["a", "b"]
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse ok");
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.fallback_model, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn resolve_empty_array_suppresses_defaults() {
        let toml = r#"
[defaults]
fallback_model = "default-fb"

[tasks.chat_companion]
model = "x"
fallback = []
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse ok");
        let r = cfg.resolve("chat_companion", None);
        assert!(
            r.fallback_model.is_empty(),
            "explicit empty array must suppress defaults; got {:?}",
            r.fallback_model
        );
    }

    #[test]
    fn resolve_empty_string_suppresses_defaults() {
        let toml = r#"
[defaults]
fallback_model = "default-fb"

[tasks.chat_companion]
model = "x"
fallback = ""
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse ok");
        let r = cfg.resolve("chat_companion", None);
        assert!(
            r.fallback_model.is_empty(),
            "explicit empty string must suppress defaults; got {:?}",
            r.fallback_model
        );
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! TOML-driven model orchestration config.

use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// A task/tier's primary `model`. Accepts three TOML shapes:
/// `"id"` (fixed), `["a","b"]` (round-robin), or `{ "a" = 0.8, "b" = 0.2 }`
/// (weighted random, any positive weights, normalized by sum).
#[derive(Debug, Clone)]
pub enum ModelSpec {
    Fixed(String),
    RoundRobin {
        models: Vec<String>,
        cursor: Arc<AtomicUsize>,
    },
    Weighted(Vec<(String, f64)>),
}

impl<'de> Deserialize<'de> for ModelSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Untagged intermediate: TOML string vs array vs inline table are
        // unambiguous to serde (same pattern as `FallbackSpec`).
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Fixed(String),
            RoundRobin(Vec<String>),
            Weighted(HashMap<String, f64>),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Fixed(s) => ModelSpec::Fixed(s),
            Raw::RoundRobin(models) => ModelSpec::RoundRobin {
                models: models.into_iter().filter(|s| !s.is_empty()).collect(),
                cursor: Arc::new(AtomicUsize::new(0)),
            },
            // Drop non-positive weights at parse time; normalization is by sum
            // at selection. Sort by id so the cumulative-band order is
            // deterministic across restarts (HashMap iteration order is not).
            Raw::Weighted(map) => {
                let mut entries: Vec<(String, f64)> =
                    map.into_iter().filter(|(_, w)| *w > 0.0).collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                ModelSpec::Weighted(entries)
            }
        })
    }
}

impl ModelSpec {
    /// Pick one concrete model id. `None` means the spec is empty (empty array,
    /// empty/all-non-positive table, or empty fixed string) — the caller should
    /// fall through to the next precedence level.
    fn select(&self) -> Option<String> {
        match self {
            ModelSpec::Fixed(s) if !s.is_empty() => Some(s.clone()),
            ModelSpec::RoundRobin { models, cursor } if !models.is_empty() => {
                let i = cursor.fetch_add(1, Ordering::Relaxed) % models.len();
                Some(models[i].clone())
            }
            ModelSpec::Weighted(entries) if !entries.is_empty() => {
                let sum: f64 = entries.iter().map(|(_, w)| w).sum();
                let position = rand::thread_rng().gen_range(0.0..sum);
                Some(pick_weighted(entries, position).to_string())
            }
            _ => None,
        }
    }
}

/// Pure cumulative-weight walk: given `position` in `[0, sum)`, return the id
/// whose cumulative band contains it. Split out so the random draw stays in
/// `select()` and the band logic is unit-testable. Caller guarantees non-empty.
fn pick_weighted(entries: &[(String, f64)], position: f64) -> &str {
    let mut acc = 0.0;
    for (model, w) in entries {
        acc += w;
        if position < acc {
            return model;
        }
    }
    // Reachable when position >= acc: gen_range uses Iterator::sum() while the
    // loop accumulates with sequential +=, and the two can round differently,
    // so the last entry absorbs the rounding remainder.
    &entries.last().expect("caller ensures non-empty").0
}

#[cfg(test)]
impl ModelSpec {
    fn as_fixed(&self) -> Option<&str> {
        match self {
            ModelSpec::Fixed(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// Mirror of OpenRouter's `reasoning` request object. Parsed from TOML and
/// forwarded to the wire unchanged, so operators control reasoning in the
/// same shape OpenRouter documents. Every field optional; absent fields are
/// omitted from the wire. Common uses: `{ enabled = false }` to disable
/// reasoning entirely, or `{ exclude = true }` to keep reasoning but drop it
/// from the response. (Extend with `effort`/`max_tokens` here if ever needed.)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReasoningConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<bool>,
}

/// One tier's overrides for a task. Every field is optional; an absent
/// field inherits from the enclosing task's default block in `resolve`.
#[derive(Debug, Clone, Deserialize)]
pub struct TierConfig {
    #[serde(default)]
    pub model: Option<ModelSpec>,
    #[serde(default)]
    pub fallback: Option<FallbackSpec>,
    /// Allow-listed prompt-trait tags. Three-state, mirroring `fallback`'s
    /// absent≠empty rule: absent → None (no gating); `[]` → empty whitelist
    /// (drop all); `["a","b"]` → keep only those tags.
    #[serde(default)]
    pub allow_traits: Option<Vec<String>>,
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
    pub model: ModelSpec,
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
    /// Task-level (default-block) prompt-trait allow-list. Same three-state
    /// semantics as `TierConfig::allow_traits`.
    #[serde(default)]
    pub allow_traits: Option<Vec<String>>,
    /// Task-level reasoning config (OpenRouter `reasoning` object). Absent →
    /// omit the param (model default); present → forwarded to the wire (e.g.
    /// `reasoning = { enabled = false }` to disable). Task-level only — tiers
    /// inherit, like `temperature`/`max_tokens`.
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    /// Per-tier overrides keyed by tier name. Empty for tasks that don't tier.
    #[serde(default)]
    pub tiers: HashMap<String, TierConfig>,
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
    /// Resolved trait allow-list. `None` → no gating; `Some(set)` → the chat
    /// handler keeps only `prompt_traits` whose tag is in `set`.
    pub allow_traits: Option<Vec<String>>,
    /// Resolved reasoning config (see `TaskConfig::reasoning`). `None` → omit
    /// the wire param; `Some(cfg)` → forwarded as the `reasoning` object.
    pub reasoning: Option<ReasoningConfig>,
}

impl ModelConfig {
    pub fn from_toml_str(text: &str) -> Result<Self, LlmError> {
        Ok(toml::from_str(text)?)
    }

    /// Library-side convenience: load the config from `MODEL_CONFIG_PATH`,
    /// or fall back to `examples/model_config.toml.example` to match the
    /// `eros-engine-server` boot default. The server binary itself reads
    /// the file inline via `from_toml_str` rather than calling this; this
    /// method is provided for embedders who want the same behaviour in
    /// one call.
    pub fn load() -> Result<Arc<Self>, LlmError> {
        let path = std::env::var("MODEL_CONFIG_PATH")
            .unwrap_or_else(|_| "examples/model_config.toml.example".to_string());
        let text = std::fs::read_to_string(&path)?;
        let cfg = Self::from_toml_str(&text)?;
        Ok(Arc::new(cfg))
    }

    /// Resolve a task's model. Priority for `model`/`fallback`/`allow_traits`:
    /// matched tier block > task default block > `[defaults]` > compiled-in.
    /// `temperature`/`max_tokens`/`reasoning` are task-level only (no per-tier
    /// override).
    pub fn resolve(&self, task: &str, tier: Option<&str>) -> ResolvedModel {
        let task_cfg = self.tasks.get(task);
        if task_cfg.is_none() {
            tracing::warn!(task, "model_config: unknown task, using defaults");
        }

        // Matched tier block, if a tier was supplied and exists on this task.
        let tier_cfg = match (task_cfg, tier) {
            (Some(t), Some(name)) => {
                let found = t.tiers.get(name);
                if found.is_none() {
                    tracing::warn!(
                        task,
                        tier = name,
                        "model_config: unknown tier, using task default block"
                    );
                }
                found
            }
            _ => None,
        };

        // Primary model: pick the winning spec by precedence
        // (tier > task default > defaults.fallback_model > compiled-in), then
        // select() a concrete id from it. An empty spec (e.g. `model = []`)
        // yields None and falls through, warning as it goes.
        let select_with_warn = |spec: Option<&ModelSpec>, level: &str| -> Option<String> {
            let picked = spec.and_then(ModelSpec::select);
            if spec.is_some() && picked.is_none() {
                tracing::warn!(task, level, "model_config: empty model spec, falling through");
            }
            picked
        };
        let model = select_with_warn(tier_cfg.and_then(|t| t.model.as_ref()), "tier")
            .or_else(|| select_with_warn(task_cfg.map(|t| &t.model), "task"))
            .or_else(|| self.defaults.fallback_model.clone())
            .unwrap_or_else(|| FALLBACK_MODEL.to_string());

        // fallback: tier (even empty) > task (even empty) > defaults singleton.
        let mut fallback_model: Vec<String> = match tier_cfg.and_then(|t| t.fallback.as_ref()) {
            Some(spec) => spec.clone().into_vec(),
            None => match task_cfg.and_then(|t| t.fallback.as_ref()) {
                Some(spec) => spec.clone().into_vec(),
                None => self.defaults.fallback_model.iter().cloned().collect(),
            },
        };
        // A just-failed primary in its own fallback chain is a wasted retry.
        fallback_model.retain(|m| m != &model);

        // allow_traits: tier (even empty) > task > None.
        let allow_traits = tier_cfg
            .and_then(|t| t.allow_traits.clone())
            .or_else(|| task_cfg.and_then(|t| t.allow_traits.clone()));

        let temperature = task_cfg
            .and_then(|t| t.temperature)
            .or(self.defaults.fallback_temperature)
            .unwrap_or(FALLBACK_TEMPERATURE);

        let max_tokens = task_cfg
            .and_then(|t| t.max_tokens)
            .or(self.defaults.fallback_max_tokens)
            .unwrap_or(FALLBACK_MAX_TOKENS);

        // Task-level only (tiers inherit), mirroring temperature/max_tokens.
        let reasoning = task_cfg.and_then(|t| t.reasoning.clone());

        ResolvedModel {
            model,
            fallback_model,
            temperature,
            max_tokens,
            allow_traits,
            reasoning,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_weighted_boundaries_and_normalization() {
        let raw = vec![("a".to_string(), 8.0), ("b".to_string(), 2.0)];
        assert_eq!(pick_weighted(&raw, 0.0), "a");
        assert_eq!(pick_weighted(&raw, 7.999), "a");
        assert_eq!(pick_weighted(&raw, 8.0), "b");
        assert_eq!(pick_weighted(&raw, 9.999), "b");

        let norm = vec![("a".to_string(), 0.8), ("b".to_string(), 0.2)];
        assert_eq!(pick_weighted(&norm, 0.79), "a");
        assert_eq!(pick_weighted(&norm, 0.80), "b");
    }

    #[test]
    fn model_spec_parses_three_forms() {
        let toml = r#"
[tasks.fixed]
model = "a"
[tasks.rr]
model = ["a", "b"]
[tasks.weighted]
model = { "a" = 0.8, "b" = 0.2 }
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert!(matches!(cfg.tasks["fixed"].model, ModelSpec::Fixed(_)));
        assert!(matches!(cfg.tasks["rr"].model, ModelSpec::RoundRobin { .. }));
        assert!(matches!(cfg.tasks["weighted"].model, ModelSpec::Weighted(_)));
    }

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

    const TIERED: &str = r#"
[tasks.chat_companion]
model        = "default-model"
fallback     = ["default-fb"]
allow_traits = ["allow_politics"]
temperature  = 0.8
max_tokens   = 1200

[tasks.chat_companion.tiers.free]
model        = "free-model"
fallback     = ["free-fb"]
allow_traits = ["allow_politics"]

[tasks.chat_companion.tiers.gold]
model        = "gold-model"
fallback     = ["gold-fb-1", "gold-fb-2"]
allow_traits = ["allow_nsfw", "allow_politics"]
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
        assert_eq!(task.model.as_fixed(), Some("deepseek/chat"));
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
    fn test_resolve_unknown_task_uses_defaults() {
        let cfg = ModelConfig::from_toml_str(SAMPLE).unwrap();
        let r = cfg.resolve("nonexistent_task", None);
        assert_eq!(r.model, "x-ai/grok-4-mini");
        // defaults.fallback_model is the same id as the selected primary, so
        // after primary-dedup it is removed from the chain (retrying the same
        // model that just failed is wasteful).
        assert!(
            r.fallback_model.is_empty(),
            "primary dedup must remove the defaults fallback when it equals the primary; got {:?}",
            r.fallback_model
        );
        assert_eq!(r.temperature, 0.5);
        assert_eq!(r.max_tokens, 200);
    }

    #[test]
    fn resolve_tier_match_uses_tier_block() {
        let cfg = ModelConfig::from_toml_str(TIERED).unwrap();
        let r = cfg.resolve("chat_companion", Some("gold"));
        assert_eq!(r.model, "gold-model");
        assert_eq!(
            r.fallback_model,
            vec!["gold-fb-1".to_string(), "gold-fb-2".to_string()]
        );
        assert_eq!(
            r.allow_traits,
            Some(vec!["allow_nsfw".to_string(), "allow_politics".to_string()])
        );
        assert_eq!(r.temperature, 0.8);
        assert_eq!(r.max_tokens, 1200);
    }

    #[test]
    fn resolve_unknown_tier_falls_back_to_default_block() {
        let cfg = ModelConfig::from_toml_str(TIERED).unwrap();
        let r = cfg.resolve("chat_companion", Some("platinum"));
        assert_eq!(r.model, "default-model");
        assert_eq!(r.fallback_model, vec!["default-fb".to_string()]);
        assert_eq!(r.allow_traits, Some(vec!["allow_politics".to_string()]));
    }

    #[test]
    fn resolve_no_tier_uses_default_block() {
        let cfg = ModelConfig::from_toml_str(TIERED).unwrap();
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.model, "default-model");
        assert_eq!(r.allow_traits, Some(vec!["allow_politics".to_string()]));
    }

    #[test]
    fn resolve_tier_inherits_unspecified_fields_from_default_block() {
        let toml = r#"
[tasks.chat_companion]
model        = "default-model"
fallback     = ["default-fb"]
allow_traits = ["allow_politics"]

[tasks.chat_companion.tiers.free]
model = "free-model"
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let r = cfg.resolve("chat_companion", Some("free"));
        assert_eq!(r.model, "free-model");
        assert_eq!(r.fallback_model, vec!["default-fb".to_string()]);
        assert_eq!(r.allow_traits, Some(vec!["allow_politics".to_string()]));
    }

    #[test]
    fn resolve_tier_empty_fallback_suppresses_task_fallback() {
        // A tier `fallback = []` must suppress the task default block's
        // fallback (mirrors the task-vs-defaults suppression rule), not
        // inherit it.
        let toml = r#"
[tasks.chat_companion]
model    = "default-model"
fallback = ["default-fb"]

[tasks.chat_companion.tiers.bare]
fallback = []
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let r = cfg.resolve("chat_companion", Some("bare"));
        assert_eq!(r.model, "default-model"); // inherited (tier sets no model)
        assert!(
            r.fallback_model.is_empty(),
            "tier `fallback = []` must suppress task fallback; got {:?}",
            r.fallback_model
        );
    }

    #[test]
    fn resolve_allow_traits_three_state() {
        let absent = r#"
[tasks.chat_companion]
model = "m"
"#;
        let r = ModelConfig::from_toml_str(absent)
            .unwrap()
            .resolve("chat_companion", None);
        assert_eq!(r.allow_traits, None);

        let empty = r#"
[tasks.chat_companion]
model = "m"
allow_traits = ["allow_politics"]

[tasks.chat_companion.tiers.locked]
allow_traits = []
"#;
        let r = ModelConfig::from_toml_str(empty)
            .unwrap()
            .resolve("chat_companion", Some("locked"));
        assert_eq!(r.allow_traits, Some(vec![]));

        let list = r#"
[tasks.chat_companion]
model = "m"
allow_traits = ["a", "b"]
"#;
        let r = ModelConfig::from_toml_str(list)
            .unwrap()
            .resolve("chat_companion", None);
        assert_eq!(r.allow_traits, Some(vec!["a".to_string(), "b".to_string()]));
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
allow_traits = ["allow_politics"]

[tasks.chat_companion.tiers.gold]
model        = "x-ai/grok-4.20"
fallback     = ["a", "b"]
allow_traits = ["allow_nsfw", "allow_politics"]

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
        assert_eq!(chat.model.as_fixed(), Some("x-ai/grok-4-fast"));
        assert_eq!(
            chat.fallback.clone().expect("fallback present").into_vec(),
            vec!["deepseek/deepseek-chat-v3.2".to_string()]
        );
        assert_eq!(chat.temperature, Some(0.85));
        assert_eq!(chat.max_tokens, Some(600));
        assert_eq!(chat.description, "AI companion chat");
        // New optional fields round-trip (schema lock for `allow_traits` + `tiers`).
        assert_eq!(chat.allow_traits, Some(vec!["allow_politics".to_string()]));
        let gold = chat.tiers.get("gold").expect("gold tier present");
        assert_eq!(gold.model.as_ref().and_then(ModelSpec::as_fixed), Some("x-ai/grok-4.20"));
        assert_eq!(
            gold.fallback
                .clone()
                .expect("tier fallback present")
                .into_vec(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            gold.allow_traits,
            Some(vec!["allow_nsfw".to_string(), "allow_politics".to_string()])
        );

        // insight_extraction — same shape.
        let insight = cfg.tasks.get("insight_extraction").unwrap();
        assert_eq!(insight.model.as_fixed(), Some("x-ai/grok-4-mini"));
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
        assert_eq!(pde.model.as_fixed(), Some("x-ai/grok-4-mini"));
        assert!(pde.fallback.is_none());
        assert_eq!(pde.temperature, Some(0.5));

        // embedding — reserved, with `dimensions` set.
        let emb = cfg.tasks.get("embedding").unwrap();
        assert_eq!(emb.model.as_fixed(), Some("voyage-3-lite"));
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

        // A tier name that isn't configured falls back to the task default
        // block; temperature / max_tokens are always task-level.
        let r = cfg.resolve("chat_companion", Some("nonexistent_tier"));
        assert_eq!(r.model, "x-ai/grok-4-fast");
        assert_eq!(r.temperature, 0.85);
        assert_eq!(r.max_tokens, 600);

        // A configured tier resolves to its own block.
        let r = cfg.resolve("chat_companion", Some("gold"));
        assert_eq!(r.model, "x-ai/grok-4.20");
        assert_eq!(r.fallback_model, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            r.allow_traits,
            Some(vec!["allow_nsfw".to_string(), "allow_politics".to_string()])
        );
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

    #[test]
    fn resolve_reads_task_level_reasoning_and_tiers_inherit() {
        let toml = r#"
[tasks.chat_companion]
model = "m"
reasoning = { enabled = false }

[tasks.chat_companion.tiers.free]
model = "free-m"
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let expected = ReasoningConfig {
            enabled: Some(false),
            exclude: None,
        };
        // Task-level value applies with no tier...
        assert_eq!(
            cfg.resolve("chat_companion", None).reasoning,
            Some(expected.clone())
        );
        // ...and a tier that doesn't override it inherits the task value.
        assert_eq!(
            cfg.resolve("chat_companion", Some("free")).reasoning,
            Some(expected)
        );
    }

    #[test]
    fn resolve_reasoning_absent_is_none() {
        let toml = r#"
[tasks.chat_companion]
model = "m"
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.resolve("chat_companion", None).reasoning, None);
    }

    #[test]
    fn resolve_reasoning_parses_exclude_field() {
        let toml = r#"
[tasks.chat_companion]
model = "m"
reasoning = { exclude = true }
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert_eq!(
            cfg.resolve("chat_companion", None).reasoning,
            Some(ReasoningConfig {
                enabled: None,
                exclude: Some(true),
            })
        );
    }

    // Regression: the committed deployed config (examples/model_config.toml,
    // copied to /etc/eros-engine in the Docker image) must always parse and
    // must define the affinity_evaluation task the post-process evaluator
    // depends on — otherwise resolve() silently falls back to the wrong model.
    #[test]
    fn committed_example_config_parses_and_has_affinity_task() {
        let text = include_str!("../../../examples/model_config.toml.example");
        let cfg = ModelConfig::from_toml_str(text).expect("examples/model_config.toml.example must parse");
        let r = cfg.resolve("affinity_evaluation", None);
        assert_eq!(r.model, "anthropic/claude-haiku-4.5");
        assert_eq!(r.max_tokens, 400);
        assert!((r.temperature - 0.3).abs() < 1e-9);
        assert_eq!(
            r.fallback_model,
            vec![
                "deepseek/deepseek-v4-flash".to_string(),
                "google/gemini-3.1-flash-lite".to_string(),
            ]
        );
    }

    #[test]
    fn committed_example_chat_companion_disables_reasoning() {
        let text = include_str!("../../../examples/model_config.toml.example");
        let cfg = ModelConfig::from_toml_str(text).expect("examples/model_config.toml.example must parse");
        let disabled = ReasoningConfig {
            enabled: Some(false),
            exclude: None,
        };
        // Disabled for the default block...
        assert_eq!(
            cfg.resolve("chat_companion", None).reasoning,
            Some(disabled.clone())
        );
        // ...and inherited by the free tier (no per-tier override).
        assert_eq!(
            cfg.resolve("chat_companion", Some("free")).reasoning,
            Some(disabled)
        );
        // Untouched tasks stay at model default.
        assert_eq!(cfg.resolve("insight_extraction", None).reasoning, None);
    }

    #[test]
    fn fallback_drops_selected_primary() {
        let toml = r#"
[tasks.t]
model = "a"
fallback = ["a", "c"]
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let r = cfg.resolve("t", None);
        assert_eq!(r.model, "a");
        assert_eq!(r.fallback_model, vec!["c".to_string()]);
    }

    #[test]
    fn fallback_dedup_is_dynamic_under_round_robin() {
        let toml = r#"
[tasks.t]
model = ["a", "b"]
fallback = ["a", "c"]
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        // turn 1 selects "a" -> "a" dropped from fallback
        let r1 = cfg.resolve("t", None);
        assert_eq!(r1.model, "a");
        assert_eq!(r1.fallback_model, vec!["c".to_string()]);
        // turn 2 selects "b" -> "a" stays
        let r2 = cfg.resolve("t", None);
        assert_eq!(r2.model, "b");
        assert_eq!(r2.fallback_model, vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn round_robin_alternates() {
        let toml = r#"
[tasks.t]
model = ["a", "b"]
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.resolve("t", None).model, "a");
        assert_eq!(cfg.resolve("t", None).model, "b");
        assert_eq!(cfg.resolve("t", None).model, "a");
        assert_eq!(cfg.resolve("t", None).model, "b");
    }

    #[test]
    fn round_robin_task_and_tier_counters_independent() {
        let toml = r#"
[tasks.t]
model = ["a", "b"]

[tasks.t.tiers.free]
model = ["c", "d"]
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.resolve("t", None).model, "a");
        assert_eq!(cfg.resolve("t", Some("free")).model, "c");
        assert_eq!(cfg.resolve("t", None).model, "b");
        assert_eq!(cfg.resolve("t", Some("free")).model, "d");
    }

    #[test]
    fn single_entry_array_behaves_like_fixed() {
        let toml = r#"
[tasks.t]
model = ["only"]
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.resolve("t", None).model, "only");
        assert_eq!(cfg.resolve("t", None).model, "only");
    }

    #[test]
    fn empty_model_array_falls_through_to_defaults() {
        let toml = r#"
[defaults]
fallback_model = "fb"
[tasks.t]
model = []
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.resolve("t", None).model, "fb");
    }
}

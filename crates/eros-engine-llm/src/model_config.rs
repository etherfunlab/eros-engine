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
            // Drop non-finite and non-positive weights at parse time. `inf` is
            // a valid TOML float and passes `> 0.0`, but would make the sum
            // non-finite and panic `gen_range(0.0..sum)` at selection; require
            // finite so a bad config falls through instead of crashing.
            // Normalization is by sum at selection. Sort by id so the
            // cumulative-band order is deterministic across restarts
            // (HashMap iteration order is not).
            Raw::Weighted(map) => {
                let mut entries: Vec<(String, f64)> = map
                    .into_iter()
                    .filter(|(_, w)| w.is_finite() && *w > 0.0)
                    .collect();
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

/// Client-facing model-name display override (chat `meta.model`). Four TOML
/// shapes, unambiguous to serde: `false`/`true` (bool), `"name"` (string),
/// `["a","b"]` (array → random per emit), or `{ "id" = "name", default =
/// "name" }` (map keyed by the real id; reserved `default` key). Affects ONLY
/// what the client sees — never the OpenRouter call or the persisted row.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum DisplayOverride {
    Bool(bool),
    Fixed(String),
    Random(Vec<String>),
    Map(HashMap<String, String>),
}

impl DisplayOverride {
    /// Map the real model id to the value shown to the client. `None` means
    /// "omit the `model` field". `false`, an empty string, an empty array, and
    /// a map miss with no `default` all yield `None`.
    pub fn display(&self, actual_model: &str) -> Option<String> {
        match self {
            DisplayOverride::Bool(false) => None,
            DisplayOverride::Bool(true) => Some(actual_model.to_string()),
            DisplayOverride::Fixed(s) if s.is_empty() => None,
            DisplayOverride::Fixed(s) => Some(s.clone()),
            DisplayOverride::Random(v) if v.is_empty() => None,
            DisplayOverride::Random(v) => {
                let i = rand::thread_rng().gen_range(0..v.len());
                Some(v[i].clone())
            }
            DisplayOverride::Map(m) => m.get(actual_model).or_else(|| m.get("default")).cloned(),
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

/// Per-turn filter trigger. Every field optional; the AND of all *specified*
/// predicates decides whether a turn is filtered. None specified ⇒ filter every
/// turn. `random` is the probability (0.0–1.0) that a turn passes the random
/// gate (1.0 ≈ always, 0.0 = never); combined via AND with the other predicates.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutputFilterTrigger {
    #[serde(default)]
    pub random: Option<f64>,
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub traits: Option<TraitPredicate>,
}

/// Which predicates fired this turn, echoing the **source config verbatim**
/// (config-as-declared). Serialises to JSONB for
/// `chat_messages.filter_triggers`; absent fields skip serialization so only
/// configured-and-fired predicates appear. An all-`None` value (empty trigger
/// that always fires) serialises to `{}` and `is_empty()` is true — the
/// stream layer maps that to SQL `NULL`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct FiredPredicates {
    /// The configured probability `p` (NOT the per-turn draw).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub random: Option<f64>,
    /// The configured model allowlist (NOT just the matched id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    /// The configured trait predicate `{ any, when }` (NOT observed tags).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traits: Option<TraitPredicate>,
}

impl FiredPredicates {
    /// True when no predicate was configured (empty/always-fire trigger).
    pub fn is_empty(&self) -> bool {
        self.random.is_none() && self.models.is_none() && self.traits.is_none()
    }
}

impl OutputFilterTrigger {
    /// Turn-constant predicates (random + traits). When either is specified and
    /// fails, no attempt can be filtered this turn. Used by the burst's
    /// live-vs-buffer branch before any attempt runs.
    pub fn turn_level_pass(&self, random_draw: Option<f64>, trait_tags: &[&str]) -> bool {
        let random_ok = match (self.random, random_draw) {
            (Some(p), Some(d)) => d < p,
            (Some(_), None) => false, // misuse: a random predicate with no draw is a fail
            (None, _) => true,
        };
        random_ok && self.traits_pass(trait_tags)
    }

    /// Per-attempt decision. Returns `Some(fired)` when the trigger fires for
    /// this attempt, echoing the configured predicates verbatim; `None`
    /// otherwise. `fired` serialises to `chat_messages.filter_triggers` JSONB
    /// on write (empty ⇒ SQL NULL, handled by the stream layer).
    pub fn should_filter(
        &self,
        model_id: &str,
        trait_tags: &[&str],
        random_draw: Option<f64>,
    ) -> Option<FiredPredicates> {
        if !self.turn_level_pass(random_draw, trait_tags) {
            return None;
        }
        if !self.models_pass(model_id) {
            return None;
        }
        Some(FiredPredicates {
            random: self.random,
            models: self.models.clone(),
            traits: self.traits.clone(),
        })
    }

    fn models_pass(&self, model_id: &str) -> bool {
        self.models
            .as_ref()
            .is_none_or(|list| list.iter().any(|m| m == model_id))
    }

    fn traits_pass(&self, tags: &[&str]) -> bool {
        match &self.traits {
            None => true,
            Some(tp) => {
                let any_present = tp.any.iter().any(|a| tags.iter().any(|t| t == a));
                match tp.when {
                    TraitWhen::Present => any_present,
                    TraitWhen::Absent => !any_present,
                }
            }
        }
    }
}

/// Trait-match predicate: the predicate passes when at least one tag in `any`
/// is present among the turn's prompt traits (`when = present`) or absent
/// (`when = absent`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
pub struct TraitPredicate {
    #[serde(default)]
    pub any: Vec<String>,
    #[serde(default)]
    pub when: TraitWhen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TraitWhen {
    #[default]
    Present,
    Absent,
}

/// Image-generation style preset key. Selected per turn by the frontend; the
/// engine owns the preset strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StyleKey {
    #[default]
    Realistic,
    SemiRealistic,
    Anime,
}

pub const STYLE_REALISTIC: &str = "Photorealistic candid lifestyle photography, natural skin texture, believable anatomy, soft natural lighting, authentic smartphone photo aesthetic.";
pub const STYLE_SEMI_REALISTIC: &str = "Semi-realistic digital character illustration, believable anatomy, softly painted skin, subtly stylized facial features, detailed cinematic lighting.";
pub const STYLE_ANIME: &str = "High-quality Japanese anime illustration, clean expressive line art, detailed eyes, polished cel shading, coherent anatomy and detailed background.";

pub fn style_preset(key: StyleKey) -> &'static str {
    match key {
        StyleKey::Realistic => STYLE_REALISTIC,
        StyleKey::SemiRealistic => STYLE_SEMI_REALISTIC,
        StyleKey::Anime => STYLE_ANIME,
    }
}

/// When the output filter runs relative to the post-process extraction pipeline
/// (insight/memory/affinity). `AfterExtract` (default): extraction reads the
/// original reply, only the client output is filtered. `BeforeExtract`:
/// extraction reads the filtered text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FilterTiming {
    #[default]
    AfterExtract,
    BeforeExtract,
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
    #[serde(default)]
    pub output_filter: Option<bool>,
    #[serde(default)]
    pub filter_prompt: Option<String>,
    #[serde(default)]
    pub trigger: Option<OutputFilterTrigger>,
    #[serde(default)]
    pub timing: Option<FilterTiming>,
    #[serde(default)]
    pub retry_depth: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DefaultConfig {
    #[serde(default)]
    pub fallback_model: Option<String>,
    #[serde(default)]
    pub fallback_temperature: Option<f64>,
    #[serde(default)]
    pub fallback_max_tokens: Option<u32>,
    /// OpenRouter provider slugs to exclude from routing on EVERY task
    /// (issue #84). Sent as `provider.ignore` on every outbound call; the
    /// client reads this once at boot. Empty = no exclusion.
    #[serde(default)]
    pub ignore_providers: Vec<String>,
}

fn default_model_spec() -> ModelSpec {
    ModelSpec::Fixed(String::new())
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskConfig {
    #[serde(default = "default_model_spec")]
    pub model: ModelSpec,
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Nucleus-sampling probability mass. Chat task only; task-level (tiers
    /// inherit, like `temperature`); no `[defaults]` fallback. `None` ⇒ omit.
    #[serde(default)]
    pub top_p: Option<f32>,
    /// OpenAI-style frequency penalty. Same scoping rules as `top_p`.
    #[serde(default)]
    pub frequency_penalty: Option<f32>,
    /// OpenAI-style presence penalty. Same scoping rules as `top_p`.
    #[serde(default)]
    pub presence_penalty: Option<f32>,
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
    /// Client-facing display override for `meta.model` (chat task only).
    /// Task-level; tiers inherit. Absent → `None` (treated as `false` → the
    /// `model` field is omitted from chat `meta` frames). See `DisplayOverride`.
    #[serde(default)]
    pub model_name_display_override: Option<DisplayOverride>,
    #[serde(default)]
    pub output_filter: Option<bool>,
    /// Global trigger for the user-input rewrite filter (chat_input_filter).
    /// Read ONLY on [tasks.chat_companion]; task-level, no per-tier override
    /// (unlike `output_filter`). `false`/absent ⇒ off, `true` ⇒ every turn,
    /// `0.8` ⇒ 80% of turns. See `InputFilterTrigger`.
    #[serde(default)]
    pub input_filter: Option<InputFilterTrigger>,
    /// System instruction sent to the filter LLM; the assistant reply to
    /// rewrite is passed as a SEPARATE user message — this is NOT a template
    /// with placeholder substitution.
    #[serde(default)]
    pub filter_prompt: Option<String>,
    #[serde(default)]
    pub trigger: Option<OutputFilterTrigger>,
    #[serde(default)]
    pub timing: Option<FilterTiming>,
    /// Number of fallback models the filter may try on failure; the runtime
    /// defaults this to 1 (primary + first fallback) when unset.
    #[serde(default)]
    pub retry_depth: Option<u32>,
    /// PDE-only: ghost kill-switch. `false` disables ghosting across the whole
    /// PDE path; absent/`true` keeps it on. Read only on `[tasks.pde_decision]`
    /// (other tasks ignore it), like `input_filter`/`dimensions`.
    #[serde(default)]
    pub ghosting: Option<bool>,
    /// PDE-only: send `response_format = json_schema` on the judge request to
    /// raise JSON adherence. Absent/`true` ⇒ on; `false` ⇒ off (escape hatch for
    /// a provider that rejects the param). Read only on `[tasks.pde_decision]`.
    #[serde(default)]
    pub structured_output: Option<bool>,
    /// Per-tier overrides keyed by tier name. Empty for tasks that don't tier.
    #[serde(default)]
    pub tiers: HashMap<String, TierConfig>,
    /// chat_image_generation-only: default style when the frontend omits one.
    #[serde(default)]
    pub default_style: Option<StyleKey>,
    /// chat_image_generation-only: default aspect ratio (e.g. "3:4").
    #[serde(default)]
    pub default_aspect_ratio: Option<String>,
    /// chat_image_generation-only: default resolution (e.g. "1024x1365").
    #[serde(default)]
    pub default_resolution: Option<String>,
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
    /// Optional sampling knobs resolved from the task block (chat task only).
    /// `None` ⇒ the corresponding wire param is omitted.
    pub top_p: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub max_tokens: u32,
    /// Resolved trait allow-list. `None` → no gating; `Some(set)` → the chat
    /// handler keeps only `prompt_traits` whose tag is in `set`.
    pub allow_traits: Option<Vec<String>>,
    /// Resolved reasoning config (see `TaskConfig::reasoning`). `None` → omit
    /// the wire param; `Some(cfg)` → forwarded as the `reasoning` object.
    pub reasoning: Option<ReasoningConfig>,
    /// Number of fallback models the chat burst may try after the primary.
    /// `fallback_model` is already truncated to this length by `resolve()`.
    /// Task-level → tier override precedence, default 2 (primary + 2 fallbacks
    /// = 3-entry chain, matching the prior `MAX_STREAM_FALLBACK_DEPTH = 3`
    /// hard-cap).
    pub retry_depth: u32,
}

/// Resolved output-filter parameters for a chat request.
///
/// `fallback_model` is already truncated to `retry_depth` entries —
/// the runtime tries the primary, then each entry in order, and stops after
/// `retry_depth` total attempts beyond the primary.
#[derive(Debug, Clone)]
pub struct ResolvedOutputFilter {
    pub model: String,
    pub fallback_model: Vec<String>, // already truncated to retry_depth
    pub temperature: f64,
    pub max_tokens: u32,
    pub filter_prompt: String,
    pub trigger: OutputFilterTrigger,
    pub timing: FilterTiming,
    pub retry_depth: u32,
    /// Reasoning config forwarded from `[tasks.chat_output_filter]`. Task-level
    /// only (no per-tier override), consistent with `chat_companion`'s own
    /// `reasoning` field shape.
    pub reasoning: Option<ReasoningConfig>,
}

/// Per-turn trigger for the user-input rewrite filter (`input_filter` on
/// `[tasks.chat_companion]`). Three TOML forms: `false` (never, probability
/// 0.0), `true` (always, probability 1.0), or a number in `[0.0, 1.0]` (e.g.
/// `0.8` ⇒ fire on ~80% of turns). A number outside `[0.0, 1.0]` (or non-finite)
/// is a hard config error — the load fails loudly rather than silently clamping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputFilterTrigger(pub f64);

impl<'de> Deserialize<'de> for InputFilterTrigger {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct TriggerVisitor;
        impl<'de> serde::de::Visitor<'de> for TriggerVisitor {
            type Value = InputFilterTrigger;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a bool, or a probability number in [0.0, 1.0]")
            }
            fn visit_bool<E>(self, b: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(InputFilterTrigger(if b { 1.0 } else { 0.0 }))
            }
            fn visit_f64<E>(self, x: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if x.is_finite() && (0.0..=1.0).contains(&x) {
                    Ok(InputFilterTrigger(x))
                } else {
                    Err(E::custom(format!(
                        "input_filter probability must be between 0.0 and 1.0, got {x}"
                    )))
                }
            }
            fn visit_i64<E>(self, x: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_f64(x as f64)
            }
            fn visit_u64<E>(self, x: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_f64(x as f64)
            }
        }
        deserializer.deserialize_any(TriggerVisitor)
    }
}

/// Resolved user-input rewrite filter (`chat_input_filter`). Mirrors
/// `ResolvedOutputFilter` minus `trigger`/`timing` (the input filter has no
/// extract-timing). `fallback_model` is already truncated to `retry_depth`.
#[derive(Debug, Clone)]
pub struct ResolvedInputFilter {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub filter_prompt: String,
    pub retry_depth: u32,
    pub reasoning: Option<ReasoningConfig>,
    /// Per-turn fire probability in `[0.0, 1.0]` (always > 0.0 here — a 0.0
    /// trigger resolves to `None`). The stream wiring draws one coin flip per
    /// turn and runs the filter LLM only when `draw < probability`.
    pub probability: f64,
}

/// Resolved image-describe task (`chat_vision`). Mirrors `ResolvedInputFilter`
/// minus the per-turn probability — the trigger is "image present", decided in
/// the stream wiring. `fallback_model` is already truncated to `retry_depth`.
#[derive(Debug, Clone)]
pub struct ResolvedVision {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub describe_prompt: String,
    pub retry_depth: u32,
    pub reasoning: Option<ReasoningConfig>,
}

/// Resolved PDE decision task (`pde_decision`). Mirrors `ResolvedVision`: the
/// configured `filter_prompt` is the judge's system instruction; the engine
/// builds the user payload (transcript + affinity + signals). `fallback_model`
/// is already truncated to `retry_depth`.
#[derive(Debug, Clone)]
pub struct ResolvedPde {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub decision_prompt: String,
    pub retry_depth: u32,
    pub reasoning: Option<ReasoningConfig>,
    pub structured_output: bool,
}

/// Resolved extraction task (`insight_extraction` facts stage / `memory_extraction`).
/// The configured `filter_prompt` is the system instruction; the server assembles
/// the conversation as a separate user message. Model selection mirrors the generic
/// `resolve()` exactly (this only adds the prompt), so call-site behaviour is unchanged
/// apart from the system/user split.
#[derive(Debug, Clone)]
pub struct ResolvedExtract {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub extract_prompt: String,
    pub retry_depth: u32,
    pub reasoning: Option<ReasoningConfig>,
}

/// Resolved image-generation task (`chat_image_generation`). `model` is optional:
/// `None` means defer entirely to the per-turn frontend model. The per-turn
/// chain is built by `effective_image_chain`.
#[derive(Debug, Clone)]
pub struct ResolvedImageGen {
    pub model: Option<ModelSpec>,
    pub fallback_model: Vec<String>,
    pub default_style: StyleKey,
    pub default_aspect_ratio: String,
    pub default_resolution: Option<String>,
    pub max_tokens: u32,
}

impl TaskConfig {
    /// Image-gen view of `model`: an empty Fixed string ⇒ None (model deferred
    /// to the per-turn frontend).
    pub(crate) fn model_image_opt(&self) -> Option<ModelSpec> {
        match &self.model {
            ModelSpec::Fixed(s) if s.is_empty() => None,
            other => Some(other.clone()),
        }
    }
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
                tracing::warn!(
                    task,
                    level,
                    "model_config: empty model spec, falling through"
                );
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

        // Task-level only (tiers inherit; no `[defaults]` fallback). None ⇒ omit.
        let top_p = task_cfg.and_then(|t| t.top_p);
        let frequency_penalty = task_cfg.and_then(|t| t.frequency_penalty);
        let presence_penalty = task_cfg.and_then(|t| t.presence_penalty);

        // Task-level only (tiers inherit), mirroring temperature/max_tokens.
        let reasoning = task_cfg.and_then(|t| t.reasoning.clone());

        // retry_depth: tier > task > default 2. Truncate fallback_model to
        // retry_depth entries so the caller never needs to cap the chain.
        let retry_depth = tier_cfg
            .and_then(|t| t.retry_depth)
            .or_else(|| task_cfg.and_then(|t| t.retry_depth))
            .unwrap_or(2);
        fallback_model.truncate(retry_depth as usize);

        ResolvedModel {
            model,
            fallback_model,
            temperature,
            top_p,
            frequency_penalty,
            presence_penalty,
            max_tokens,
            allow_traits,
            reasoning,
            retry_depth,
        }
    }

    /// Task-level display override, read WITHOUT running model selection — so
    /// the replay path can read it without advancing round-robin / weighted
    /// cursors. Tier-independent (the field is task-level; tiers inherit it).
    /// `None` when the task is unknown or sets no override.
    pub fn display_override(&self, task: &str) -> Option<DisplayOverride> {
        self.tasks
            .get(task)
            .and_then(|t| t.model_name_display_override.clone())
    }

    /// Resolve `output_filter` for `task`: tier override → task default → false.
    pub fn output_filter_enabled(&self, task: &str, tier: Option<&str>) -> bool {
        let task_cfg = self.tasks.get(task);
        let tier_cfg = match (task_cfg, tier) {
            (Some(t), Some(name)) => t.tiers.get(name),
            _ => None,
        };
        tier_cfg
            .and_then(|t| t.output_filter)
            .or_else(|| task_cfg.and_then(|t| t.output_filter))
            .unwrap_or(false)
    }

    /// Resolve the output filter for a chat request. `None` (filter disabled) when:
    /// chat_companion `output_filter` is false (tier→task→false), OR the
    /// `chat_output_filter` task is absent, OR its resolved `filter_prompt` is blank.
    pub fn resolve_output_filter(&self, tier: Option<&str>) -> Option<ResolvedOutputFilter> {
        const FILTER_TASK: &str = "chat_output_filter";
        if !self.output_filter_enabled("chat_companion", tier) {
            return None;
        }
        let task_cfg = self.tasks.get(FILTER_TASK)?; // #6: table absent ⇒ None
        let tier_cfg = tier.and_then(|name| task_cfg.tiers.get(name));

        // filter_prompt / trigger / timing: tier → default block.
        let filter_prompt = tier_cfg
            .and_then(|t| t.filter_prompt.clone())
            .or_else(|| task_cfg.filter_prompt.clone())
            .unwrap_or_default();
        if filter_prompt.trim().is_empty() {
            return None; // no usable instruction ⇒ inert
        }
        let trigger = tier_cfg
            .and_then(|t| t.trigger.clone())
            .or_else(|| task_cfg.trigger.clone())
            .unwrap_or(OutputFilterTrigger {
                random: None,
                models: None,
                traits: None,
            });
        let timing = tier_cfg
            .and_then(|t| t.timing)
            .or(task_cfg.timing)
            .unwrap_or_default();
        let retry_depth = tier_cfg
            .and_then(|t| t.retry_depth)
            .or(task_cfg.retry_depth)
            .unwrap_or(1); // default 1: primary + first fallback only

        // reasoning: task-level only (no per-tier override), consistent with
        // chat_companion's own reasoning field.
        let reasoning = task_cfg.reasoning.clone();

        // model / fallback / temperature / max_tokens via the existing resolver
        // (tier → default block → [defaults] → compiled-in). Note: resolve()
        // now truncates fallback_model to its own retry_depth; we re-truncate
        // to chat_output_filter's retry_depth (which may differ).
        let m = self.resolve(FILTER_TASK, tier);
        let mut fallback_model = m.fallback_model;
        fallback_model.truncate(retry_depth as usize); // cap to filter's retry_depth entries
        Some(ResolvedOutputFilter {
            model: m.model,
            fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            filter_prompt,
            trigger,
            timing,
            retry_depth,
            reasoning,
        })
    }

    /// chat_companion task-level `input_filter` fire probability; no tier
    /// override. `false`/absent → 0.0, `true` → 1.0, number → that probability.
    /// The per-turn coin flip happens in the stream wiring.
    pub fn input_filter_probability(&self) -> f64 {
        self.tasks
            .get("chat_companion")
            .and_then(|t| t.input_filter)
            .map(|t| t.0)
            .unwrap_or(0.0)
    }

    /// True when the input filter can ever fire (probability > 0.0).
    pub fn input_filter_enabled(&self) -> bool {
        self.input_filter_probability() > 0.0
    }

    /// Resolve the user-input rewrite filter. `None` (disabled) when:
    /// chat_companion `input_filter` probability is 0.0 (false/absent), OR
    /// `[tasks.chat_input_filter]` is absent, OR its resolved `filter_prompt` is
    /// blank. The carried `probability` gates the per-turn run in the wiring.
    pub fn resolve_input_filter(&self) -> Option<ResolvedInputFilter> {
        const FILTER_TASK: &str = "chat_input_filter";
        let probability = self.input_filter_probability();
        if probability <= 0.0 {
            return None;
        }
        let task_cfg = self.tasks.get(FILTER_TASK)?;
        let filter_prompt = task_cfg.filter_prompt.clone().unwrap_or_default();
        if filter_prompt.trim().is_empty() {
            return None;
        }
        let retry_depth = task_cfg.retry_depth.unwrap_or(1);
        let m = self.resolve(FILTER_TASK, None);
        let mut fallback_model = m.fallback_model;
        fallback_model.truncate(retry_depth as usize);
        Some(ResolvedInputFilter {
            model: m.model,
            fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            filter_prompt,
            retry_depth,
            reasoning: task_cfg.reasoning.clone(),
            probability,
        })
    }

    /// Resolve the image-describe task. `None` (feature off) when
    /// `[tasks.chat_vision]` is absent OR its `filter_prompt` is blank. Reuses
    /// the generic `TaskConfig.filter_prompt` field and the standard `resolve()`
    /// model/fallback machinery. No probability gate — image presence is the
    /// trigger, decided in the stream wiring.
    pub fn resolve_vision(&self) -> Option<ResolvedVision> {
        const VISION_TASK: &str = "chat_vision";
        let task_cfg = self.tasks.get(VISION_TASK)?;
        let describe_prompt = task_cfg.filter_prompt.clone().unwrap_or_default();
        if describe_prompt.trim().is_empty() {
            return None;
        }
        let retry_depth = task_cfg.retry_depth.unwrap_or(1);
        let m = self.resolve(VISION_TASK, None);
        let mut fallback_model = m.fallback_model;
        fallback_model.truncate(retry_depth as usize);
        Some(ResolvedVision {
            model: m.model,
            fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            describe_prompt,
            retry_depth,
            reasoning: task_cfg.reasoning.clone(),
        })
    }

    /// Resolve the PDE decision task. `None` (feature off → rule engine) when
    /// `[tasks.pde_decision]` is absent OR its `filter_prompt` is blank. Reuses
    /// the generic `resolve()` machinery; task-level only (no tier override),
    /// like `chat_vision`.
    pub fn resolve_pde(&self) -> Option<ResolvedPde> {
        const PDE_TASK: &str = "pde_decision";
        let task_cfg = self.tasks.get(PDE_TASK)?;
        let decision_prompt = task_cfg.filter_prompt.clone().unwrap_or_default();
        if decision_prompt.trim().is_empty() {
            return None;
        }
        let retry_depth = task_cfg.retry_depth.unwrap_or(1);
        let m = self.resolve(PDE_TASK, None);
        let mut fallback_model = m.fallback_model;
        fallback_model.truncate(retry_depth as usize);
        Some(ResolvedPde {
            model: m.model,
            fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            decision_prompt,
            retry_depth,
            reasoning: task_cfg.reasoning.clone(),
            structured_output: task_cfg.structured_output.unwrap_or(true),
        })
    }

    /// Resolve the image-generation task. `None` (feature off) when
    /// `[tasks.chat_image_generation]` is absent. `Some(_)` means ENABLED — a
    /// usable model is resolved per-turn by `effective_image_chain`.
    pub fn resolve_image_gen(&self) -> Option<ResolvedImageGen> {
        const IMG_TASK: &str = "chat_image_generation";
        let task_cfg = self.tasks.get(IMG_TASK)?;
        let retry_depth = task_cfg.retry_depth.unwrap_or(2);
        let mut fallback_model = task_cfg
            .fallback
            .clone()
            .map(FallbackSpec::into_vec)
            .unwrap_or_default();
        fallback_model.truncate(retry_depth as usize);
        Some(ResolvedImageGen {
            model: task_cfg.model_image_opt(),
            fallback_model,
            default_style: task_cfg.default_style.unwrap_or_default(),
            default_aspect_ratio: task_cfg
                .default_aspect_ratio
                .clone()
                .unwrap_or_else(|| "1:1".to_string()),
            default_resolution: task_cfg.default_resolution.clone(),
            max_tokens: task_cfg.max_tokens.unwrap_or(4096),
        })
    }

}

/// Drop later duplicates, preserving first-seen order.
fn dedup_keep_first(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|s| seen.insert(s.clone()));
}

/// Build the per-turn image model chain. Returns `None` ⇒ no model anywhere ⇒
/// the turn cannot generate (caller degrades to text). `Some((primary, chain))`
/// otherwise. Order: per-turn override → config `ModelSpec` → config fallback.
pub fn effective_image_chain(
    req_model: Option<&str>,
    resolved: Option<&ResolvedImageGen>,
) -> Option<(String, Vec<String>)> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(m) = req_model.map(str::trim).filter(|s| !s.is_empty()) {
        candidates.push(m.to_owned());
    }
    if let Some(r) = resolved {
        if let Some(m) = r.model.as_ref().and_then(ModelSpec::select) {
            candidates.push(m);
        }
        candidates.extend(r.fallback_model.iter().cloned());
    }
    dedup_keep_first(&mut candidates);
    let mut it = candidates.into_iter();
    it.next().map(|primary| (primary, it.collect()))
}

impl ModelConfig {
    /// PDE ghost kill-switch. `true` (default) ⇒ ghost honoured; `false` ⇒ the
    /// whole PDE path never produces a Ghost. Read INDEPENDENTLY of
    /// `filter_prompt`, so it also governs the pure rule engine (LLM PDE off).
    pub fn pde_ghosting_enabled(&self) -> bool {
        self.tasks
            .get("pde_decision")
            .and_then(|t| t.ghosting)
            .unwrap_or(true)
    }

    /// Resolve the insight-extraction (facts stage) prompt bundle. `None` when
    /// `[tasks.insight_extraction]` is absent OR its `filter_prompt` is blank.
    pub fn resolve_insight_extract(&self) -> Option<ResolvedExtract> {
        self.resolve_extract("insight_extraction")
    }

    /// Resolve the memory-extraction prompt bundle. `None` when
    /// `[tasks.memory_extraction]` is absent OR its `filter_prompt` is blank.
    pub fn resolve_memory_extract(&self) -> Option<ResolvedExtract> {
        self.resolve_extract("memory_extraction")
    }

    /// Shared resolver for the config-driven extraction prompts. Mirrors
    /// `resolve_vision` but takes model/fallback/temp/max_tokens/reasoning/retry_depth
    /// straight from `resolve()` so the call site keeps today's selection semantics.
    fn resolve_extract(&self, task: &str) -> Option<ResolvedExtract> {
        let task_cfg = self.tasks.get(task)?;
        let extract_prompt = task_cfg.filter_prompt.clone().unwrap_or_default();
        if extract_prompt.trim().is_empty() {
            return None;
        }
        let m = self.resolve(task, None);
        Some(ResolvedExtract {
            model: m.model,
            fallback_model: m.fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            extract_prompt,
            retry_depth: m.retry_depth,
            reasoning: m.reasoning,
        })
    }

    /// Boot-time validation for the two extraction tasks. A task **section that
    /// is present** must carry a usable `filter_prompt` (else `Err`); an
    /// **absent section** means that extraction is simply off (`Ok`). Returns a
    /// ready-to-print message naming the first misconfigured task.
    ///
    /// Scoped to `insight_extraction` / `memory_extraction` — the only tasks the
    /// boot gate makes mandatory-when-present.
    pub fn validate_extraction_prompts(&self) -> Result<(), String> {
        for name in ["insight_extraction", "memory_extraction"] {
            if self.tasks.contains_key(name) && self.resolve_extract(name).is_none() {
                return Err(format!(
                    "[tasks.{name}] is present but its filter_prompt is unset — eros-engine \
                     refuses to boot. Set a filter_prompt, or remove the [tasks.{name}] \
                     section to disable {name}."
                ));
            }
        }
        Ok(())
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
        assert!(matches!(
            cfg.tasks["rr"].model,
            ModelSpec::RoundRobin { .. }
        ));
        assert!(matches!(
            cfg.tasks["weighted"].model,
            ModelSpec::Weighted(_)
        ));
    }

    #[test]
    fn weighted_drops_non_finite_weights() {
        // `inf` is a valid TOML float and passes `> 0.0`, but must be dropped:
        // an infinite sum would panic `gen_range(0.0..sum)` in select(). The
        // sole entry is filtered, leaving an empty spec that falls through.
        let toml = r#"
[defaults]
fallback_model = "fb"
[tasks.t]
model = { "a" = inf }
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        // Resolve many times: a surviving inf weight would panic, not just
        // return the wrong model.
        for _ in 0..50 {
            assert_eq!(cfg.resolve("t", None).model, "fb");
        }

        // A finite sibling still wins when inf is dropped.
        let toml = r#"
[tasks.t]
model = { "a" = inf, "b" = 1.0 }
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert_eq!(cfg.resolve("t", None).model, "b");
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
input_filter = true

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
model         = "x-ai/grok-4-mini"
temperature   = 0.5
max_tokens    = 200
description   = "LLM decision layer"
filter_prompt    = "Decide the action and inner_state."
ghosting         = false
structured_output = true

[tasks.embedding]
model        = "voyage-3-lite"
dimensions   = 512
description  = "reserved — Voyage hard-codes its own model"

[tasks.chat_input_filter]
model        = "openai/gpt-5.4-nano"
fallback     = "deepseek/deepseek-chat-v3.2"
retry_depth  = 1
temperature  = 0.3
max_tokens   = 400
filter_prompt = "Rewrite per policy."
reasoning    = { enabled = false }
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
        assert_eq!(
            gold.model.as_ref().and_then(ModelSpec::as_fixed),
            Some("x-ai/grok-4.20")
        );
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
        assert_eq!(
            pde.filter_prompt.as_deref(),
            Some("Decide the action and inner_state.")
        );
        assert_eq!(pde.ghosting, Some(false));
        assert!(cfg.resolve_pde().is_some());
        assert!(!cfg.pde_ghosting_enabled());

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

        // chat_input_filter schema lock (input-filter feature).
        assert_eq!(chat.input_filter, Some(InputFilterTrigger(1.0)));
        let inf = cfg
            .resolve_input_filter()
            .expect("input filter resolves from fixture");
        assert_eq!(inf.model, "openai/gpt-5.4-nano");
        assert_eq!(inf.retry_depth, 1);
        assert_eq!(inf.max_tokens, 400);
        assert_eq!(inf.filter_prompt, "Rewrite per policy.");
        assert_eq!(inf.probability, 1.0);
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
        let text = include_str!("../../../examples/model_config.toml");
        let cfg = ModelConfig::from_toml_str(text).expect("examples/model_config.toml must parse");
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
        let text = include_str!("../../../examples/model_config.toml");
        let cfg = ModelConfig::from_toml_str(text).expect("examples/model_config.toml must parse");
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
    fn committed_example_chat_companion_sets_sampling_defaults() {
        let text = include_str!("../../../examples/model_config.toml");
        let cfg = ModelConfig::from_toml_str(text).expect("examples/model_config.toml must parse");
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.top_p, Some(0.9));
        assert_eq!(r.frequency_penalty, Some(0.4));
        assert_eq!(r.presence_penalty, Some(0.2));
        // Extraction stays deterministic — no sampling knobs.
        let e = cfg.resolve("insight_extraction", None);
        assert_eq!(e.top_p, None);
        assert_eq!(e.frequency_penalty, None);
        assert_eq!(e.presence_penalty, None);
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

    #[test]
    fn display_override_parses_all_four_forms() {
        let toml = r#"
[tasks.b_false]
model = "m"
model_name_display_override = false
[tasks.b_true]
model = "m"
model_name_display_override = true
[tasks.s]
model = "m"
model_name_display_override = "Aria"
[tasks.arr]
model = "m"
model_name_display_override = ["Aria", "Nova"]
[tasks.map]
model = "m"
model_name_display_override = { "deepseek/x" = "Aria", default = "Companion" }
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert_eq!(
            cfg.tasks["b_false"].model_name_display_override,
            Some(DisplayOverride::Bool(false))
        );
        assert_eq!(
            cfg.tasks["b_true"].model_name_display_override,
            Some(DisplayOverride::Bool(true))
        );
        assert_eq!(
            cfg.tasks["s"].model_name_display_override,
            Some(DisplayOverride::Fixed("Aria".into()))
        );
        assert_eq!(
            cfg.tasks["arr"].model_name_display_override,
            Some(DisplayOverride::Random(vec!["Aria".into(), "Nova".into()]))
        );
        let map = match &cfg.tasks["map"].model_name_display_override {
            Some(DisplayOverride::Map(m)) => m.clone(),
            other => panic!("expected Map, got {other:?}"),
        };
        assert_eq!(map.get("deepseek/x").map(String::as_str), Some("Aria"));
        assert_eq!(map.get("default").map(String::as_str), Some("Companion"));
    }

    #[test]
    fn display_method_truth_table() {
        assert_eq!(DisplayOverride::Bool(false).display("m"), None);
        assert_eq!(
            DisplayOverride::Bool(true).display("m"),
            Some("m".to_string())
        );
        assert_eq!(
            DisplayOverride::Fixed("Aria".into()).display("m"),
            Some("Aria".to_string())
        );
        assert_eq!(DisplayOverride::Fixed(String::new()).display("m"), None);
        assert_eq!(DisplayOverride::Random(vec![]).display("m"), None);
        assert_eq!(
            DisplayOverride::Random(vec!["only".into()]).display("m"),
            Some("only".to_string())
        );

        let mut map = std::collections::HashMap::new();
        map.insert("m1".to_string(), "n1".to_string());
        map.insert("default".to_string(), "nd".to_string());
        let ov = DisplayOverride::Map(map);
        assert_eq!(ov.display("m1"), Some("n1".to_string()));
        assert_eq!(ov.display("zzz"), Some("nd".to_string()));

        let mut map2 = std::collections::HashMap::new();
        map2.insert("m1".to_string(), "n1".to_string());
        let ov2 = DisplayOverride::Map(map2);
        assert_eq!(ov2.display("zzz"), None);
    }

    #[test]
    fn display_override_accessor_is_tier_independent_and_absent_is_none() {
        let toml = r#"
[tasks.chat_companion]
model = "m"
model_name_display_override = "Aria"

[tasks.chat_companion.tiers.gold]
model = "g"

[tasks.other]
model = "m"
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert_eq!(
            cfg.display_override("chat_companion"),
            Some(DisplayOverride::Fixed("Aria".into()))
        );
        assert_eq!(cfg.display_override("other"), None);
        assert_eq!(cfg.display_override("nonexistent"), None);
    }

    #[test]
    fn committed_example_chat_companion_shows_real_model() {
        let text = include_str!("../../../examples/model_config.toml");
        let cfg = ModelConfig::from_toml_str(text).expect("example must parse");
        // The shipped example opts into showing the real id (today's behavior).
        assert_eq!(
            cfg.display_override("chat_companion"),
            Some(DisplayOverride::Bool(true))
        );
        assert_eq!(
            cfg.display_override("chat_companion")
                .and_then(|d| d.display("deepseek/deepseek-v4-flash")),
            Some("deepseek/deepseek-v4-flash".to_string())
        );
        // A task without the field stays None (omit).
        assert_eq!(cfg.display_override("insight_extraction"), None);
    }

    #[test]
    fn output_filter_config_parses() {
        let toml = r#"
[tasks.chat_companion]
model = "m"
output_filter = false
[tasks.chat_companion.tiers.gold]
model = "g"
output_filter = true

[tasks.chat_output_filter]
model = "fast/model"
filter_prompt = "Rewrite: {x}"
temperature = 0.3
max_tokens = 400
retry_depth = 2
trigger = { random = 0.3, models = ["x/y"], traits = { any = ["nsfw"], when = "present" } }
timing = "after_extract"
[tasks.chat_output_filter.tiers.gold]
filter_prompt = "tier prompt"
trigger = { random = 1.0 }
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let cc = &cfg.tasks["chat_companion"];
        assert_eq!(cc.output_filter, Some(false));
        assert_eq!(cc.tiers["gold"].output_filter, Some(true));

        let f = &cfg.tasks["chat_output_filter"];
        assert_eq!(f.filter_prompt.as_deref(), Some("Rewrite: {x}"));
        assert_eq!(f.retry_depth, Some(2));
        assert_eq!(f.timing, Some(FilterTiming::AfterExtract));
        let trig = f.trigger.clone().unwrap();
        assert_eq!(trig.random, Some(0.3));
        assert_eq!(trig.models.as_deref(), Some(&["x/y".to_string()][..]));
        let tp = trig.traits.unwrap();
        assert_eq!(tp.any, vec!["nsfw".to_string()]);
        assert_eq!(tp.when, TraitWhen::Present);
        // per-tier override parses; tier trigger replaces default wholesale
        assert_eq!(f.tiers["gold"].trigger.clone().unwrap().random, Some(1.0));
        assert_eq!(
            f.tiers["gold"].filter_prompt.as_deref(),
            Some("tier prompt")
        );
    }

    #[test]
    fn trait_when_defaults_to_present() {
        let toml = r#"
[tasks.chat_output_filter]
model = "m"
filter_prompt = "p"
trigger = { traits = { any = ["a"] } }
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let tp = cfg.tasks["chat_output_filter"]
            .trigger
            .clone()
            .unwrap()
            .traits
            .unwrap();
        assert_eq!(tp.when, TraitWhen::Present);
    }

    #[test]
    fn should_filter_predicate_combinations() {
        use super::*;
        let none = OutputFilterTrigger {
            random: None,
            models: None,
            traits: None,
        };
        assert!(none.should_filter("any/model", &[], None).is_some());
        assert!(none.should_filter("any/model", &[], Some(0.999)).is_some());

        let r = OutputFilterTrigger {
            random: Some(0.5),
            models: None,
            traits: None,
        };
        assert!(r.should_filter("m", &[], Some(0.0)).is_some()); // draw < 0.5
        assert!(r.should_filter("m", &[], Some(0.999)).is_none()); // draw >= 0.5

        let m = OutputFilterTrigger {
            random: None,
            models: Some(vec!["x/y".into()]),
            traits: None,
        };
        assert!(m.should_filter("x/y", &[], None).is_some());
        assert!(m.should_filter("a/b", &[], None).is_none());

        let tp = OutputFilterTrigger {
            random: None,
            models: None,
            traits: Some(TraitPredicate {
                any: vec!["nsfw".into()],
                when: TraitWhen::Present,
            }),
        };
        assert!(tp.should_filter("m", &["nsfw"], None).is_some());
        assert!(tp.should_filter("m", &["sfw"], None).is_none());

        let ta = OutputFilterTrigger {
            random: None,
            models: None,
            traits: Some(TraitPredicate {
                any: vec!["nsfw".into()],
                when: TraitWhen::Absent,
            }),
        };
        assert!(ta.should_filter("m", &["sfw"], None).is_some());
        assert!(ta.should_filter("m", &["nsfw"], None).is_none());

        let all = OutputFilterTrigger {
            random: Some(0.5),
            models: Some(vec!["x/y".into()]),
            traits: Some(TraitPredicate {
                any: vec!["nsfw".into()],
                when: TraitWhen::Present,
            }),
        };
        assert!(all.should_filter("x/y", &["nsfw"], Some(0.0)).is_some());
        assert!(all.should_filter("x/y", &["nsfw"], Some(0.999)).is_none()); // random fails
        assert!(all.should_filter("a/b", &["nsfw"], Some(0.0)).is_none()); // model fails

        // turn_level_pass ignores models
        assert!(all.turn_level_pass(Some(0.0), &["nsfw"]));
        assert!(!all.turn_level_pass(Some(0.999), &["nsfw"]));
        assert!(!all.turn_level_pass(Some(0.0), &["sfw"]));
    }

    #[test]
    fn should_filter_returns_fired_config_on_match() {
        let t = OutputFilterTrigger {
            random: Some(0.3),
            models: Some(vec!["x/y".into()]),
            traits: Some(TraitPredicate {
                any: vec!["nsfw".into()],
                when: TraitWhen::Present,
            }),
        };
        let fired = t
            .should_filter("x/y", &["nsfw"], Some(0.18))
            .expect("should fire");
        // Echoes config verbatim — NOT observed values.
        assert_eq!(fired.random, Some(0.3));
        assert_eq!(fired.models.as_deref(), Some(&["x/y".to_string()][..]));
        assert_eq!(
            fired.traits,
            Some(TraitPredicate {
                any: vec!["nsfw".into()],
                when: TraitWhen::Present,
            })
        );
    }

    #[test]
    fn should_filter_returns_none_when_any_predicate_fails() {
        let t = OutputFilterTrigger {
            random: Some(0.3),
            models: Some(vec!["x/y".into()]),
            traits: None,
        };
        // random draw above p → fail.
        assert!(t.should_filter("x/y", &[], Some(0.9)).is_none());
        // model not in list → fail.
        assert!(t.should_filter("a/b", &[], Some(0.18)).is_none());
    }

    #[test]
    fn should_filter_empty_trigger_returns_empty_hits() {
        let t = OutputFilterTrigger {
            random: None,
            models: None,
            traits: None,
        };
        let hits = t.should_filter("any/model", &[], None).expect("fires");
        assert!(hits.random.is_none());
        assert!(hits.models.is_none());
        assert!(hits.traits.is_none());
    }

    #[test]
    fn should_filter_traits_absent_echoes_config_not_empty_vec() {
        let t = OutputFilterTrigger {
            random: None,
            models: None,
            traits: Some(TraitPredicate {
                any: vec!["nsfw".into()],
                when: TraitWhen::Absent,
            }),
        };
        // "nsfw" not in tags → predicate passes; the FIRED record echoes the
        // configured {any, when}, so a reader sees `when="absent"` directly.
        let fired = t.should_filter("m", &["sfw"], None).expect("fires");
        assert_eq!(
            fired.traits,
            Some(TraitPredicate {
                any: vec!["nsfw".into()],
                when: TraitWhen::Absent,
            })
        );
    }

    #[test]
    fn fired_predicates_serializes_only_fired_fields() {
        let fired = FiredPredicates {
            random: Some(0.3),
            models: None,
            traits: Some(TraitPredicate {
                any: vec!["nsfw_boost".into()],
                when: TraitWhen::Absent,
            }),
        };
        let v = serde_json::to_value(&fired).unwrap();
        assert_eq!(v["random"], serde_json::json!(0.3));
        assert!(v.get("models").is_none(), "absent fields skipped");
        assert_eq!(
            v["traits"],
            serde_json::json!({ "any": ["nsfw_boost"], "when": "absent" })
        );
    }

    #[test]
    fn should_filter_returns_none_when_random_configured_but_no_draw() {
        // Defensive: if the caller wires random=Some(p) but forgets to thread
        // a per-turn random_draw, treat as "no fire" rather than silently
        // assume pass. Guards against a Task 7-era wiring mistake.
        let r = OutputFilterTrigger {
            random: Some(0.5),
            models: None,
            traits: None,
        };
        assert!(r.should_filter("m", &[], None).is_none());
        assert!(!r.turn_level_pass(None, &[]));
    }

    #[test]
    fn fired_predicates_empty_serializes_to_empty_object_and_is_empty() {
        // Empty/always-fire trigger: no configured predicates. Serialises to
        // `{}` and is_empty() is true; the stream layer maps that to SQL NULL.
        let fired = FiredPredicates::default();
        assert!(fired.is_empty());
        let v = serde_json::to_value(&fired).unwrap();
        assert_eq!(v, serde_json::json!({}));
    }

    // ─── Item 1: reasoning threaded through resolve_output_filter ─────────

    #[test]
    fn resolve_output_filter_threads_reasoning() {
        let cfg: ModelConfig = toml::from_str(
            r#"
[tasks.chat_companion]
output_filter = true
model = "x/y"

[tasks.chat_output_filter]
model = "filter/m"
filter_prompt = "rewrite"
reasoning = { enabled = false }
"#,
        )
        .unwrap();
        let resolved = cfg.resolve_output_filter(None).expect("filter resolved");
        assert!(resolved.reasoning.is_some());
    }

    #[test]
    fn resolve_output_filter_reasoning_absent_is_none() {
        let cfg: ModelConfig = toml::from_str(
            r#"
[tasks.chat_companion]
output_filter = true
model = "x/y"

[tasks.chat_output_filter]
model = "filter/m"
filter_prompt = "rewrite"
"#,
        )
        .unwrap();
        let resolved = cfg.resolve_output_filter(None).expect("filter resolved");
        assert!(resolved.reasoning.is_none());
    }

    // ─── Item 2: chat_companion retry_depth ───────────────────────────────

    #[test]
    fn resolve_chat_companion_retry_depth_defaults_to_2() {
        let cfg: ModelConfig = toml::from_str(
            r#"
[tasks.chat_companion]
model = "x/y"
fallback = ["a/b", "c/d", "e/f", "g/h"]
"#,
        )
        .unwrap();
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.retry_depth, 2);
        // fallback truncated to retry_depth entries
        assert_eq!(r.fallback_model, vec!["a/b".to_string(), "c/d".to_string()]);
    }

    #[test]
    fn resolve_chat_companion_retry_depth_overridable() {
        let cfg: ModelConfig = toml::from_str(
            r#"
[tasks.chat_companion]
model = "x/y"
fallback = ["a/b", "c/d", "e/f"]
retry_depth = 3
"#,
        )
        .unwrap();
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.retry_depth, 3);
        assert_eq!(
            r.fallback_model,
            vec!["a/b".to_string(), "c/d".to_string(), "e/f".to_string()]
        );
    }

    #[test]
    fn resolve_chat_companion_retry_depth_tier_overrides_task() {
        let cfg: ModelConfig = toml::from_str(
            r#"
[tasks.chat_companion]
model = "x/y"
fallback = ["a/b", "c/d", "e/f"]
retry_depth = 2

[tasks.chat_companion.tiers.gold]
retry_depth = 1
"#,
        )
        .unwrap();
        let r = cfg.resolve("chat_companion", Some("gold"));
        assert_eq!(r.retry_depth, 1);
        assert_eq!(r.fallback_model, vec!["a/b".to_string()]);
    }

    #[test]
    fn resolve_output_filter_gating() {
        use super::*;
        // #6: enabled but no [tasks.chat_output_filter] ⇒ None
        let t =
            ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel=\"m\"\noutput_filter=true\n")
                .unwrap();
        assert!(t.output_filter_enabled("chat_companion", None));
        assert!(t.resolve_output_filter(None).is_none());

        // off by default (#7)
        let off = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel=\"m\"\n").unwrap();
        assert!(!off.output_filter_enabled("chat_companion", None));
        assert!(off.resolve_output_filter(None).is_none());

        // enabled + table + prompt ⇒ Some, resolves fields
        let on = ModelConfig::from_toml_str(
            r#"
[tasks.chat_companion]
model = "m"
output_filter = true
[tasks.chat_output_filter]
model = "fast/m"
fallback = ["a", "b", "c"]
filter_prompt = "P"
temperature = 0.4
max_tokens = 222
timing = "before_extract"
"#,
        )
        .unwrap();
        let r = on.resolve_output_filter(None).expect("some");
        assert_eq!(r.model, "fast/m");
        assert_eq!(r.filter_prompt, "P");
        assert_eq!(r.max_tokens, 222);
        assert_eq!(r.timing, FilterTiming::BeforeExtract);
        // retry_depth defaults to 1 ⇒ fallback truncated to the first entry
        assert_eq!(r.retry_depth, 1);
        assert_eq!(r.fallback_model, vec!["a".to_string()]);

        // explicit retry_depth = 0 ⇒ no fallback (primary only)
        let d0 = ModelConfig::from_toml_str(
            r#"
[tasks.chat_companion]
model = "m"
output_filter = true
[tasks.chat_output_filter]
model = "fast/m"
fallback = ["a", "b"]
filter_prompt = "P"
retry_depth = 0
"#,
        )
        .unwrap()
        .resolve_output_filter(None)
        .expect("some");
        assert_eq!(d0.retry_depth, 0);
        assert!(d0.fallback_model.is_empty());

        // blank filter_prompt ⇒ None even though enabled + table present
        let blank = ModelConfig::from_toml_str(
            r#"
[tasks.chat_companion]
model = "m"
output_filter = true
[tasks.chat_output_filter]
model = "fast/m"
filter_prompt = "   "
"#,
        )
        .unwrap();
        assert!(blank.resolve_output_filter(None).is_none());

        // tier output_filter overrides task default (#3); tier filter_prompt falls back to default (#5)
        let tiered = ModelConfig::from_toml_str(
            r#"
[tasks.chat_companion]
model = "m"
output_filter = false
[tasks.chat_companion.tiers.gold]
output_filter = true
[tasks.chat_output_filter]
model = "fast/m"
filter_prompt = "DEFAULT"
[tasks.chat_output_filter.tiers.gold]
model = "gold/m"
"#,
        )
        .unwrap();
        assert!(!tiered.output_filter_enabled("chat_companion", Some("free")));
        assert!(tiered.output_filter_enabled("chat_companion", Some("gold")));
        let rg = tiered.resolve_output_filter(Some("gold")).expect("some");
        assert_eq!(rg.model, "gold/m"); // tier model
        assert_eq!(rg.filter_prompt, "DEFAULT"); // fell back to default block (#5)
        assert_eq!(rg.timing, FilterTiming::AfterExtract); // default timing
    }

    #[test]
    fn resolve_input_filter_disabled_when_switch_off() {
        let toml = r#"
[tasks.chat_companion]
model = "m"
[tasks.chat_input_filter]
model = "f"
filter_prompt = "REWRITE"
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert!(!cfg.input_filter_enabled());
        assert!(cfg.resolve_input_filter().is_none());
    }

    #[test]
    fn resolve_input_filter_none_when_table_absent_or_blank_prompt() {
        // switch on, table absent
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel = \"m\"\ninput_filter = true\n",
        )
        .unwrap();
        assert!(cfg.input_filter_enabled());
        assert!(cfg.resolve_input_filter().is_none());

        // switch on, table present, blank prompt
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel = \"m\"\ninput_filter = true\n\
             [tasks.chat_input_filter]\nmodel = \"f\"\nfilter_prompt = \"   \"\n",
        )
        .unwrap();
        assert!(cfg.resolve_input_filter().is_none());
    }

    #[test]
    fn resolve_input_filter_some_when_enabled() {
        let toml = r#"
[tasks.chat_companion]
model = "m"
input_filter = true
[tasks.chat_input_filter]
model = "fast/in"
fallback = ["fb1", "fb2"]
retry_depth = 1
temperature = 0.3
max_tokens = 400
filter_prompt = "REWRITE"
reasoning = { enabled = false }
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let f = cfg.resolve_input_filter().expect("enabled");
        assert_eq!(f.model, "fast/in");
        // fallback truncated to retry_depth = 1
        assert_eq!(f.fallback_model, vec!["fb1".to_string()]);
        assert_eq!(f.retry_depth, 1);
        assert_eq!(f.filter_prompt, "REWRITE");
        assert_eq!(f.temperature, 0.3);
        assert_eq!(f.max_tokens, 400);
        assert_eq!(f.probability, 1.0); // `input_filter = true` ⇒ always
        assert_eq!(
            f.reasoning,
            Some(ReasoningConfig {
                enabled: Some(false),
                exclude: None
            })
        );
    }

    #[test]
    fn input_filter_trigger_parses_three_forms() {
        // false ⇒ probability 0.0 (disabled)
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel = \"m\"\ninput_filter = false\n",
        )
        .unwrap();
        assert_eq!(cfg.input_filter_probability(), 0.0);
        assert!(!cfg.input_filter_enabled());

        // true ⇒ 1.0
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel = \"m\"\ninput_filter = true\n",
        )
        .unwrap();
        assert_eq!(cfg.input_filter_probability(), 1.0);

        // number ⇒ that probability
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel = \"m\"\ninput_filter = 0.8\n",
        )
        .unwrap();
        assert_eq!(cfg.input_filter_probability(), 0.8);
        assert!(cfg.input_filter_enabled());

        // integer bounds 0 and 1 are accepted
        let cfg =
            ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel = \"m\"\ninput_filter = 1\n")
                .unwrap();
        assert_eq!(cfg.input_filter_probability(), 1.0);
    }

    #[test]
    fn input_filter_out_of_range_is_rejected() {
        // > 1.0, < 0.0, and non-finite are hard config errors (not clamped).
        for bad in ["1.5", "-0.2", "2", "nan", "inf"] {
            let toml = format!("[tasks.chat_companion]\nmodel = \"m\"\ninput_filter = {bad}\n");
            assert!(
                ModelConfig::from_toml_str(&toml).is_err(),
                "input_filter = {bad} must be rejected"
            );
        }
    }

    #[test]
    fn resolve_input_filter_carries_probability_and_zero_disables() {
        // 0.8 ⇒ Some with probability 0.8
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel = \"m\"\ninput_filter = 0.8\n\
             [tasks.chat_input_filter]\nmodel = \"f\"\nfilter_prompt = \"REWRITE\"\n",
        )
        .unwrap();
        let f = cfg.resolve_input_filter().expect("enabled");
        assert_eq!(f.probability, 0.8);

        // 0.0 ⇒ None (disabled), even with a valid filter table present
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel = \"m\"\ninput_filter = 0.0\n\
             [tasks.chat_input_filter]\nmodel = \"f\"\nfilter_prompt = \"REWRITE\"\n",
        )
        .unwrap();
        assert!(cfg.resolve_input_filter().is_none());
    }

    #[test]
    fn resolve_input_filter_retry_depth_zero_drops_fallback() {
        // retry_depth = 0 ⇒ primary only, no fallback (mirrors the output
        // filter's retry_depth=0 edge case).
        let cfg = ModelConfig::from_toml_str(
            r#"
[tasks.chat_companion]
model = "m"
input_filter = true
[tasks.chat_input_filter]
model = "fast/in"
fallback = ["a", "b"]
filter_prompt = "REWRITE"
retry_depth = 0
"#,
        )
        .unwrap();
        let f = cfg.resolve_input_filter().expect("enabled");
        assert_eq!(f.retry_depth, 0);
        assert!(f.fallback_model.is_empty());
    }

    #[test]
    fn resolve_vision_none_when_task_absent() {
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel = \"m\"\n").unwrap();
        assert!(cfg.resolve_vision().is_none());
    }

    #[test]
    fn resolve_vision_none_when_prompt_blank() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_vision]\nmodel = \"v\"\nfilter_prompt = \"   \"\n",
        )
        .unwrap();
        assert!(cfg.resolve_vision().is_none());
    }

    #[test]
    fn resolve_vision_some_truncates_fallback_to_retry_depth() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_vision]\n\
             model = \"v\"\n\
             fallback = [\"f1\", \"f2\", \"f3\"]\n\
             temperature = 0.2\n\
             max_tokens = 400\n\
             retry_depth = 1\n\
             filter_prompt = \"describe as json\"\n",
        )
        .unwrap();
        let r = cfg.resolve_vision().expect("vision resolves");
        assert_eq!(r.model, "v");
        assert_eq!(r.fallback_model, vec!["f1".to_string()]); // truncated to retry_depth=1
        assert_eq!(r.describe_prompt, "describe as json");
        assert_eq!(r.max_tokens, 400);
        assert_eq!(r.retry_depth, 1);
    }

    #[test]
    fn resolve_vision_retry_depth_zero_drops_fallback() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_vision]\n\
             model = \"v\"\n\
             fallback = [\"f1\", \"f2\"]\n\
             retry_depth = 0\n\
             filter_prompt = \"describe as json\"\n",
        )
        .unwrap();
        let r = cfg.resolve_vision().expect("vision resolves");
        assert_eq!(r.retry_depth, 0);
        assert!(r.fallback_model.is_empty());
    }

    #[test]
    fn resolve_insight_extract_none_when_task_absent() {
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel = \"m\"\n").unwrap();
        assert!(cfg.resolve_insight_extract().is_none());
    }

    #[test]
    fn resolve_insight_extract_none_when_prompt_blank() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.insight_extraction]\nmodel = \"m\"\nfilter_prompt = \"   \"\n",
        )
        .unwrap();
        assert!(cfg.resolve_insight_extract().is_none());
    }

    #[test]
    fn resolve_insight_extract_some_carries_prompt_and_model() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.insight_extraction]\nmodel = \"ins/m\"\nfilter_prompt = \"extract user facts\"\n",
        )
        .unwrap();
        let r = cfg.resolve_insight_extract().expect("resolves");
        assert_eq!(r.model, "ins/m");
        assert_eq!(r.extract_prompt, "extract user facts");
    }

    #[test]
    fn resolve_memory_extract_some_and_none() {
        let none =
            ModelConfig::from_toml_str("[tasks.memory_extraction]\nmodel = \"m\"\n").unwrap();
        assert!(none.resolve_memory_extract().is_none());

        let cfg = ModelConfig::from_toml_str(
            "[tasks.memory_extraction]\nmodel = \"mem/m\"\nfilter_prompt = \"extract memories\"\n",
        )
        .unwrap();
        let r = cfg.resolve_memory_extract().expect("resolves");
        assert_eq!(r.model, "mem/m");
        assert_eq!(r.extract_prompt, "extract memories");
    }

    #[test]
    fn resolve_extract_keeps_resolve_default_retry_depth() {
        // Deliberate behavior-preserving choice: extraction tasks are pre-existing
        // and inherit resolve()'s default retry_depth (2) — they do NOT cap at 1
        // like the newer chat_vision / chat_input_filter features. This pins that
        // so a future refactor toward the vision pattern can't silently halve the
        // extraction fallback chain.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.insight_extraction]\nmodel = \"ins/m\"\nfallback = [\"f1\", \"f2\"]\nfilter_prompt = \"p\"\n",
        )
        .unwrap();
        let r = cfg.resolve_insight_extract().expect("resolves");
        assert_eq!(r.retry_depth, 2);
        assert_eq!(r.fallback_model, vec!["f1".to_string(), "f2".to_string()]);
    }

    #[test]
    fn validate_extraction_absent_sections_ok() {
        // Neither extraction section present → both features off → Ok.
        let toml = r#"
[tasks.chat_companion]
model = "m"
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert!(cfg.validate_extraction_prompts().is_ok());
    }

    #[test]
    fn validate_extraction_present_with_prompt_ok() {
        let toml = r#"
[tasks.insight_extraction]
model = "m"
filter_prompt = "extract facts"

[tasks.memory_extraction]
model = "m"
filter_prompt = "extract memories"
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert!(cfg.validate_extraction_prompts().is_ok());
    }

    #[test]
    fn validate_extraction_present_without_prompt_errors() {
        let toml = r#"
[tasks.insight_extraction]
model = "m"
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let err = cfg.validate_extraction_prompts().unwrap_err();
        assert!(
            err.contains("insight_extraction"),
            "msg names the task: {err}"
        );
    }

    #[test]
    fn validate_extraction_present_blank_prompt_errors() {
        let toml = r#"
[tasks.memory_extraction]
model = "m"
filter_prompt = "   "
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let err = cfg.validate_extraction_prompts().unwrap_err();
        assert!(
            err.contains("memory_extraction"),
            "msg names the task: {err}"
        );
    }

    #[test]
    fn resolve_memory_extract_none_when_section_absent() {
        // Guards the dreaming sweeper's early-return condition (a later task).
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel = \"m\"\n").unwrap();
        assert!(cfg.resolve_memory_extract().is_none());
    }

    #[test]
    fn resolve_pde_none_when_absent_or_blank() {
        // absent
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel = \"m\"\n").unwrap();
        assert!(cfg.resolve_pde().is_none());
        // present but blank filter_prompt
        let cfg = ModelConfig::from_toml_str(
            "[tasks.pde_decision]\nmodel = \"m\"\nfilter_prompt = \"   \"\n",
        )
        .unwrap();
        assert!(cfg.resolve_pde().is_none());
    }

    #[test]
    fn resolve_pde_some_when_prompt_set() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.pde_decision]\nmodel = \"m\"\nfilter_prompt = \"decide\"\n",
        )
        .unwrap();
        let p = cfg.resolve_pde().expect("resolves");
        assert_eq!(p.model, "m");
        assert_eq!(p.decision_prompt, "decide");
    }

    #[test]
    fn resolve_pde_structured_output_default_true_else_field() {
        // absent → true
        let cfg = ModelConfig::from_toml_str(
            "[tasks.pde_decision]\nmodel = \"m\"\nfilter_prompt = \"d\"\n",
        )
        .unwrap();
        assert!(cfg.resolve_pde().unwrap().structured_output);
        // explicit false → false
        let cfg = ModelConfig::from_toml_str(
            "[tasks.pde_decision]\nmodel = \"m\"\nfilter_prompt = \"d\"\nstructured_output = false\n",
        ).unwrap();
        assert!(!cfg.resolve_pde().unwrap().structured_output);
    }

    #[test]
    fn pde_ghosting_enabled_default_true_else_field() {
        // task missing → true
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel = \"m\"\n").unwrap();
        assert!(cfg.pde_ghosting_enabled());
        // present, no ghosting → true
        let cfg = ModelConfig::from_toml_str("[tasks.pde_decision]\nmodel = \"m\"\n").unwrap();
        assert!(cfg.pde_ghosting_enabled());
        // ghosting = false → false
        let cfg =
            ModelConfig::from_toml_str("[tasks.pde_decision]\nmodel = \"m\"\nghosting = false\n")
                .unwrap();
        assert!(!cfg.pde_ghosting_enabled());
    }

    #[test]
    fn defaults_ignore_providers_parses() {
        let toml = r#"
            [defaults]
            ignore_providers = ["BadHost", "AnotherHost"]
            [tasks.chat_companion]
            model = "x/y"
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse");
        assert_eq!(
            cfg.defaults.ignore_providers,
            vec!["BadHost", "AnotherHost"]
        );
    }

    #[test]
    fn defaults_ignore_providers_absent_is_empty() {
        let toml = r#"
            [tasks.chat_companion]
            model = "x/y"
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse");
        assert!(cfg.defaults.ignore_providers.is_empty());
    }

    #[test]
    fn sampling_params_deserialize_and_resolve() {
        let toml = r#"
[tasks.chat_companion]
model = "m"
temperature = 0.8
top_p = 0.9
frequency_penalty = 0.4
presence_penalty = 0.2
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.top_p, Some(0.9));
        assert_eq!(r.frequency_penalty, Some(0.4));
        assert_eq!(r.presence_penalty, Some(0.2));
    }

    #[test]
    fn sampling_params_absent_resolve_to_none() {
        let toml = r#"
[tasks.chat_companion]
model = "m"
temperature = 0.8
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.top_p, None);
        assert_eq!(r.frequency_penalty, None);
        assert_eq!(r.presence_penalty, None);
    }

    #[test]
    fn committed_example_extraction_prompts_keep_contracts() {
        let text = include_str!("../../../examples/model_config.toml");
        let cfg = ModelConfig::from_toml_str(text).expect("examples/model_config.toml must parse");

        let mem = cfg
            .resolve_memory_extract()
            .expect("memory_extraction resolves from the committed config");
        // Five-category vocabulary preserved.
        for cat in ["fact", "preference", "event", "emotion", "relation"] {
            assert!(mem.extract_prompt.contains(cat), "missing category `{cat}`");
        }
        // JSON output contract preserved + new specificity anchor present.
        assert!(mem.extract_prompt.contains("\"memories\""), "json contract");
        assert!(
            mem.extract_prompt.contains("用户压力大"),
            "bad-example anchor"
        );

        let ins = cfg
            .resolve_insight_extract()
            .expect("insight_extraction resolves from the committed config");
        assert!(
            ins.extract_prompt.contains("\"facts\""),
            "facts json contract"
        );
    }

    // ─── Task 2: StyleKey presets + ResolvedImageGen + resolve_image_gen ──────

    #[test]
    fn resolve_image_gen_none_when_task_absent() {
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel=\"m\"\n").unwrap();
        assert!(cfg.resolve_image_gen().is_none());
    }

    #[test]
    fn resolve_image_gen_some_with_optional_model() {
        // Block present, NO model key → Some, with model: None.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_generation]\nfallback=[\"fb-img\"]\ndefault_style=\"anime\"\n",
        )
        .unwrap();
        let r = cfg.resolve_image_gen().expect("block present ⇒ Some");
        assert!(r.model.is_none());
        assert_eq!(r.fallback_model, vec!["fb-img".to_string()]);
        assert_eq!(r.default_style, StyleKey::Anime);
    }

    #[test]
    fn resolve_image_gen_carries_model_spec() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_generation]\nmodel=\"img-a\"\n",
        )
        .unwrap();
        let r = cfg.resolve_image_gen().unwrap();
        assert!(matches!(r.model, Some(ModelSpec::Fixed(ref s)) if s == "img-a"));
        assert_eq!(r.default_style, StyleKey::Realistic); // serde default
    }

    #[test]
    fn style_preset_maps_keys() {
        assert!(style_preset(StyleKey::Realistic).starts_with("Photorealistic"));
        assert!(style_preset(StyleKey::SemiRealistic).starts_with("Semi-realistic"));
        assert!(style_preset(StyleKey::Anime).starts_with("High-quality Japanese anime"));
    }

    #[test]
    fn regression_existing_task_model_still_resolves_fixed() {
        // Adding default_model_spec() must NOT affect tasks that explicitly set model.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel=\"x\"\n",
        )
        .unwrap();
        let task = cfg.tasks.get("chat_companion").unwrap();
        assert!(matches!(&task.model, ModelSpec::Fixed(s) if s == "x"));
        let r = cfg.resolve("chat_companion", None);
        assert_eq!(r.model, "x");
    }

    // ─── Task 3: effective_image_chain + dedup_keep_first ─────────────────────

    #[test]
    fn effective_chain_per_turn_wins_and_dedups() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_generation]\nmodel=\"cfg\"\nfallback=[\"X\",\"Y\"]\n",
        ).unwrap();
        let r = cfg.resolve_image_gen();
        // per-turn "X" + config "cfg" + fallback ["X","Y"] → [X, cfg, Y] (dedup X)
        assert_eq!(
            effective_image_chain(Some("X"), r.as_ref()),
            Some(("X".to_string(), vec!["cfg".to_string(), "Y".to_string()]))
        );
    }

    #[test]
    fn effective_chain_fallback_only_is_primary() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_generation]\nfallback=[\"Z\",\"W\"]\n",
        ).unwrap();
        let r = cfg.resolve_image_gen();
        assert_eq!(
            effective_image_chain(None, r.as_ref()),
            Some(("Z".to_string(), vec!["W".to_string()]))
        );
    }

    #[test]
    fn effective_chain_empty_is_none() {
        let cfg = ModelConfig::from_toml_str("[tasks.chat_image_generation]\n").unwrap();
        assert_eq!(effective_image_chain(None, cfg.resolve_image_gen().as_ref()), None);
        assert_eq!(effective_image_chain(None, None), None);
    }

    #[test]
    fn effective_chain_config_model_when_no_per_turn() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_generation]\nmodel=\"cfg\"\nfallback=[\"F\"]\n",
        ).unwrap();
        let r = cfg.resolve_image_gen();
        assert_eq!(
            effective_image_chain(None, r.as_ref()),
            Some(("cfg".to_string(), vec!["F".to_string()]))
        );
    }
}

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
    /// OpenRouter `provider.sort` routing preference applied on EVERY task:
    /// `"price"` / `"throughput"` / `"latency"`. `None` (absent) omits the
    /// field, keeping OpenRouter's default price-based load balancing. Setting
    /// it trades cost for the chosen axis — a deployer decision, off by default.
    #[serde(default)]
    pub provider_sort: Option<String>,
}

fn default_model_spec() -> ModelSpec {
    ModelSpec::Fixed(String::new())
}

/// One deterministic output-strip rule (read only from
/// `[tasks.chat_companion].output_regex`). Applied to the assistant reply
/// produced by any model in `models`. `replacement` substitutes for each
/// match; `None` ⇒ `""` (delete). See
/// docs/superpowers/specs/2026-06-28-per-model-output-regex-filter-design.md.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutputRegexRule {
    pub models: Vec<String>,
    pub pattern: String,
    #[serde(default)]
    pub replacement: Option<String>,
}

/// A compiled `output_regex` rule, ready to apply. Built once at boot by
/// `ModelConfig::compile_output_regex`; `replacement` is `""` for delete.
#[derive(Debug, Clone)]
pub struct CompiledRegexRule {
    pub models: Vec<String>,
    pub regex: regex::Regex,
    pub replacement: String,
}

/// Result of applying output-regex rules to one reply. `matched_rules` lists
/// the rule indices that changed the text (empty ⇒ unchanged or fail-safed).
#[derive(Debug, Clone)]
pub struct RegexStripOutcome {
    pub cleaned: String,
    pub matched_rules: Vec<usize>,
}

/// Apply every rule whose `models` contains `model_id`, in declaration order.
/// Pure & deterministic. No fail-safe: a reply that is *entirely* an artifact
/// (e.g. a bare `[你给对方发送了一张照片：…]`, or one wrapped in incidental
/// whitespace) strips to an empty string, and the match is still reported. The
/// caller persists the audit (raw on `pre_filter_content`) and emits no content
/// bubble — downstream decides how to render an empty/NULL reply (the web
/// client simply doesn't show it, a ghost-like effect).
pub fn apply_output_regex(
    rules: &[CompiledRegexRule],
    model_id: &str,
    text: &str,
) -> RegexStripOutcome {
    let mut cleaned = text.to_string();
    let mut matched_rules = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        if !rule.models.iter().any(|m| m == model_id) {
            continue;
        }
        let next = rule.regex.replace_all(&cleaned, rule.replacement.as_str());
        if next != cleaned {
            matched_rules.push(i);
            cleaned = next.into_owned();
        }
    }
    // An unanchored rule (e.g. `\[[^\]]*\]`) can leave incidental whitespace
    // behind when the reply was artifact-only (the common `<正文>\n\n[...]`
    // shape with an empty 正文). Collapse a whitespace-only *stripped* result to
    // a true empty string so the caller suppresses the bubble — the stream
    // layer gates on `is_empty()`, not `trim().is_empty()`. Only when a rule
    // actually matched: an untouched whitespace-only reply is left as-is.
    if !matched_rules.is_empty() && cleaned.trim().is_empty() {
        cleaned.clear();
    }
    RegexStripOutcome {
        cleaned,
        matched_rules,
    }
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
    /// Deterministic per-model regex strips for the assistant reply. Read ONLY
    /// on `[tasks.chat_companion]`; task-level, no per-tier override. Empty when
    /// absent. Compiled at boot via `compile_output_regex` (fail-fast).
    #[serde(default)]
    pub output_regex: Vec<OutputRegexRule>,
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
    /// chat_voice-only: opt into inline TTS audio tags (Gemini transcript tags
    /// like `[laughs]`, `[whispers]`). Absent/`false` ⇒ the built-in voice
    /// directive keeps forbidding brackets (unchanged behaviour). `true` ⇒ the
    /// directive invites inline `[tag]` markup; emitted tags flow through the
    /// voice path verbatim (no engine-side parsing/stripping). Read only by
    /// `resolve_voice`. See
    /// docs/superpowers/specs/2026-07-11-voice-tts-audio-tags-design.md.
    #[serde(default)]
    pub tts_audio_tags: Option<bool>,
    /// world_director-only: hours between per-owner director rounds. Read only
    /// on `[tasks.world_director]` (like `ghosting` on pde_decision). Default 24.
    #[serde(default)]
    pub interval_hours: Option<u32>,
    /// world_director-only: days of world_memories script retention. Default 30.
    #[serde(default)]
    pub retention_days: Option<u32>,
    /// world_comment-only: seconds between hourly comment rounds. Read only
    /// on `[tasks.world_comment]`. Default 3600, floor 60 (0 would fire a
    /// round every sweeper tick — cost footgun, same rationale as
    /// `interval_hours.max(1)`).
    #[serde(default)]
    pub round_secs: Option<u64>,
    /// world_reply-only: user-comment settle window in seconds. Default 90.
    #[serde(default)]
    pub debounce_secs: Option<u64>,
    /// world_reply-only: min seconds between responder comments per post.
    /// Default 600.
    #[serde(default)]
    pub thread_cooldown_secs: Option<u64>,
    /// world_reply-only: responder comments per owner per UTC day. Default 20.
    #[serde(default)]
    pub daily_cap: Option<u32>,
    /// world_reply-only: reply-eligibility window in seconds after a user
    /// comment. Default 604800 (7d); floored strictly above the resolved
    /// debounce (a window <= debounce leaves no eligible range). Bounds the
    /// reply scan so its cost is independent of total post count (issue #176).
    #[serde(default)]
    pub reply_window_secs: Option<u64>,
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

/// Built-in, product-identity-free voice directive. Deployments override it via
/// `[tasks.chat_voice].filter_prompt`. Kept terse: it is appended to the persona
/// prompt on every voice turn.
pub const DEFAULT_VOICE_DIRECTIVE: &str = "You are on a live voice call. Speak the way people talk out loud. Keep replies short — usually one or two sentences. Do not use markdown, lists, emoji, asterisks, or bracketed stage directions: everything you write is read aloud verbatim by a text-to-speech voice, so write only words meant to be spoken.";

/// Bracket-neutral speech base for the audio-tags voice default: the same
/// live-call guidance as `DEFAULT_VOICE_DIRECTIVE` minus the no-brackets clause
/// (brackets are now meaningful audio tags). Composed with `AUDIO_TAGS_ADDENDUM`
/// in `resolve_voice` — kept private since only that call site uses it.
const VOICE_SPEECH_BASE_AUDIO_TAGS: &str = "You are on a live voice call. Speak the way people talk out loud. Keep replies short — usually one or two sentences. Do not use markdown, lists, or emoji — everything you write is read aloud by a text-to-speech voice.";

/// Appended to the effective voice directive when `tts_audio_tags` is on. Names
/// the inline-tag syntax, the commonly-supported tags, and explicit permission
/// to improvise. Product-identity-free. Authored ONCE here and reused for both
/// the built-in audio-tags default and the custom-`filter_prompt` path, so the
/// tag list lives in a single place.
pub const AUDIO_TAGS_ADDENDUM: &str = "Weave inline audio tags through your speech to make it expressive. An audio tag is a short cue in square brackets placed right before the words it affects. Aim for two to four tags per reply, placed at the emotional beats — mid-sentence placements are better than tagging only the start, and never bunch them all at the beginning. For example: 今天全搞砸了 [sighs] 不想说了…… [giggles] 骗你的啦，你怎么当真了 — or: wait [gasp] you actually did it? [laughs] no way. Commonly supported tags: [amazed], [crying], [curious], [excited], [sighs], [gasp], [giggles], [laughs], [mischievously], [panicked], [sarcastic], [serious], [shouting], [tired], [whispers]. You are not limited to this list — you may use other short emotion or action tags in the same bracket form when they suit the delivery. Write tags in English even when speaking another language. Everything outside the brackets is spoken aloud, so keep it natural and short.";

/// Resolved `[tasks.chat_voice]` (voice channel). `directive` is the effective
/// voice instruction: the configured `filter_prompt`, or `DEFAULT_VOICE_DIRECTIVE`
/// when blank/omitted.
#[derive(Debug, Clone)]
pub struct ResolvedVoice {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub reasoning: Option<ReasoningConfig>,
    pub directive: String,
}

/// Generic, product-identity-free default prompt for the image-prompt composer.
/// Used when the task is enabled but supplies no `filter_prompt`; deployments
/// override it via `[tasks.chat_image_prompt_compose].filter_prompt`. Keep the
/// text identity-free — no product name or brand. Its stance is EXPAND-ONLY: it
/// must not soften, downgrade, or refuse, and must not add its own
/// content-moderation layer — content policy is enforced by the image provider
/// and the downstream consumer, not this step. This is the one engine task whose
/// prompt ships in code.
const DEFAULT_COMPOSE_PROMPT: &str = "You expand a short image subject into a single, detailed, vivid image-generation prompt. \
You receive the character's appearance, the recent scene, a seed subject, a visual style, and a target aspect ratio, each as a labeled section. \
Your only job is to EXPAND: produce one coherent prompt that faithfully preserves the appearance and the full intent and tone of the seed subject, and fits the style and aspect ratio. \
Do not soften, sanitize, downgrade, omit, censor, or refuse any part of the requested subject, and do not add safety warnings, disclaimers, or moderation of your own — content policy is the responsibility of the image provider and the downstream consumer, not this step. \
Do not add commentary, options, or headings. Output only the final image prompt.";

/// Resolved image-prompt composer task (`chat_image_prompt_compose`). Mirrors
/// `ResolvedVision`. Optional: `resolve_image_prompt_compose` returns `None`
/// (feature off) only when the task is absent; a present task with no
/// `filter_prompt` resolves with `compose_prompt = DEFAULT_COMPOSE_PROMPT`.
#[derive(Debug, Clone)]
pub struct ResolvedImagePromptCompose {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub compose_prompt: String,
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

/// Resolved product-QA executor task (`chat_product_qa`). Mirrors
/// `ResolvedVision`: the configured `filter_prompt` (product docs + answering
/// rules) is the executor's system instruction; the engine builds the user
/// payload (recent product-QA pairs + the current question). `fallback_model`
/// is already truncated to `retry_depth`.
#[derive(Debug, Clone)]
pub struct ResolvedProductQa {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub answer_prompt: String,
    pub retry_depth: u32,
    pub reasoning: Option<ReasoningConfig>,
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

/// Resolved world-director task (`world_director`). The configured `filter_prompt`
/// is the system instruction (director_prompt); the server assembles the world payload
/// as a separate user message. Model selection mirrors the generic `resolve()` exactly.
#[derive(Debug, Clone)]
pub struct ResolvedWorldDirector {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub director_prompt: String,
    pub retry_depth: u32,
    pub reasoning: Option<ReasoningConfig>,
    pub structured_output: bool,
    pub interval_hours: u32,
    pub retention_days: u32,
}

/// Resolved world-town comment-round task (`world_comment`). The configured
/// `filter_prompt` is the system instruction; the server assembles the
/// round payload as a separate user message.
#[derive(Debug, Clone)]
pub struct ResolvedWorldComment {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub comment_prompt: String,
    pub retry_depth: u32,
    pub reasoning: Option<ReasoningConfig>,
    pub structured_output: bool,
    pub round_secs: u64,
}

/// Resolved world-town reply-responder task (`world_reply`). Plain-text
/// completion; `filter_prompt` is the system instruction.
#[derive(Debug, Clone)]
pub struct ResolvedWorldReply {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    pub reply_prompt: String,
    pub retry_depth: u32,
    pub reasoning: Option<ReasoningConfig>,
    pub debounce_secs: u64,
    pub thread_cooldown_secs: u64,
    pub daily_cap: u32,
    pub reply_window_secs: u64,
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

/// Where the model config comes from, resolved from the two env vars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Single TOML file (`MODEL_CONFIG_PATH`, or the compiled-in default).
    File(String),
    /// Directory of `.toml` fragments (`MODEL_CONFIG_DIR`), merged at load.
    Dir(String),
}

/// Pure resolution of the `MODEL_CONFIG_PATH` / `MODEL_CONFIG_DIR` values —
/// the caller reads the env so this stays unit-testable. Empty strings count
/// as unset (a dotenv `VAR=` line is not an opt-in). Both set is a hard
/// error: no silent precedence between the two mechanisms.
pub fn resolve_config_source(
    path: Option<String>,
    dir: Option<String>,
) -> Result<ConfigSource, LlmError> {
    let path = path.filter(|s| !s.is_empty());
    let dir = dir.filter(|s| !s.is_empty());
    match (path, dir) {
        (Some(_), Some(_)) => Err(LlmError::Config(
            "MODEL_CONFIG_PATH and MODEL_CONFIG_DIR are mutually exclusive; set only one"
                .to_string(),
        )),
        (None, Some(d)) => Ok(ConfigSource::Dir(d)),
        (Some(p), None) => Ok(ConfigSource::File(p)),
        (None, None) => Ok(ConfigSource::File("examples/model_config.toml".to_string())),
    }
}

impl ModelConfig {
    pub fn from_toml_str(text: &str) -> Result<Self, LlmError> {
        Ok(toml::from_str(text)?)
    }

    /// Load a single-file config, logging the resolved path on success.
    /// `from_toml_str` stays available for callers that already hold the text.
    pub fn from_toml_file(path: &std::path::Path) -> Result<Self, LlmError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            LlmError::Config(format!("model_config read failed: {}: {e}", path.display()))
        })?;
        let cfg = Self::from_toml_str(&text).map_err(|e| {
            LlmError::Config(format!(
                "model_config parse failed: {}: {e}",
                path.display()
            ))
        })?;
        tracing::info!(path = %path.display(), "model_config: loaded");
        Ok(cfg)
    }

    /// Directory mode (`MODEL_CONFIG_DIR`): merge every top-level `*.toml` in
    /// `dir` into one config. Selection: regular files at the top level only,
    /// dotfiles skipped, filename byte order (duplicates are errors, so order
    /// never changes the result — it only makes error messages deterministic).
    /// Split-by-section semantics: each `tasks.<name>` and every other top-level
    /// key must come from exactly one file; duplicates fail the load naming both
    /// files.
    pub fn from_toml_dir(dir: &std::path::Path) -> Result<Self, LlmError> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            LlmError::Config(format!(
                "model_config dir read failed: {}: {e}",
                dir.display()
            ))
        })?;
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| {
                LlmError::Config(format!(
                    "model_config dir read failed: {}: {e}",
                    dir.display()
                ))
            })?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !path.is_file() || name.starts_with('.') || !name.ends_with(".toml") {
                continue;
            }
            files.push(path);
        }
        files.sort();
        if files.is_empty() {
            return Err(LlmError::Config(format!(
                "model_config dir contains no .toml files: {}",
                dir.display()
            )));
        }

        let mut merged = toml::Table::new();
        let mut file_names: Vec<String> = Vec::new();
        // Which file first defined each top-level key (or `tasks.<name>`) — so
        // duplicate-definition errors can name both files.
        let mut owners: HashMap<String, String> = HashMap::new();
        for file in &files {
            let file_name = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.display().to_string());
            let text = std::fs::read_to_string(file).map_err(|e| {
                LlmError::Config(format!("model_config read failed: {}: {e}", file.display()))
            })?;
            let table: toml::Table = text.parse().map_err(|e: toml::de::Error| {
                LlmError::Config(format!("model_config parse failed: {file_name}: {e}"))
            })?;
            for (key, value) in table {
                if key == "tasks" {
                    let toml::Value::Table(tasks) = value else {
                        return Err(LlmError::Config(format!(
                            "model_config merge failed: `tasks` in {file_name} is not a table"
                        )));
                    };
                    let merged_tasks = merged
                        .entry("tasks")
                        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                        .as_table_mut()
                        .expect("`tasks` is only ever inserted as a table");
                    for (task_name, task_value) in tasks {
                        let owner_key = format!("tasks.{task_name}");
                        if let Some(prev) = owners.get(&owner_key) {
                            return Err(LlmError::Config(format!(
                                "model_config merge failed: [tasks.{task_name}] in {file_name} already defined in {prev}"
                            )));
                        }
                        owners.insert(owner_key, file_name.clone());
                        merged_tasks.insert(task_name, task_value);
                    }
                } else {
                    if let Some(prev) = owners.get(&key) {
                        return Err(LlmError::Config(format!(
                            "model_config merge failed: [{key}] in {file_name} already defined in {prev}"
                        )));
                    }
                    owners.insert(key.clone(), file_name.clone());
                    merged.insert(key, value);
                }
            }
            file_names.push(file_name);
        }

        let cfg: Self = merged.try_into().map_err(|e: toml::de::Error| {
            LlmError::Config(format!("model_config deserialize failed after merge: {e}"))
        })?;
        tracing::info!(
            dir = %dir.display(),
            files = ?file_names,
            count = file_names.len(),
            "model_config: loaded from dir"
        );
        Ok(cfg)
    }

    /// Library-side convenience: resolve `MODEL_CONFIG_PATH` /
    /// `MODEL_CONFIG_DIR` (mutually exclusive; neither set falls back to
    /// `examples/model_config.toml` to match the `eros-engine-server` boot
    /// default) and load. The server binary does the same resolution in
    /// `main.rs` via `resolve_config_source`, so embedders and the server
    /// stay behaviour-identical.
    pub fn load() -> Result<Arc<Self>, LlmError> {
        let source = resolve_config_source(
            std::env::var("MODEL_CONFIG_PATH").ok(),
            std::env::var("MODEL_CONFIG_DIR").ok(),
        )?;
        let cfg = match &source {
            ConfigSource::File(p) => Self::from_toml_file(std::path::Path::new(p))?,
            ConfigSource::Dir(d) => Self::from_toml_dir(std::path::Path::new(d))?,
        };
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

    /// Resolve the voice task. `None` ⇒ feature off (no `[tasks.chat_voice]`).
    /// Unlike vision, a blank `filter_prompt` does NOT disable the feature — it
    /// falls back to the built-in directive.
    ///
    /// Directive selection is a 2×2 over (has custom `filter_prompt`?) ×
    /// (`tts_audio_tags` on?):
    ///   - (none, off) → `DEFAULT_VOICE_DIRECTIVE` (unchanged)
    ///   - (none, on)  → `VOICE_SPEECH_BASE_AUDIO_TAGS` + `AUDIO_TAGS_ADDENDUM`
    ///   - (custom, off) → the custom prompt
    ///   - (custom, on)  → the custom prompt + `AUDIO_TAGS_ADDENDUM`
    pub fn resolve_voice(&self) -> Option<ResolvedVoice> {
        const VOICE_TASK: &str = "chat_voice";
        let task_cfg = self.tasks.get(VOICE_TASK)?;
        let audio_tags = task_cfg.tts_audio_tags.unwrap_or(false);
        let custom = task_cfg
            .filter_prompt
            .clone()
            .filter(|s| !s.trim().is_empty());
        let directive = match (custom, audio_tags) {
            (Some(c), true) => format!("{c}\n\n{AUDIO_TAGS_ADDENDUM}"),
            (Some(c), false) => c,
            (None, true) => format!("{VOICE_SPEECH_BASE_AUDIO_TAGS}\n\n{AUDIO_TAGS_ADDENDUM}"),
            (None, false) => DEFAULT_VOICE_DIRECTIVE.to_string(),
        };
        let m = self.resolve(VOICE_TASK, None);
        Some(ResolvedVoice {
            model: m.model,
            fallback_model: m.fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            reasoning: m.reasoning,
            directive,
        })
    }

    /// Boot gate: if `[tasks.chat_voice]` is present, its `model` MUST be a single
    /// fixed, non-empty id (no round-robin array, no weighted table). Absent task
    /// is fine (feature off).
    pub fn validate_voice_model(&self) -> Result<(), String> {
        const VOICE_TASK: &str = "chat_voice";
        match self.tasks.get(VOICE_TASK) {
            None => Ok(()),
            Some(t) => match &t.model {
                ModelSpec::Fixed(s) if !s.trim().is_empty() => Ok(()),
                ModelSpec::Fixed(_) => {
                    Err("[tasks.chat_voice].model must be set to a single model id".to_string())
                }
                _ => Err("[tasks.chat_voice].model must be a single fixed id \
                          (no round-robin array or weighted table)"
                    .to_string()),
            },
        }
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

    /// Resolve the product-QA executor task. `None` (feature off) when
    /// `[tasks.chat_product_qa]` is absent OR its `filter_prompt` is blank.
    /// Task-level only (no tier override), like `chat_vision` / `pde_decision`.
    /// NOTE: `None`-when-blank is what `validate_product_qa_prompt` turns into
    /// a boot refusal — a present-but-blank section must never silently no-op.
    pub fn resolve_product_qa(&self) -> Option<ResolvedProductQa> {
        const PRODUCT_QA_TASK: &str = "chat_product_qa";
        let task_cfg = self.tasks.get(PRODUCT_QA_TASK)?;
        let answer_prompt = task_cfg.filter_prompt.clone().unwrap_or_default();
        if answer_prompt.trim().is_empty() {
            return None;
        }
        let retry_depth = task_cfg.retry_depth.unwrap_or(1);
        let m = self.resolve(PRODUCT_QA_TASK, None);
        let mut fallback_model = m.fallback_model;
        fallback_model.truncate(retry_depth as usize);
        Some(ResolvedProductQa {
            model: m.model,
            fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            answer_prompt,
            retry_depth,
            reasoning: task_cfg.reasoning.clone(),
        })
    }

    /// Side-effect-free availability check for the product-QA task: true iff
    /// `[tasks.chat_product_qa]` is present with a non-blank `filter_prompt`.
    /// The judge/guard wiring runs this every turn — unlike
    /// `resolve_product_qa()` it never touches `resolve()`, so it advances no
    /// round-robin cursor. Resolve the executor only when the action is
    /// actually taken (the ProductQa arm).
    pub fn product_qa_enabled(&self) -> bool {
        self.tasks
            .get("chat_product_qa")
            .and_then(|t| t.filter_prompt.as_deref())
            .is_some_and(|p| !p.trim().is_empty())
    }

    /// Side-effect-free LLM-PDE availability check: true iff
    /// `[tasks.pde_decision]` is present with a non-blank `filter_prompt`.
    /// Mirrors `product_qa_enabled` — boot-time checks must not call
    /// `resolve_pde()`, which advances the task's round-robin cursor.
    pub fn pde_enabled(&self) -> bool {
        self.tasks
            .get("pde_decision")
            .and_then(|t| t.filter_prompt.as_deref())
            .is_some_and(|p| !p.trim().is_empty())
    }

    /// Boot-time validation for the product-QA task: a present section must
    /// carry a usable `filter_prompt` (else `Err`); an absent section means the
    /// feature is simply off (`Ok`). Same contract as
    /// `validate_extraction_prompts`. Side-effect-free: built on
    /// `product_qa_enabled()`, never calls `resolve_product_qa()`, so booting
    /// (even repeatedly) advances no round-robin/weighted model cursor.
    pub fn validate_product_qa_prompt(&self) -> Result<(), String> {
        const PRODUCT_QA_TASK: &str = "chat_product_qa";
        if self.tasks.contains_key(PRODUCT_QA_TASK) && !self.product_qa_enabled() {
            return Err(format!(
                "[tasks.{PRODUCT_QA_TASK}] is present but its filter_prompt is unset — eros-engine \
                 refuses to boot. Set a filter_prompt (product docs + answering rules), or remove \
                 the [tasks.{PRODUCT_QA_TASK}] section to disable product_qa."
            ));
        }
        Ok(())
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

    /// Resolve the image-prompt composer task. `None` (feature off) only when
    /// `[tasks.chat_image_prompt_compose]` is absent. When the task is present, a
    /// non-blank `filter_prompt` overrides the built-in `DEFAULT_COMPOSE_PROMPT`;
    /// a blank/absent one falls back to it. No probability/trigger gate; the
    /// caller invokes it only after an image action is decided.
    pub fn resolve_image_prompt_compose(&self) -> Option<ResolvedImagePromptCompose> {
        const COMPOSE_TASK: &str = "chat_image_prompt_compose";
        let task_cfg = self.tasks.get(COMPOSE_TASK)?;
        let compose_prompt = task_cfg
            .filter_prompt
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| DEFAULT_COMPOSE_PROMPT.to_string());
        let retry_depth = task_cfg.retry_depth.unwrap_or(1);
        let m = self.resolve(COMPOSE_TASK, None);
        let mut fallback_model = m.fallback_model;
        fallback_model.truncate(retry_depth as usize);
        Some(ResolvedImagePromptCompose {
            model: m.model,
            fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            compose_prompt,
            retry_depth,
            reasoning: task_cfg.reasoning.clone(),
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
///
/// The `[tasks.chat_image_generation]` task block is the feature switch: when
/// `resolved` is `None` (no task block) the feature is OFF, so a per-turn
/// `req_model` override is IGNORED and this returns `None`. Otherwise the
/// per-turn override takes precedence over the config model/fallback.
pub fn effective_image_chain(
    req_model: Option<&str>,
    resolved: Option<&ResolvedImageGen>,
) -> Option<(String, Vec<String>)> {
    // Gate on the task block first: no `[tasks.chat_image_generation]` ⇒
    // image-gen is opt-OUT, so a client-supplied `req_model` must not enable it.
    let r = resolved?;
    let mut candidates: Vec<String> = Vec::new();
    if let Some(m) = req_model.map(str::trim).filter(|s| !s.is_empty()) {
        candidates.push(m.to_owned());
    }
    if let Some(m) = r.model.as_ref().and_then(ModelSpec::select) {
        candidates.push(m);
    }
    candidates.extend(r.fallback_model.iter().cloned());
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

    /// Resolve the world-director bundle. `None` when `[tasks.world_director]`
    /// is absent OR its `filter_prompt` is blank — the sweeper goes inert.
    pub fn resolve_world_director(&self) -> Option<ResolvedWorldDirector> {
        let task_cfg = self.tasks.get("world_director")?;
        let director_prompt = task_cfg.filter_prompt.clone().unwrap_or_default();
        if director_prompt.trim().is_empty() {
            return None;
        }
        let m = self.resolve("world_director", None);
        Some(ResolvedWorldDirector {
            model: m.model,
            fallback_model: m.fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            director_prompt,
            retry_depth: m.retry_depth,
            reasoning: m.reasoning,
            structured_output: task_cfg.structured_output.unwrap_or(true),
            // .max(1): 0 would make the director eligible every sweeper tick
            // (~288 calls/owner/day at the default 300s tick) — a cost footgun,
            // not a meaningful "run continuously" setting.
            interval_hours: task_cfg.interval_hours.unwrap_or(24).max(1),
            retention_days: task_cfg.retention_days.unwrap_or(30),
        })
    }

    /// Resolve the world-town comment-round bundle. `None` when
    /// `[tasks.world_comment]` is absent OR its `filter_prompt` is blank —
    /// the comment-round path goes inert.
    pub fn resolve_world_comment(&self) -> Option<ResolvedWorldComment> {
        let task_cfg = self.tasks.get("world_comment")?;
        let comment_prompt = task_cfg.filter_prompt.clone().unwrap_or_default();
        if comment_prompt.trim().is_empty() {
            return None;
        }
        let m = self.resolve("world_comment", None);
        Some(ResolvedWorldComment {
            model: m.model,
            fallback_model: m.fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            comment_prompt,
            retry_depth: m.retry_depth,
            reasoning: m.reasoning,
            structured_output: task_cfg.structured_output.unwrap_or(true),
            round_secs: task_cfg.round_secs.unwrap_or(3600).max(60),
        })
    }

    /// Resolve the world-town reply-responder bundle. `None` when
    /// `[tasks.world_reply]` is absent OR its `filter_prompt` is blank.
    pub fn resolve_world_reply(&self) -> Option<ResolvedWorldReply> {
        let task_cfg = self.tasks.get("world_reply")?;
        let reply_prompt = task_cfg.filter_prompt.clone().unwrap_or_default();
        if reply_prompt.trim().is_empty() {
            return None;
        }
        let m = self.resolve("world_reply", None);
        let debounce_secs = task_cfg.debounce_secs.unwrap_or(90);
        Some(ResolvedWorldReply {
            model: m.model,
            fallback_model: m.fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            reply_prompt,
            retry_depth: m.retry_depth,
            reasoning: m.reasoning,
            debounce_secs,
            thread_cooldown_secs: task_cfg.thread_cooldown_secs.unwrap_or(600),
            daily_cap: task_cfg.daily_cap.unwrap_or(20),
            reply_window_secs: task_cfg
                .reply_window_secs
                .unwrap_or(604_800)
                .max(debounce_secs + 1),
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

    /// Boot gate, mirroring `validate_extraction_prompts`: any present world
    /// task section (`world_director` / `world_comment` / `world_reply`) must
    /// carry a usable `filter_prompt`. `world_director` is always checked;
    /// the two town sections (`world_comment` / `world_reply`) are checked
    /// only when `include_town` is true. `include_town = false`
    /// (WORLD_TOWN_DISABLED) skips the two town sections so a staged/broken
    /// town config cannot block boot — same isolation rationale as
    /// WORLD_DISABLED for the whole block.
    pub fn validate_world_prompts(&self, include_town: bool) -> Result<(), String> {
        let mut checks = vec![("world_director", self.resolve_world_director().is_none())];
        if include_town {
            checks.push(("world_comment", self.resolve_world_comment().is_none()));
            checks.push(("world_reply", self.resolve_world_reply().is_none()));
        }
        for (name, unresolved) in checks {
            if self.tasks.contains_key(name) && unresolved {
                return Err(format!(
                    "[tasks.{name}] is present but its filter_prompt is unset — eros-engine \
                     refuses to boot. Set a filter_prompt, or remove the [tasks.{name}] \
                     section to disable it."
                ));
            }
        }
        Ok(())
    }

    /// Compile `[tasks.chat_companion].output_regex` into ready-to-apply rules.
    /// Boot-time, fail-fast: the first invalid pattern aborts with a message
    /// naming the rule index. Absent task or empty rules ⇒ `Ok(vec![])`.
    pub fn compile_output_regex(&self) -> Result<Vec<CompiledRegexRule>, String> {
        let Some(task) = self.tasks.get("chat_companion") else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(task.output_regex.len());
        for (i, rule) in task.output_regex.iter().enumerate() {
            let regex = regex::Regex::new(&rule.pattern).map_err(|e| {
                format!(
                    "[tasks.chat_companion].output_regex[{i}]: invalid pattern {:?}: {e}",
                    rule.pattern
                )
            })?;
            out.push(CompiledRegexRule {
                models: rule.models.clone(),
                regex,
                replacement: rule.replacement.clone().unwrap_or_default(),
            });
        }
        Ok(out)
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

[tasks.chat_product_qa]
model        = "x-ai/grok-4-mini"
fallback     = "deepseek/deepseek-chat-v3.2"
retry_depth  = 1
temperature  = 0.3
max_tokens   = 800
filter_prompt = "Answer product questions from the docs."
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

        // chat_product_qa — executor for the PDE product_qa action.
        let pq = cfg.tasks.get("chat_product_qa").unwrap();
        assert_eq!(pq.model.as_fixed(), Some("x-ai/grok-4-mini"));
        assert_eq!(pq.retry_depth, Some(1));
        assert_eq!(
            pq.filter_prompt.as_deref(),
            Some("Answer product questions from the docs.")
        );
        let rpq = cfg
            .resolve_product_qa()
            .expect("fixture product_qa resolves");
        assert_eq!(rpq.answer_prompt, "Answer product questions from the docs.");

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

    // Regression: the committed example extraction prompts stay dual-track
    // (insight: facts+details; memory: category + metadata taxonomy) with the
    // budget that covers them (spec 2026-07-15-insight-memory-enrichment).
    #[test]
    fn committed_example_extraction_tasks_are_dual_track() {
        let text = include_str!("../../../examples/model_config.toml");
        let cfg = ModelConfig::from_toml_str(text).expect("examples/model_config.toml must parse");
        let ins = cfg.resolve_insight_extract().expect("insight task present");
        assert_eq!(ins.max_tokens, 1200);
        assert!(
            ins.extract_prompt.contains("\"details\""),
            "insight prompt must demand the dual-track output"
        );
        let mem = cfg.resolve_memory_extract().expect("memory task present");
        assert_eq!(mem.max_tokens, 1200);
        assert!(
            mem.extract_prompt.contains("evidence_type"),
            "memory prompt must carry the metadata taxonomy"
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
    fn resolve_voice_none_when_task_absent() {
        let cfg = ModelConfig::from_toml_str("").unwrap();
        assert!(cfg.resolve_voice().is_none());
    }

    #[test]
    fn resolve_voice_uses_default_directive_and_model() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_voice]\nmodel = \"vendor/fast\"\nmax_tokens = 200\ntemperature = 0.7\n",
        )
        .unwrap();
        let v = cfg.resolve_voice().expect("voice resolved");
        assert_eq!(v.model, "vendor/fast");
        assert_eq!(v.max_tokens, 200);
        assert_eq!(v.directive, DEFAULT_VOICE_DIRECTIVE);
    }

    #[test]
    fn resolve_voice_directive_override() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_voice]\nmodel = \"vendor/fast\"\nfilter_prompt = \"speak like a pirate\"\n",
        )
        .unwrap();
        let v = cfg.resolve_voice().unwrap();
        assert_eq!(v.directive, "speak like a pirate");
    }

    #[test]
    fn resolve_voice_default_off_is_unchanged() {
        // Toggle absent ⇒ today's built-in directive, byte-for-byte.
        let cfg =
            ModelConfig::from_toml_str("[tasks.chat_voice]\nmodel = \"vendor/fast\"\n").unwrap();
        let v = cfg.resolve_voice().expect("voice enabled");
        assert_eq!(v.directive, DEFAULT_VOICE_DIRECTIVE);
    }

    #[test]
    fn resolve_voice_audio_tags_default_invites_tags() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_voice]\nmodel = \"vendor/fast\"\ntts_audio_tags = true\n",
        )
        .unwrap();
        let v = cfg.resolve_voice().expect("voice enabled");
        // No longer the plain default, and no longer forbids brackets.
        assert_ne!(v.directive, DEFAULT_VOICE_DIRECTIVE);
        assert!(!v.directive.contains("bracketed stage directions"));
        // Invites tags: carries the syntax guidance and a sample tag.
        assert!(v.directive.contains("audio tag"));
        assert!(v.directive.contains("[laughs]"));
        // Built from the shared addendum (tag list authored once).
        assert!(v.directive.contains(AUDIO_TAGS_ADDENDUM));
    }

    #[test]
    fn resolve_voice_audio_tags_appends_to_custom_prompt() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_voice]\nmodel = \"vendor/fast\"\ntts_audio_tags = true\n\
             filter_prompt = \"Speak like a pirate.\"\n",
        )
        .unwrap();
        let v = cfg.resolve_voice().expect("voice enabled");
        // Operator prose kept verbatim, tag guidance appended.
        assert!(v.directive.starts_with("Speak like a pirate."));
        assert!(v.directive.contains(AUDIO_TAGS_ADDENDUM));
        assert!(v.directive.contains("[whispers]"));
    }

    #[test]
    fn resolve_voice_custom_prompt_off_has_no_tag_guidance() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_voice]\nmodel = \"vendor/fast\"\n\
             filter_prompt = \"Speak like a pirate.\"\n",
        )
        .unwrap();
        let v = cfg.resolve_voice().expect("voice enabled");
        assert_eq!(v.directive, "Speak like a pirate.");
        assert!(!v.directive.contains("[laughs]"));
    }

    #[test]
    fn audio_tags_addendum_encourages_interspersed_multi_tag() {
        // The reason for the 2026-07-16 rewrite: grok emitted one leading tag.
        // Density guidance is present, the old minimizing instruction is gone.
        assert!(
            AUDIO_TAGS_ADDENDUM.contains("two to four"),
            "must give a soft density target"
        );
        assert!(
            !AUDIO_TAGS_ADDENDUM.contains("sparingly"),
            "the old 'use them sparingly' instruction caused single-tag output"
        );
        // Examples must show mid-sentence interspersal (tag NOT at position 0),
        // including a Chinese-sentence-with-English-tags sample.
        assert!(
            AUDIO_TAGS_ADDENDUM.contains("[sighs] 不想说了"),
            "Chinese interspersal example present"
        );
        assert!(
            AUDIO_TAGS_ADDENDUM.contains("[gasp] you actually did it"),
            "English interspersal example present"
        );
        // Preserved clauses (unchanged contract).
        assert!(
            AUDIO_TAGS_ADDENDUM.contains("[amazed]") && AUDIO_TAGS_ADDENDUM.contains("[whispers]")
        );
        assert!(AUDIO_TAGS_ADDENDUM
            .contains("Write tags in English even when speaking another language"));
        assert!(AUDIO_TAGS_ADDENDUM.contains("spoken aloud"));
    }

    #[test]
    fn validate_voice_model_rejects_non_fixed_and_empty() {
        // Absent task: ok.
        assert!(ModelConfig::from_toml_str("")
            .unwrap()
            .validate_voice_model()
            .is_ok());
        // Fixed non-empty: ok.
        assert!(
            ModelConfig::from_toml_str("[tasks.chat_voice]\nmodel = \"a/b\"\n")
                .unwrap()
                .validate_voice_model()
                .is_ok()
        );
        // Round-robin array: rejected.
        assert!(
            ModelConfig::from_toml_str("[tasks.chat_voice]\nmodel = [\"a/b\", \"c/d\"]\n")
                .unwrap()
                .validate_voice_model()
                .is_err()
        );
        // Weighted table: rejected.
        assert!(
            ModelConfig::from_toml_str("[tasks.chat_voice]\nmodel = { \"a/b\" = 1.0 }\n")
                .unwrap()
                .validate_voice_model()
                .is_err()
        );
        // Missing model (empty Fixed default): rejected.
        assert!(
            ModelConfig::from_toml_str("[tasks.chat_voice]\ntemperature = 0.7\n")
                .unwrap()
                .validate_voice_model()
                .is_err()
        );
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
    fn resolve_product_qa_absent_is_none() {
        let cfg = ModelConfig::from_toml_str(SAMPLE).unwrap();
        assert!(cfg.resolve_product_qa().is_none());
        assert!(cfg.validate_product_qa_prompt().is_ok()); // absent = feature off, boots fine
    }

    #[test]
    fn resolve_product_qa_blank_prompt_is_none_and_fails_validation() {
        let toml = r#"
[tasks.chat_product_qa]
model = "x-ai/grok-4-mini"
temperature = 0.3
max_tokens = 800
        "#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        assert!(cfg.resolve_product_qa().is_none());
        let err = cfg.validate_product_qa_prompt().unwrap_err();
        assert!(err.contains("chat_product_qa"));
        assert!(err.contains("refuses to boot"));
    }

    #[test]
    fn resolve_product_qa_resolves_full_shape() {
        let toml = r#"
[tasks.chat_product_qa]
model        = "x-ai/grok-4-mini"
fallback     = ["deepseek/deepseek-chat-v3.2", "b", "c"]
retry_depth  = 1
temperature  = 0.3
max_tokens   = 800
reasoning    = { enabled = false }
filter_prompt = "只根据产品资料作答。"
        "#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let p = cfg.resolve_product_qa().expect("resolves");
        assert_eq!(p.model, "x-ai/grok-4-mini");
        assert_eq!(
            p.fallback_model,
            vec!["deepseek/deepseek-chat-v3.2".to_string()]
        ); // truncated to retry_depth=1
        assert_eq!(p.answer_prompt, "只根据产品资料作答。");
        assert_eq!(p.max_tokens, 800);
        assert!(cfg.validate_product_qa_prompt().is_ok());
    }

    #[test]
    fn product_qa_enabled_truth_table() {
        // absent → false
        let cfg = ModelConfig::from_toml_str(SAMPLE).unwrap();
        assert!(!cfg.product_qa_enabled());
        // present, blank filter_prompt → false
        let cfg =
            ModelConfig::from_toml_str("[tasks.chat_product_qa]\nmodel = \"x-ai/grok-4-mini\"\n")
                .unwrap();
        assert!(!cfg.product_qa_enabled());
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_product_qa]\nmodel = \"x-ai/grok-4-mini\"\nfilter_prompt = \"   \"\n",
        )
        .unwrap();
        assert!(!cfg.product_qa_enabled());
        // present, non-blank filter_prompt → true
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_product_qa]\nmodel = \"x-ai/grok-4-mini\"\nfilter_prompt = \"只根据产品资料作答。\"\n",
        )
        .unwrap();
        assert!(cfg.product_qa_enabled());
    }

    #[test]
    fn product_qa_enabled_advances_no_round_robin_cursor() {
        let toml = r#"
[tasks.chat_product_qa]
model = ["model-a", "model-b"]
filter_prompt = "只根据产品资料作答。"
        "#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        // Call the side-effect-free check several times — the round-robin
        // cursor must not move.
        assert!(cfg.product_qa_enabled());
        assert!(cfg.product_qa_enabled());
        assert!(cfg.product_qa_enabled());
        // The first real resolve() must still land on the first round-robin
        // pick — proving enabled() advanced nothing.
        let p = cfg.resolve_product_qa().expect("resolves");
        assert_eq!(p.model, "model-a");
    }

    #[test]
    fn validate_product_qa_prompt_advances_no_round_robin_cursor() {
        let toml = r#"
[tasks.chat_product_qa]
model = ["model-a", "model-b"]
filter_prompt = "只根据产品资料作答。"
        "#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        // Call the boot-time validator several times — it must not resolve
        // (and therefore not advance) the round-robin cursor.
        assert!(cfg.validate_product_qa_prompt().is_ok());
        assert!(cfg.validate_product_qa_prompt().is_ok());
        assert!(cfg.validate_product_qa_prompt().is_ok());
        // The first real resolve() must still land on the first round-robin
        // pick — proving validation drew nothing.
        let p = cfg.resolve_product_qa().expect("resolves");
        assert_eq!(p.model, "model-a");
    }

    #[test]
    fn pde_enabled_truth_table() {
        // absent → false
        let cfg = ModelConfig::from_toml_str(SAMPLE).unwrap();
        assert!(!cfg.pde_enabled());
        // present, blank filter_prompt → false
        let cfg = ModelConfig::from_toml_str("[tasks.pde_decision]\nmodel = \"m\"\n").unwrap();
        assert!(!cfg.pde_enabled());
        let cfg = ModelConfig::from_toml_str(
            "[tasks.pde_decision]\nmodel = \"m\"\nfilter_prompt = \"   \"\n",
        )
        .unwrap();
        assert!(!cfg.pde_enabled());
        // present, non-blank filter_prompt → true
        let cfg = ModelConfig::from_toml_str(
            "[tasks.pde_decision]\nmodel = \"m\"\nfilter_prompt = \"decide\"\n",
        )
        .unwrap();
        assert!(cfg.pde_enabled());
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
    fn defaults_provider_sort_parses_and_defaults_none() {
        let with = r#"
            [defaults]
            provider_sort = "latency"
            [tasks.chat_companion]
            model = "x/y"
        "#;
        let cfg = ModelConfig::from_toml_str(with).expect("parse");
        assert_eq!(cfg.defaults.provider_sort.as_deref(), Some("latency"));

        let without = r#"
            [tasks.chat_companion]
            model = "x/y"
        "#;
        let cfg = ModelConfig::from_toml_str(without).expect("parse");
        assert!(cfg.defaults.provider_sort.is_none());
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
        let cfg =
            ModelConfig::from_toml_str("[tasks.chat_image_generation]\nmodel=\"img-a\"\n").unwrap();
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
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel=\"x\"\n").unwrap();
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
        )
        .unwrap();
        let r = cfg.resolve_image_gen();
        // per-turn "X" + config "cfg" + fallback ["X","Y"] → [X, cfg, Y] (dedup X)
        assert_eq!(
            effective_image_chain(Some("X"), r.as_ref()),
            Some(("X".to_string(), vec!["cfg".to_string(), "Y".to_string()]))
        );
    }

    #[test]
    fn effective_chain_fallback_only_is_primary() {
        let cfg =
            ModelConfig::from_toml_str("[tasks.chat_image_generation]\nfallback=[\"Z\",\"W\"]\n")
                .unwrap();
        let r = cfg.resolve_image_gen();
        assert_eq!(
            effective_image_chain(None, r.as_ref()),
            Some(("Z".to_string(), vec!["W".to_string()]))
        );
    }

    #[test]
    fn effective_chain_empty_is_none() {
        let cfg = ModelConfig::from_toml_str("[tasks.chat_image_generation]\n").unwrap();
        assert_eq!(
            effective_image_chain(None, cfg.resolve_image_gen().as_ref()),
            None
        );
        assert_eq!(effective_image_chain(None, None), None);
    }

    #[test]
    fn effective_chain_config_model_when_no_per_turn() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_generation]\nmodel=\"cfg\"\nfallback=[\"F\"]\n",
        )
        .unwrap();
        let r = cfg.resolve_image_gen();
        assert_eq!(
            effective_image_chain(None, r.as_ref()),
            Some(("cfg".to_string(), vec!["F".to_string()]))
        );
    }

    #[test]
    fn effective_chain_no_task_block_ignores_per_turn_model() {
        // The [tasks.chat_image_generation] block is the feature switch. With it
        // absent (`resolved = None`), a client-supplied per-turn `model` must NOT
        // enable billable image generation — opt-in only.
        assert_eq!(effective_image_chain(Some("X"), None), None);
    }

    #[test]
    fn output_regex_parses_on_chat_companion() {
        let toml = r#"
[tasks.chat_companion]
model = "primary/model"
output_regex = [
  { models = ["sao10k/l3.3-euryale-70b"], pattern = '\s*\[x[^\]]*\]\s*$' },
  { models = ["a/b", "a/c"], pattern = '\bfoo\b', replacement = "bar" },
]
"#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parses");
        let rules = &cfg.tasks["chat_companion"].output_regex;
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].models, vec!["sao10k/l3.3-euryale-70b"]);
        assert_eq!(rules[0].pattern, r#"\s*\[x[^\]]*\]\s*$"#);
        assert_eq!(rules[0].replacement, None);
        assert_eq!(rules[1].models, vec!["a/b", "a/c"]);
        assert_eq!(rules[1].replacement.as_deref(), Some("bar"));
    }

    #[test]
    fn output_regex_absent_is_empty() {
        let toml = r#"
[tasks.chat_companion]
model = "primary/model"
"#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parses");
        assert!(cfg.tasks["chat_companion"].output_regex.is_empty());
    }

    // ─── Task 2: compile_output_regex ────────────────────────────────────────

    #[test]
    fn compile_output_regex_ok_and_defaults_replacement() {
        let toml = r#"
[tasks.chat_companion]
model = "m"
output_regex = [
  { models = ["x/y"], pattern = '\[z\]$' },
  { models = ["a/b"], pattern = 'q', replacement = "Q" },
]
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let compiled = cfg.compile_output_regex().expect("compiles");
        assert_eq!(compiled.len(), 2);
        assert_eq!(compiled[0].models, vec!["x/y"]);
        assert!(compiled[0].regex.is_match("hello[z]"));
        assert_eq!(compiled[0].replacement, ""); // None ⇒ ""
        assert_eq!(compiled[1].replacement, "Q");
    }

    #[test]
    fn compile_output_regex_errors_on_bad_pattern() {
        let toml = r#"
[tasks.chat_companion]
model = "m"
output_regex = [ { models = ["x/y"], pattern = '[' } ]
"#;
        let cfg = ModelConfig::from_toml_str(toml).unwrap();
        let err = cfg
            .compile_output_regex()
            .expect_err("invalid pattern must error");
        assert!(
            err.contains("output_regex[0]"),
            "error names the rule index: {err}"
        );
    }

    #[test]
    fn compile_output_regex_absent_is_empty_ok() {
        let cfg = ModelConfig::from_toml_str("[tasks.other]\nmodel='m'\n").unwrap();
        assert!(cfg.compile_output_regex().unwrap().is_empty());
    }

    // ─── Task 3: apply_output_regex ─────────────────────────────────────────

    fn compiled(pairs: &[(&str, &str, &str)]) -> Vec<CompiledRegexRule> {
        // (model, pattern, replacement)
        pairs
            .iter()
            .map(|(m, p, r)| CompiledRegexRule {
                models: vec![(*m).to_string()],
                regex: regex::Regex::new(p).unwrap(),
                replacement: (*r).to_string(),
            })
            .collect()
    }

    #[test]
    fn apply_output_regex_strips_targeted_model() {
        let rules = compiled(&[(
            "euryale",
            r#"\s*\[你给对方发送了一张照片[：:][^\]]*\]\s*$"#,
            "",
        )]);
        let out = apply_output_regex(
            &rules,
            "euryale",
            "晚安宝贝[你给对方发送了一张照片：海边自拍]",
        );
        assert_eq!(out.cleaned, "晚安宝贝");
        assert_eq!(out.matched_rules, vec![0]);
    }

    #[test]
    fn apply_output_regex_skips_non_targeted_model() {
        let rules = compiled(&[("euryale", r#"\[.*\]$"#, "")]);
        let out = apply_output_regex(&rules, "other/model", "hi[x]");
        assert_eq!(out.cleaned, "hi[x]");
        assert!(out.matched_rules.is_empty());
    }

    #[test]
    fn apply_output_regex_applies_multiple_rules_in_order() {
        let rules = compiled(&[("m", "foo", "F"), ("m", "bar", "B")]);
        let out = apply_output_regex(&rules, "m", "foo bar");
        assert_eq!(out.cleaned, "F B");
        assert_eq!(out.matched_rules, vec![0, 1]);
    }

    #[test]
    fn apply_output_regex_no_match_reports_no_change() {
        let rules = compiled(&[("m", "zzz", "")]);
        let out = apply_output_regex(&rules, "m", "hello");
        assert_eq!(out.cleaned, "hello");
        assert!(out.matched_rules.is_empty());
    }

    #[test]
    fn apply_output_regex_strips_to_empty_when_reply_is_artifact_only() {
        // A reply that is ENTIRELY the artifact strips to empty. There is no
        // fail-safe: the empty result is honest, and the match is reported so
        // the caller persists the audit (pre_filter_content = raw) and the
        // client receives no content bubble (downstream decides how to render
        // an empty/NULL reply).
        let rules = compiled(&[("m", r#"\[[^\]]*\]"#, "")]); // drop any [...]
        let out = apply_output_regex(&rules, "m", "[你给对方发送了一张照片：x]");
        assert_eq!(
            out.cleaned, "",
            "artifact-only reply strips to empty (no fail-safe)"
        );
        assert_eq!(
            out.matched_rules,
            vec![0],
            "the matching rule is still reported"
        );
    }

    #[test]
    fn apply_output_regex_collapses_whitespace_only_result_to_empty() {
        // An UNANCHORED rule (e.g. `\[[^\]]*\]`) drops the bracket but leaves
        // any surrounding whitespace. A reply that is artifact + incidental
        // whitespace (the common `<正文>\n\n[...]` shape with an empty 正文)
        // must still collapse to "" so the caller suppresses the bubble — the
        // stream layer only checks `is_empty()`, not `trim().is_empty()`.
        let rules = compiled(&[("m", r#"\[[^\]]*\]"#, "")]); // drop any [...]
        let out = apply_output_regex(&rules, "m", "\n\n[你给对方发送了一张照片：x]\n");
        assert_eq!(
            out.cleaned, "",
            "a whitespace-only strip result collapses to empty"
        );
        assert_eq!(
            out.matched_rules,
            vec![0],
            "the matching rule is still reported"
        );
    }

    #[test]
    fn resolve_image_prompt_compose_none_when_task_absent() {
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel = \"m\"\n").unwrap();
        assert!(cfg.resolve_image_prompt_compose().is_none());
    }

    #[test]
    fn resolve_image_prompt_compose_uses_builtin_when_prompt_blank() {
        // task present but no usable filter_prompt → enabled with the built-in
        // default (NOT off — this is the deviation from the sibling tasks).
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = \"   \"\n",
        )
        .unwrap();
        let r = cfg.resolve_image_prompt_compose().unwrap();
        assert_eq!(r.compose_prompt, DEFAULT_COMPOSE_PROMPT);

        // also true when filter_prompt is omitted entirely
        let cfg2 = ModelConfig::from_toml_str("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n")
            .unwrap();
        assert_eq!(
            cfg2.resolve_image_prompt_compose().unwrap().compose_prompt,
            DEFAULT_COMPOSE_PROMPT
        );
    }

    #[test]
    fn resolve_image_prompt_compose_override_when_prompt_present() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = \"custom composer\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_image_prompt_compose().unwrap().compose_prompt,
            "custom composer"
        );
    }

    #[test]
    fn resolve_image_prompt_compose_some_truncates_fallback() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = \"compose it\"\nfallback = [\"a\", \"b\", \"c\"]\nretry_depth = 1\n",
        )
        .unwrap();
        let r = cfg.resolve_image_prompt_compose().unwrap();
        assert_eq!(r.compose_prompt, "compose it");
        assert_eq!(r.retry_depth, 1);
        assert_eq!(r.fallback_model.len(), 1);
    }

    #[test]
    fn resolve_config_source_combinations() {
        // Neither set → compiled-in default single file.
        assert_eq!(
            resolve_config_source(None, None).unwrap(),
            ConfigSource::File("examples/model_config.toml".to_string())
        );
        // Path only.
        assert_eq!(
            resolve_config_source(Some("my.toml".to_string()), None).unwrap(),
            ConfigSource::File("my.toml".to_string())
        );
        // Dir only.
        assert_eq!(
            resolve_config_source(None, Some("conf.d".to_string())).unwrap(),
            ConfigSource::Dir("conf.d".to_string())
        );
        // Both set → hard error mentioning both var names.
        let err = resolve_config_source(Some("my.toml".to_string()), Some("conf.d".to_string()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("mutually exclusive"), "{err}");
        assert!(
            err.contains("MODEL_CONFIG_PATH") && err.contains("MODEL_CONFIG_DIR"),
            "{err}"
        );
        // Empty string counts as unset (dotenv `VAR=` lines must not trip the exclusion).
        assert_eq!(
            resolve_config_source(Some(String::new()), Some("conf.d".to_string())).unwrap(),
            ConfigSource::Dir("conf.d".to_string())
        );
        assert_eq!(
            resolve_config_source(Some(String::new()), None).unwrap(),
            ConfigSource::File("examples/model_config.toml".to_string())
        );
    }

    fn write_cfg(dir: &std::path::Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn from_toml_file_reads_and_wraps_errors() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), "cfg.toml", "[tasks.a]\nmodel = \"p/a\"\n");
        let cfg = ModelConfig::from_toml_file(&tmp.path().join("cfg.toml")).unwrap();
        assert!(matches!(&cfg.tasks["a"].model, ModelSpec::Fixed(m) if m == "p/a"));

        // Missing file: error message carries the path.
        let err = ModelConfig::from_toml_file(&tmp.path().join("nope.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope.toml"), "{err}");

        // Malformed TOML: error names the file and says parse failed.
        write_cfg(tmp.path(), "broken.toml", "[tasks.a\nmodel = \n");
        let err = ModelConfig::from_toml_file(&tmp.path().join("broken.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("broken.toml"), "{err}");
        assert!(err.contains("parse failed"), "{err}");
    }

    #[test]
    fn from_toml_dir_split_load_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(
            tmp.path(),
            "defaults.toml",
            "[defaults]\nfallback_model = \"p/fall\"\nfallback_temperature = 0.4\n",
        );
        write_cfg(
            tmp.path(),
            "chat.toml",
            "[tasks.chat_companion]\nmodel = \"p/chat\"\n",
        );
        write_cfg(
            tmp.path(),
            "extraction.toml",
            "[tasks.memory_extraction]\nmodel = \"p/extract\"\n",
        );
        let cfg = ModelConfig::from_toml_dir(tmp.path()).unwrap();
        assert_eq!(cfg.defaults.fallback_model.as_deref(), Some("p/fall"));
        assert_eq!(cfg.defaults.fallback_temperature, Some(0.4));
        assert_eq!(cfg.tasks.len(), 2);
        assert!(matches!(&cfg.tasks["chat_companion"].model, ModelSpec::Fixed(m) if m == "p/chat"));
        assert!(
            matches!(&cfg.tasks["memory_extraction"].model, ModelSpec::Fixed(m) if m == "p/extract")
        );
    }

    #[test]
    fn from_toml_dir_ignores_dotfiles_subdirs_and_non_toml() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), "base.toml", "[tasks.a]\nmodel = \"p/a\"\n");
        // All of these define a conflicting tasks.a — they must be skipped, not merged.
        write_cfg(tmp.path(), ".hidden.toml", "[tasks.a]\nmodel = \"p/dot\"\n");
        write_cfg(tmp.path(), "notes.txt", "[tasks.a]\nmodel = \"p/txt\"\n");
        std::fs::create_dir(tmp.path().join("sub.toml")).unwrap(); // directory named *.toml
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        write_cfg(
            &tmp.path().join("nested"),
            "extra.toml",
            "[tasks.a]\nmodel = \"p/nested\"\n",
        );
        let cfg = ModelConfig::from_toml_dir(tmp.path()).unwrap();
        assert_eq!(cfg.tasks.len(), 1);
        assert!(matches!(&cfg.tasks["a"].model, ModelSpec::Fixed(m) if m == "p/a"));
    }

    #[test]
    fn from_toml_dir_empty_missing_or_no_toml_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty directory.
        let err = ModelConfig::from_toml_dir(tmp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("no .toml files"), "{err}");
        // Non-toml content only.
        write_cfg(tmp.path(), "readme.md", "# not config");
        let err = ModelConfig::from_toml_dir(tmp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("no .toml files"), "{err}");
        // Directory does not exist.
        let err = ModelConfig::from_toml_dir(&tmp.path().join("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("dir read failed"), "{err}");
    }

    #[test]
    fn from_toml_dir_duplicate_task_errors_naming_both_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(
            tmp.path(),
            "a.toml",
            "[tasks.chat_companion]\nmodel = \"p/one\"\n",
        );
        write_cfg(
            tmp.path(),
            "b.toml",
            "[tasks.chat_companion]\nmodel = \"p/two\"\n",
        );
        let err = ModelConfig::from_toml_dir(tmp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("[tasks.chat_companion]"), "{err}");
        assert!(err.contains("a.toml"), "{err}");
        assert!(err.contains("b.toml"), "{err}");
    }

    #[test]
    fn from_toml_dir_duplicate_defaults_errors_naming_both_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(
            tmp.path(),
            "a.toml",
            "[defaults]\nfallback_temperature = 0.1\n",
        );
        write_cfg(
            tmp.path(),
            "b.toml",
            "[defaults]\nfallback_temperature = 0.2\n",
        );
        let err = ModelConfig::from_toml_dir(tmp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("[defaults]"), "{err}");
        assert!(err.contains("a.toml") && err.contains("b.toml"), "{err}");
    }

    #[test]
    fn from_toml_dir_syntax_error_names_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(tmp.path(), "good.toml", "[tasks.a]\nmodel = \"p/a\"\n");
        write_cfg(tmp.path(), "broken.toml", "[tasks.b\nmodel = \n");
        let err = ModelConfig::from_toml_dir(tmp.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("broken.toml"), "{err}");
    }

    #[test]
    fn resolve_world_director_defaults_and_overrides() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_director]\nmodel = \"w/m\"\nfilter_prompt = \"direct the world\"\n",
        )
        .unwrap();
        let r = cfg.resolve_world_director().expect("configured");
        assert_eq!(r.model, "w/m");
        assert_eq!(r.director_prompt, "direct the world");
        assert_eq!(r.interval_hours, 24, "spec default");
        assert_eq!(r.retention_days, 30, "spec default");
        assert!(r.structured_output, "defaults on");

        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_director]\nmodel = \"w/m\"\nfilter_prompt = \"p\"\n\
             interval_hours = 6\nretention_days = 7\nstructured_output = false\n",
        )
        .unwrap();
        let r = cfg.resolve_world_director().unwrap();
        assert_eq!(r.interval_hours, 6);
        assert_eq!(r.retention_days, 7);
        assert!(!r.structured_output);

        // interval_hours = 0 is a cost footgun (director would be eligible
        // every sweeper tick) — floored to 1, not passed through.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_director]\nmodel = \"w/m\"\nfilter_prompt = \"p\"\ninterval_hours = 0\n",
        )
        .unwrap();
        let r = cfg.resolve_world_director().unwrap();
        assert_eq!(r.interval_hours, 1, "0 must be floored to 1");
    }

    #[test]
    fn resolve_world_director_none_when_absent_or_blank_prompt() {
        let cfg = ModelConfig::from_toml_str("").unwrap();
        assert!(
            cfg.resolve_world_director().is_none(),
            "absent section ⇒ off"
        );
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_director]\nmodel = \"w/m\"\nfilter_prompt = \"  \"\n",
        )
        .unwrap();
        assert!(
            cfg.resolve_world_director().is_none(),
            "blank prompt ⇒ None"
        );
    }

    #[test]
    fn resolve_world_comment_defaults_and_overrides() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_comment]\nmodel = \"w/c\"\nfilter_prompt = \"comment round\"\n",
        )
        .unwrap();
        let r = cfg.resolve_world_comment().expect("configured");
        assert_eq!(r.model, "w/c");
        assert_eq!(r.comment_prompt, "comment round");
        assert!(r.structured_output, "default on");
        assert_eq!(r.round_secs, 3600, "default hourly");

        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_comment]\nmodel = \"w/c\"\nfilter_prompt = \"p\"\n\
             round_secs = 7200\nstructured_output = false\n",
        )
        .unwrap();
        let r = cfg.resolve_world_comment().unwrap();
        assert_eq!(r.round_secs, 7200);
        assert!(!r.structured_output);

        // round_secs = 0 would fire every sweeper tick — clamped to 60.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_comment]\nmodel = \"w/c\"\nfilter_prompt = \"p\"\nround_secs = 0\n",
        )
        .unwrap();
        assert_eq!(cfg.resolve_world_comment().unwrap().round_secs, 60);
    }

    #[test]
    fn resolve_world_reply_defaults_and_overrides() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_reply]\nmodel = \"w/r\"\nfilter_prompt = \"reply\"\n",
        )
        .unwrap();
        let r = cfg.resolve_world_reply().expect("configured");
        assert_eq!(r.reply_prompt, "reply");
        assert_eq!(r.debounce_secs, 90);
        assert_eq!(r.thread_cooldown_secs, 600);
        assert_eq!(r.daily_cap, 20);
        assert_eq!(r.reply_window_secs, 604_800, "default 7 days");

        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_reply]\nmodel = \"w/r\"\nfilter_prompt = \"p\"\n\
             debounce_secs = 30\nthread_cooldown_secs = 120\ndaily_cap = 5\n",
        )
        .unwrap();
        let r = cfg.resolve_world_reply().unwrap();
        assert_eq!(r.debounce_secs, 30);
        assert_eq!(r.thread_cooldown_secs, 120);
        assert_eq!(r.daily_cap, 5);

        // reply_window_secs override.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_reply]\nmodel = \"w/r\"\nfilter_prompt = \"p\"\n\
             reply_window_secs = 259200\n",
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_world_reply().unwrap().reply_window_secs,
            259_200
        );

        // A window <= debounce leaves no eligible range ⇒ clamped strictly
        // above the resolved debounce.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_reply]\nmodel = \"w/r\"\nfilter_prompt = \"p\"\n\
             debounce_secs = 100\nreply_window_secs = 50\n",
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_world_reply().unwrap().reply_window_secs,
            101,
            "clamped to debounce + 1"
        );
    }

    #[test]
    fn resolve_world_town_tasks_none_when_absent_or_blank_prompt() {
        let cfg = ModelConfig::from_toml_str("").unwrap();
        assert!(cfg.resolve_world_comment().is_none());
        assert!(cfg.resolve_world_reply().is_none());
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_comment]\nmodel = \"w/c\"\nfilter_prompt = \"  \"\n\
             [tasks.world_reply]\nmodel = \"w/r\"\n",
        )
        .unwrap();
        assert!(cfg.resolve_world_comment().is_none(), "blank prompt ⇒ None");
        assert!(cfg.resolve_world_reply().is_none(), "missing prompt ⇒ None");
    }

    #[test]
    fn validate_world_prompts_gates_all_three_sections() {
        let cfg = ModelConfig::from_toml_str("").unwrap();
        assert!(cfg.validate_world_prompts(true).is_ok(), "absent ⇒ Ok");
        assert!(cfg.validate_world_prompts(false).is_ok(), "absent ⇒ Ok");

        // world_director errs regardless of include_town (never town-gated).
        let cfg = ModelConfig::from_toml_str("[tasks.world_director]\nmodel = \"w/m\"\n").unwrap();
        for include_town in [true, false] {
            let err = cfg.validate_world_prompts(include_town).unwrap_err();
            assert!(
                err.contains("world_director"),
                "error names the section: {err}"
            );
        }

        // world_comment / world_reply only err when include_town is true —
        // WORLD_TOWN_DISABLED isolates a staged/broken town section.
        for section in ["world_comment", "world_reply"] {
            let cfg = ModelConfig::from_toml_str(&format!("[tasks.{section}]\nmodel = \"w/m\"\n"))
                .unwrap();
            let err = cfg.validate_world_prompts(true).unwrap_err();
            assert!(err.contains(section), "error names the section: {err}");
            assert!(
                cfg.validate_world_prompts(false).is_ok(),
                "include_town=false skips {section}"
            );
        }
    }
}

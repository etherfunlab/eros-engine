// SPDX-License-Identifier: AGPL-3.0-only
//! TOML-driven model orchestration config.

use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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

    /// Every literal candidate id, non-empty only (see `ModelSpec::candidate_ids`).
    fn candidate_ids(&self) -> Vec<&str> {
        match self {
            FallbackSpec::Single(s) if s.is_empty() => Vec::new(),
            FallbackSpec::Single(s) => vec![s.as_str()],
            FallbackSpec::Multiple(v) => v
                .iter()
                .filter(|s| !s.is_empty())
                .map(String::as_str)
                .collect(),
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

    /// Every literal candidate id in this spec, non-empty entries only. Boot
    /// validation must see ALL candidates: a weighted table picks at random
    /// per call, so validating only `select()`'s output would let a
    /// misconfigured entry lie dormant until an unlucky draw.
    fn candidate_ids(&self) -> Vec<&str> {
        match self {
            ModelSpec::Fixed(s) if s.is_empty() => Vec::new(),
            ModelSpec::Fixed(s) => vec![s.as_str()],
            ModelSpec::RoundRobin { models, .. } => models.iter().map(String::as_str).collect(),
            ModelSpec::Weighted(entries) => entries.iter().map(|(m, _)| m.as_str()).collect(),
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

/// A task's `filter_prompt`. Accepts three TOML shapes, mirroring `ModelSpec`:
/// `"xxx"` (plain), `["aaa","bbb"]` (index-keyed variants), or
/// `{ a = "aaa", b = "bbb" }` (string-keyed variants).
///
/// Only `[tasks.chat_image_prompt_compose]` reads variants; every other task —
/// and every tier block, including the composer's own — must use the plain
/// shape. Enforced at boot by `ModelConfig::validate_prompt_variants`, because
/// `TaskConfig` is shared by every `[tasks.*]` section and the type alone
/// cannot express the restriction.
///
/// `BTreeMap` (not `HashMap`) so key ordering in boot-failure messages is
/// deterministic across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum PromptSpec {
    Plain(String),
    Indexed(Vec<String>),
    Keyed(BTreeMap<String, String>),
}

impl PromptSpec {
    /// The prompt as a plain string. `None` for the variant shapes — which,
    /// after `validate_prompt_variants`, only the composer task can hold.
    /// Callers treat `None` exactly like an absent `filter_prompt`.
    pub fn as_plain(&self) -> Option<&str> {
        match self {
            PromptSpec::Plain(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Pick the variant named by `variant`. `None` ⇒ nothing was selected and
    /// the caller falls back to its built-in default.
    ///
    /// `Plain` ignores `variant` entirely — the string IS the prompt.
    /// `Indexed` parses `variant` as a `usize` index. `Keyed` looks it up as an
    /// exact, case-sensitive key (`default` carries no special meaning). Every
    /// miss in a variant shape — absent, unparseable, out of range, unknown
    /// key — is `None`.
    pub fn select(&self, variant: Option<&str>) -> Option<&str> {
        match self {
            PromptSpec::Plain(s) => Some(s.as_str()),
            PromptSpec::Indexed(v) => {
                let idx: usize = variant?.parse().ok()?;
                v.get(idx).map(String::as_str)
            }
            PromptSpec::Keyed(m) => m.get(variant?).map(String::as_str),
        }
    }
}

/// A task/tier `filter_prompt` as a plain string, or `""`. This is the shape
/// every non-composer `resolve_*` expects. A variant shape reads as `""`
/// (i.e. "unset"), so those tasks degrade to "feature off" rather than
/// misbehaving — a branch `validate_prompt_variants` makes unreachable at boot.
fn plain_or_empty(spec: Option<&PromptSpec>) -> String {
    spec.and_then(PromptSpec::as_plain)
        .unwrap_or_default()
        .to_string()
}

/// Structural rules for a variant-shaped `filter_prompt`. `Plain` always
/// passes — its blank-string leniency ("commented out" ⇒ built-in default) is
/// deliberate and unchanged. A variant container is a deliberate list, so a
/// blank inside it is a typo, and silently substituting the generic built-in
/// prompt would be the hardest class of misconfiguration to notice.
fn check_variant_shape(task: &str, spec: &PromptSpec) -> Result<(), String> {
    let refuse = |why: String| {
        Err(format!(
            "[tasks.{task}].filter_prompt {why} — eros-engine refuses to boot."
        ))
    };
    match spec {
        PromptSpec::Plain(_) => Ok(()),
        PromptSpec::Indexed(v) => {
            if v.is_empty() {
                return refuse("is an empty array".to_string());
            }
            match v.iter().position(|s| s.trim().is_empty()) {
                Some(i) => refuse(format!("has a blank entry at index {i}")),
                None => Ok(()),
            }
        }
        PromptSpec::Keyed(m) => {
            if m.is_empty() {
                return refuse("is an empty table".to_string());
            }
            for (k, v) in m {
                if k.trim().is_empty() {
                    return refuse("has a blank key".to_string());
                }
                if k.trim() != k.as_str() {
                    return refuse(format!(
                        "has whitespace-padded key \"{k}\": its trimmed form differs from the raw \
                         key, but `select` matches a client's `image.prompt_variant` exactly — so \
                         neither \"{k}\" nor its trimmed form could ever select it"
                    ));
                }
                if v.trim().is_empty() {
                    return refuse(format!("has a blank value for key \"{k}\""));
                }
            }
            Ok(())
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
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
    pub filter_prompt: Option<PromptSpec>,
    #[serde(default)]
    pub trigger: Option<OutputFilterTrigger>,
    #[serde(default)]
    pub timing: Option<FilterTiming>,
    #[serde(default)]
    pub retry_depth: Option<u32>,
}

/// The `[defaults]` section — cross-task fallbacks.
///
/// **Construct with struct-update syntax** — `DefaultConfig { fallback_model:
/// Some(…), ..Default::default() }` — never a bare exhaustive literal. New
/// optional fields are added here in minor releases and a bare literal breaks
/// on every such upgrade; `..Default::default()` is the supported
/// construction pattern (issue #188).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DefaultConfig {
    #[serde(default)]
    pub fallback_model: Option<String>,
    #[serde(default)]
    pub fallback_temperature: Option<f64>,
    #[serde(default)]
    pub fallback_max_tokens: Option<u32>,
    /// REMOVED (spec 2026-08-02-provider-body-params) — parsed only so
    /// `validate_providers` can refuse boot with a migration message pointing
    /// at `[[providers.openrouter.body]]`; never read anywhere else.
    #[serde(default)]
    #[doc(hidden)]
    pub ignore_providers: Vec<String>,
    /// REMOVED (spec 2026-08-02-provider-body-params) — parsed only so
    /// `validate_providers` can refuse boot with a migration message pointing
    /// at `[[providers.openrouter.body]]`; never read anywhere else.
    #[serde(default)]
    #[doc(hidden)]
    pub provider_sort: Option<String>,
}

/// One `[[providers.<name>.body]]` rule (spec 2026-08-02-provider-body-params):
/// opaque params merged into the chat/completions wire body for the tasks it
/// names. `tasks` omitted ⇒ every chat task this provider serves; `params` is
/// TOML→JSON passthrough the engine never interprets. Rules apply in
/// declaration order; later rules win on key conflicts, and the merged params
/// win over engine-built wire fields. Structural validation (non-empty params,
/// engine-owned-key refusal, task-name warnings) lives in `validate_providers`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BodyRule {
    #[serde(default)]
    pub tasks: Option<Vec<String>>,
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// One `[providers]` entry (spec 2026-08-01-embedding-providers §1). URLs are
/// complete and posted verbatim — no path joining. `headers` are sent verbatim
/// on every request to this entry's endpoints (both `chat` and `embeddings`);
/// `Authorization`/`Content-Type` are engine-owned and rejected at boot.
/// `BTreeMap` so validation-error ordering is deterministic across restarts.
///
/// `Eq` is dropped: `serde_json::Value` is not `Eq`.
/// `Deserialize` is hand-written (below) rather than derived: the 0.9.3
/// shape was a plain string, and a config still written that way must not
/// see a generic serde "invalid type: string ..., expected struct
/// ProviderEntry" — it should see the table form it needs to switch to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderEntry {
    pub chat: Option<String>,
    pub embeddings: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
    pub body: Option<Vec<BodyRule>>,
}

/// Error taught to an operator whose `[providers]` entry is still the 0.9.3
/// plain-string shape (dropped with no compatibility layer — spec §0/§1).
const PROVIDER_ENTRY_TABLE_FORM_ERROR: &str = "[providers] values must be tables — write \
     venice = { chat = \"https://…\" } (and/or embeddings = \"…\", headers = { … }); the \
     0.9.3 string form was removed";

impl<'de> Deserialize<'de> for ProviderEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Table-only inner shape, `deny_unknown_fields` preserved exactly as
        // before. Kept private and reached only via `visit_map` below, so a
        // non-table value never reaches it and never gets its generic
        // "expected struct" message.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            #[serde(default)]
            chat: Option<String>,
            #[serde(default)]
            embeddings: Option<String>,
            #[serde(default)]
            headers: Option<BTreeMap<String, String>>,
            #[serde(default)]
            body: Option<Vec<BodyRule>>,
        }

        struct ProviderEntryVisitor;

        impl<'de> serde::de::Visitor<'de> for ProviderEntryVisitor {
            type Value = ProviderEntry;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a [providers] entry table (chat = \"…\", embeddings = \"…\", headers = { … })",
                )
            }

            // Every scalar shape a 0.9.3-era config could still send —
            // string is the one that actually shipped, the rest are
            // defensive (an int/bool/etc is just as clearly not a table).
            fn visit_str<E>(self, _v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Err(E::custom(PROVIDER_ENTRY_TABLE_FORM_ERROR))
            }

            fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Err(E::custom(PROVIDER_ENTRY_TABLE_FORM_ERROR))
            }

            fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Err(E::custom(PROVIDER_ENTRY_TABLE_FORM_ERROR))
            }

            fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Err(E::custom(PROVIDER_ENTRY_TABLE_FORM_ERROR))
            }

            fn visit_seq<A>(self, _seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                use serde::de::Error as _;
                Err(A::Error::custom(PROVIDER_ENTRY_TABLE_FORM_ERROR))
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                // Unknown-key / wrong-type errors from `Table`'s own
                // `deny_unknown_fields` derive propagate unchanged — this
                // adapter only changes how a NON-table value is reported.
                let table = Table::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(ProviderEntry {
                    chat: table.chat,
                    embeddings: table.embeddings,
                    headers: table.headers,
                    body: table.body,
                })
            }
        }

        deserializer.deserialize_any(ProviderEntryVisitor)
    }
}

impl ProviderEntry {
    /// True when nothing at all is declared (all-`None`).
    pub fn is_empty(&self) -> bool {
        self.chat.is_none()
            && self.embeddings.is_none()
            && self.headers.is_none()
            && self.body.is_none()
    }

    /// Boot gate for the `headers` table: engine-owned names are rejected
    /// (a silently overridden `Authorization` is the worst kind of footgun),
    /// and every pair must be valid HTTP header material — loud-fail, not
    /// the env era's warn-and-drop.
    fn validate_headers(&self, entry: &str) -> Result<(), String> {
        let Some(headers) = &self.headers else {
            return Ok(());
        };
        if headers.is_empty() {
            return Err(format!("[providers].{entry}.headers: table is empty"));
        }
        for (name, value) in headers {
            let lower = name.to_ascii_lowercase();
            if lower == "authorization" || lower == "content-type" {
                return Err(format!(
                    "[providers].{entry}.headers: `{name}` is engine-owned — the \
                     engine sets Authorization from the provider's API key and \
                     Content-Type from the request body; it cannot be overridden"
                ));
            }
            if reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
                return Err(format!(
                    "[providers].{entry}.headers: `{name}` is not a valid HTTP header name"
                ));
            }
            if reqwest::header::HeaderValue::from_str(value).is_err() {
                return Err(format!(
                    "[providers].{entry}.headers: value for `{name}` is not a valid \
                     HTTP header value"
                ));
            }
        }
        Ok(())
    }

    /// The validated `headers` table as a reqwest `HeaderMap`. Invalid pairs
    /// are skipped with a warning — unreachable after `validate_headers`, but
    /// this method must not panic for library callers that skip validation.
    pub fn header_map(&self) -> reqwest::header::HeaderMap {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in self.headers.iter().flatten() {
            match (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                (Ok(n), Ok(v)) => {
                    map.insert(n, v);
                }
                _ => tracing::warn!(header = %name, "providers: skipping invalid header"),
            }
        }
        map
    }
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
    /// Embedding-only: recall-path model (voyage-4 series and above; pair-only
    /// with `model_write`; mutually exclusive with `model`). Plain string —
    /// round-robin/weighted shapes are type errors by design.
    #[serde(default)]
    pub model_read: Option<String>,
    /// Embedding-only: storage-path model. See `model_read`.
    #[serde(default)]
    pub model_write: Option<String>,
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
    pub filter_prompt: Option<PromptSpec>,
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
    /// (other tasks ignore it), like `input_filter`.
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
    /// world_stories_director-only: activity gate — a story instance is only
    /// claimed when its (owner, instance) chatted within this many hours.
    /// Default 72, floor 1.
    #[serde(default)]
    pub active_window_hours: Option<u32>,
    /// world_stories_director-only: chat/affinity evidence window in days fed
    /// to each story round. Default 7, floor 1.
    #[serde(default)]
    pub context_days: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelConfig {
    #[serde(default)]
    pub defaults: DefaultConfig,
    /// The `[providers]` block — custom endpoints keyed by name. Each entry is
    /// a table with optional `chat` (OpenAI-compatible) and `embeddings`
    /// (OpenRouter-compatible) URLs, plus an optional `headers` table sent
    /// verbatim on every request. URLs are COMPLETE, posted verbatim — no
    /// path joining. The API key comes from env as `<NAME_UPPERCASED>_API_KEY`.
    /// "openrouter" is special: it overrides BUILT-IN OpenRouter endpoints per
    /// key and homes the attribution headers (HTTP-Referer, X-OpenRouter-Title,
    /// X-OpenRouter-Categories). Under MODEL_CONFIG_DIR this merges as one
    /// whole top-level key (like `[defaults]`, unlike `[tasks]`).
    #[serde(default)]
    pub providers: HashMap<String, ProviderEntry>,
    #[serde(default)]
    pub tasks: HashMap<String, TaskConfig>,
}

/// Where one embedding call goes (spec 2026-08-01-embedding-providers §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedRoute {
    /// The built-in native Voyage client.
    Voyage,
    /// The built-in OpenRouter embeddings endpoint (URL overridable via
    /// `[providers].openrouter.embeddings`).
    OpenRouter,
    /// A `[providers]` entry's `embeddings` URL.
    Custom(String),
}

/// One resolved embedding target: bare model id + route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedTarget {
    pub model: String,
    pub route: EmbedRoute,
}

/// Resolved `[tasks.embedding]`: the read (recall / embed_query) and write
/// (storage / embed_document) targets. Identical targets when a single
/// `model` is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEmbedding {
    pub read: EmbedTarget,
    pub write: EmbedTarget,
}

const DEFAULT_EMBED_MODEL: &str = "voyage-4-lite";

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
/// text identity-free — no product name or brand.
///
/// Its stance is GENERATE (not expand): the judge no longer writes a seed, so
/// this step decides what the picture shows from the turn's context. It must
/// not soften, downgrade, or refuse, and must not add its own
/// content-moderation layer — content policy is enforced by the image provider
/// and the downstream consumer, not this step.
///
/// It returns JSON with two fields: `prompt` (the image-generation string) and
/// `caption` (one short line for the chat history). This is the one engine task
/// whose prompt ships in code.
const DEFAULT_COMPOSE_PROMPT: &str = "You compose the image for a picture the character is sending in a chat, and you return JSON with exactly two fields: \"prompt\" and \"caption\".\n\
\n\
You receive the character's appearance, the recent scene, the partner's latest message, a visual style, and a target aspect ratio, each as a labeled section.\n\
\n\
\"prompt\" is the image-generation prompt: decide what the picture shows from the partner's latest message and the recent scene. Produce one coherent, detailed, vivid prompt that faithfully preserves the appearance and fits the style and aspect ratio, delivering the full intent and tone of what was asked for. Do not soften, sanitize, downgrade, omit, censor, or refuse any part of it, and do not add safety warnings, disclaimers, or moderation of your own — content policy is the responsibility of the image provider and the downstream consumer, not this step.\n\
\n\
\"caption\" is one short line, in the language the conversation is in, saying what the picture shows — as the character would recall it later. It is read back into the conversation history, so keep it brief and natural; it is not an image-generation prompt and must not repeat the style boilerplate.\n\
\n\
Output only the JSON object. No commentary, options, or headings.";

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
    /// The Keyed key / Indexed index that selected `compose_prompt`, for the
    /// `metadata.image.compose_variant` audit key. `None` when there was no
    /// variant selection to speak of: `Plain`, no `filter_prompt`, or a miss
    /// (built-in prompt fallback).
    pub variant_key: Option<String>,
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

/// Resolved world-stories task (`world_stories_director`). The configured
/// `filter_prompt` is the system instruction; the server assembles the
/// per-instance story payload as a separate user message.
#[derive(Debug, Clone)]
pub struct ResolvedWorldStories {
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
    pub active_window_hours: u32,
    pub context_days: u32,
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
            .and_then(|t| t.filter_prompt.as_ref())
            .and_then(PromptSpec::as_plain)
            .or_else(|| {
                task_cfg
                    .filter_prompt
                    .as_ref()
                    .and_then(PromptSpec::as_plain)
            })
            .unwrap_or_default()
            .to_string();
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
        let filter_prompt = plain_or_empty(task_cfg.filter_prompt.as_ref());
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
        let describe_prompt = plain_or_empty(task_cfg.filter_prompt.as_ref());
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
            .as_ref()
            .and_then(PromptSpec::as_plain)
            .map(str::to_string)
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
        let decision_prompt = plain_or_empty(task_cfg.filter_prompt.as_ref());
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
        let answer_prompt = plain_or_empty(task_cfg.filter_prompt.as_ref());
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
            .and_then(|t| t.filter_prompt.as_ref())
            .and_then(PromptSpec::as_plain)
            .is_some_and(|p| !p.trim().is_empty())
    }

    /// Side-effect-free LLM-PDE availability check: true iff
    /// `[tasks.pde_decision]` is present with a non-blank `filter_prompt`.
    /// Mirrors `product_qa_enabled` — boot-time checks must not call
    /// `resolve_pde()`, which advances the task's round-robin cursor.
    pub fn pde_enabled(&self) -> bool {
        self.tasks
            .get("pde_decision")
            .and_then(|t| t.filter_prompt.as_ref())
            .and_then(PromptSpec::as_plain)
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

    /// Whether a `[tasks.<name>]` section exists at all, independent of its
    /// contents. Used by capability gates that must distinguish "not configured"
    /// from "configured but inert this turn".
    pub fn has_task(&self, name: &str) -> bool {
        self.tasks.contains_key(name)
    }

    /// Resolve the image-prompt composer task. `None` (composer does not run)
    /// for exactly one reason: `[tasks.chat_image_prompt_compose]` is absent.
    ///
    /// `variant` is the client's per-turn `image.prompt_variant`, already
    /// trimmed by the caller. Selection is delegated to `PromptSpec::select`:
    /// a plain `filter_prompt` ignores it, and any miss in a variant shape
    /// falls back to the built-in `DEFAULT_COMPOSE_PROMPT`.
    ///
    /// No probability/trigger gate; the caller invokes it only after an image
    /// action is decided.
    pub fn resolve_image_prompt_compose(
        &self,
        variant: Option<&str>,
    ) -> Option<ResolvedImagePromptCompose> {
        const COMPOSE_TASK: &str = "chat_image_prompt_compose";
        let variant = variant.map(str::trim).filter(|v| !v.is_empty());
        let task_cfg = self.tasks.get(COMPOSE_TASK)?;
        let selected = task_cfg
            .filter_prompt
            .as_ref()
            .and_then(|s| s.select(variant));
        // Audit value, NOT derivable from `selected` alone: `Plain` returns
        // `Some` from `select()` regardless of the supplied variant, but has
        // no variant selection to audit.
        let variant_key = match task_cfg.filter_prompt.as_ref() {
            None | Some(PromptSpec::Plain(_)) => None,
            Some(_) => selected.and(variant).map(str::to_string),
        };
        // The warn/debug decision is a pure function of (selected, variant,
        // has a filter_prompt at all) — pulled out of the tracing call so it
        // can be unit-tested directly, without a tracing subscriber, and so a
        // refactor that accidentally drops a guard shows up as a plain
        // assertion failure instead of only a missing log line.
        // `?` (Debug), not `%` (Display): `variant` is a client-supplied wire
        // value (`image.prompt_variant`) that nothing on the validation path
        // bounds — unlike `tier`, which is pattern-and-length checked in
        // `validate_payload`. Debug escapes/quotes so embedded newlines or
        // control characters can't smuggle fake log lines, and
        // `cap_for_log` bounds its length so a pathological value can't blow
        // up log volume.
        match compose_variant_log_event(selected, variant, task_cfg.filter_prompt.is_some()) {
            Some(ComposeVariantLogEvent::Mismatch) => tracing::warn!(
                variant = ?cap_for_log(variant.unwrap_or_default(), 64),
                "image-compose: variant not found; using the built-in prompt"
            ),
            Some(ComposeVariantLogEvent::Selected) => {
                tracing::debug!(
                    variant = ?cap_for_log(variant.unwrap_or_default(), 64),
                    "image-compose: variant selected"
                )
            }
            None => {}
        }
        // `trim`/`is_empty` is what makes a blank PLAIN filter_prompt fall
        // through to the built-in prompt. Redundant for the variant shapes
        // (`validate_prompt_variants` rejects blanks there at boot), but
        // removing it would regress the plain shape.
        let compose_prompt = selected
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
            variant_key,
        })
    }
}

/// Cap a string to at most `max_chars` characters before it reaches a log
/// line, appending `…` when truncated. Counts/truncates by `char`, never mid
/// UTF-8 codepoint. Used to bound client-supplied values (e.g. the image
/// compose `variant`) that no request-validation step already length-limits.
fn cap_for_log(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// What (if anything) `resolve_image_prompt_compose` should log about a
/// variant lookup. A `warn` on every miss would fire on the common
/// no-`prompt_variant`-supplied turn, which is silent by design — only an
/// explicitly-supplied variant that failed to match is worth surfacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposeVariantLogEvent {
    /// The caller supplied a variant, a `filter_prompt` container exists, and
    /// nothing matched — falling back to the built-in prompt is surprising
    /// enough to warn about.
    Mismatch,
    /// The caller supplied a variant and something was selected for it
    /// (worth a breadcrumb at `debug`, not louder).
    Selected,
}

/// Pure decision behind the warn/debug logging in
/// `resolve_image_prompt_compose`, split out so the guard conditions are
/// unit-testable without a `tracing` subscriber: a refactor that drops the
/// `variant.is_some()` or `has_filter_prompt` guard changes this function's
/// return value directly, instead of only silently changing log output.
fn compose_variant_log_event(
    selected: Option<&str>,
    variant: Option<&str>,
    has_filter_prompt: bool,
) -> Option<ComposeVariantLogEvent> {
    if selected.is_none() && variant.is_some() && has_filter_prompt {
        Some(ComposeVariantLogEvent::Mismatch)
    } else if selected.is_some() && variant.is_some() {
        Some(ComposeVariantLogEvent::Selected)
    } else {
        None
    }
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

    /// Pure resolution of `[tasks.embedding]` — call after `validate_providers`
    /// passed. Absent block, or a block with no model fields, resolves to the
    /// default (native Voyage, voyage-4-lite) on both sides. Deployments that
    /// need the no-longer-recommended voyage-3-lite must pin it explicitly —
    /// note the vector-space consequence: rows embedded by one model are not
    /// comparable to queries embedded by another.
    pub fn resolve_embedding(&self) -> ResolvedEmbedding {
        let task = self.tasks.get("embedding");
        let target = |slug: &str| -> EmbedTarget {
            match crate::provider::split_model_slug(slug) {
                Ok((bare, None)) | Ok((bare, Some("voyage"))) => EmbedTarget {
                    model: bare,
                    route: EmbedRoute::Voyage,
                },
                Ok((bare, Some("openrouter"))) => EmbedTarget {
                    model: bare,
                    route: EmbedRoute::OpenRouter,
                },
                Ok((bare, Some(p))) => EmbedTarget {
                    model: bare,
                    route: EmbedRoute::Custom(p.to_string()),
                },
                // Unreachable post-boot (validation walked every slug).
                Err(_) => EmbedTarget {
                    model: slug.to_string(),
                    route: EmbedRoute::Voyage,
                },
            }
        };
        if let Some(t) = task {
            if let (Some(r), Some(w)) = (t.model_read.as_deref(), t.model_write.as_deref()) {
                return ResolvedEmbedding {
                    read: target(r),
                    write: target(w),
                };
            }
            if let ModelSpec::Fixed(m) = &t.model {
                if !m.is_empty() {
                    let one = target(m);
                    return ResolvedEmbedding {
                        read: one.clone(),
                        write: one,
                    };
                }
            }
        }
        let default = EmbedTarget {
            model: DEFAULT_EMBED_MODEL.to_string(),
            route: EmbedRoute::Voyage,
        };
        ResolvedEmbedding {
            read: default.clone(),
            write: default,
        }
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
        let extract_prompt = plain_or_empty(task_cfg.filter_prompt.as_ref());
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
        let director_prompt = plain_or_empty(task_cfg.filter_prompt.as_ref());
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
        let comment_prompt = plain_or_empty(task_cfg.filter_prompt.as_ref());
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
        // Minimum width of the reply-eligibility band (`window - debounce`),
        // pinned to one town-sweeper tick (`TOWN_TICK` in
        // eros-engine-server/src/pipeline/world_town.rs). A narrower band
        // falls between two 30s scans, so a misconfigured
        // `reply_window_secs <= debounce_secs` would silently near-disable
        // replies instead of surfacing the misconfig (issue #180).
        const MIN_BAND_SECS: u64 = 30;

        let task_cfg = self.tasks.get("world_reply")?;
        let reply_prompt = plain_or_empty(task_cfg.filter_prompt.as_ref());
        if reply_prompt.trim().is_empty() {
            return None;
        }
        let m = self.resolve("world_reply", None);
        let debounce_secs = task_cfg.debounce_secs.unwrap_or(90);
        let configured_window = task_cfg.reply_window_secs.unwrap_or(604_800);
        let reply_window_secs = configured_window.max(debounce_secs + MIN_BAND_SECS);
        if reply_window_secs != configured_window {
            tracing::warn!(
                configured_window,
                debounce_secs,
                clamped_to = reply_window_secs,
                "world_reply: reply_window_secs leaves an eligibility band \
                 narrower than one 30s sweeper tick — clamped; fix \
                 [tasks.world_reply].reply_window_secs (must exceed \
                 debounce_secs by at least 30s)"
            );
        }
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
            reply_window_secs,
        })
    }

    /// Resolve the world-stories bundle. `None` when
    /// `[tasks.world_stories_director]` is absent OR its `filter_prompt` is
    /// blank — the story claim path goes inert.
    pub fn resolve_world_stories_director(&self) -> Option<ResolvedWorldStories> {
        let task_cfg = self.tasks.get("world_stories_director")?;
        let director_prompt = plain_or_empty(task_cfg.filter_prompt.as_ref());
        if director_prompt.trim().is_empty() {
            return None;
        }
        let m = self.resolve("world_stories_director", None);
        Some(ResolvedWorldStories {
            model: m.model,
            fallback_model: m.fallback_model,
            temperature: m.temperature,
            max_tokens: m.max_tokens,
            director_prompt,
            retry_depth: m.retry_depth,
            reasoning: m.reasoning,
            structured_output: task_cfg.structured_output.unwrap_or(true),
            // .max(1) floors mirror world_director: 0 would fire every tick /
            // empty the evidence window — cost/behavior footguns, not settings.
            interval_hours: task_cfg.interval_hours.unwrap_or(8).max(1),
            retention_days: task_cfg.retention_days.unwrap_or(30),
            active_window_hours: task_cfg.active_window_hours.unwrap_or(72).max(1),
            context_days: task_cfg.context_days.unwrap_or(7).max(1),
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

    /// Boot gate for `filter_prompt` variant shapes. Variants are read by
    /// `chat_image_prompt_compose` alone, and never from a tier block (the
    /// composer resolves with `tier = None`), so a variant anywhere else is
    /// dead config — refuse to boot rather than let it silently no-op.
    ///
    /// Also enforces the structural rules for a variant container: non-empty,
    /// no blank or whitespace-padded keys, no blank values (a padded key like
    /// `" a "` could never be selected, since `select` matches a client's
    /// variant exactly).
    ///
    /// Task names are visited in sorted order so the reported failure is
    /// deterministic across restarts (`self.tasks` is a `HashMap`).
    pub fn validate_prompt_variants(&self) -> Result<(), String> {
        const COMPOSE_TASK: &str = "chat_image_prompt_compose";
        let mut names: Vec<&String> = self.tasks.keys().collect();
        names.sort();
        for name in names {
            let task = &self.tasks[name];
            if let Some(spec) = &task.filter_prompt {
                if !matches!(spec, PromptSpec::Plain(_)) && name != COMPOSE_TASK {
                    return Err(format!(
                        "[tasks.{name}].filter_prompt uses a variant shape (array/table), but \
                         only [tasks.{COMPOSE_TASK}] reads variants — eros-engine refuses to \
                         boot. Use a plain string here."
                    ));
                }
                check_variant_shape(name, spec)?;
            }
            let mut tier_names: Vec<&String> = task.tiers.keys().collect();
            tier_names.sort();
            for tier_name in tier_names {
                let tier = &task.tiers[tier_name];
                if let Some(spec) = &tier.filter_prompt {
                    if !matches!(spec, PromptSpec::Plain(_)) {
                        return Err(format!(
                            "[tasks.{name}.tiers.{tier_name}].filter_prompt uses a variant shape \
                             (array/table), but tier blocks never carry variants — the composer \
                             resolves with no tier, so it could never be selected. eros-engine \
                             refuses to boot. Use a plain string here."
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Boot gate for `[tasks.affinity_evaluation].filter_prompt` — a key that
    /// parses but is never read.
    ///
    /// The affinity evaluator's prompt is engine-owned by design (issue #210):
    /// it interpolates per-turn context the config layer cannot express, and
    /// affinity is what PDE thresholds, scope gating, and `[emotional_context]`
    /// all consume — so a downstream rewrite of the evaluator's contract breaks
    /// behaviour the operator cannot see. Rather than add a resolver, refuse to
    /// boot, consistent with `validate_prompt_variants`: dead config must never
    /// silently no-op.
    ///
    /// Rejects the key in EVERY shape, blank included. Blank leniency
    /// ("commented out" ⇒ built-in default) exists for keys that are actually
    /// read; here it would reproduce the exact silence this gate removes.
    /// Every other `[tasks.affinity_evaluation]` field (model, fallback,
    /// temperature, max_tokens, reasoning) stays configurable.
    pub fn validate_affinity_prompt_unset(&self) -> Result<(), String> {
        const AFFINITY_TASK: &str = "affinity_evaluation";
        let has_prompt = self
            .tasks
            .get(AFFINITY_TASK)
            .is_some_and(|t| t.filter_prompt.is_some());
        if has_prompt {
            return Err(format!(
                "[tasks.{AFFINITY_TASK}].filter_prompt is set, but the affinity evaluator's \
                 prompt is engine-owned and deliberately not configurable — it was never read, \
                 and eros-engine refuses to boot rather than let it silently no-op. Remove the \
                 key; model/fallback/temperature/max_tokens/reasoning remain configurable. \
                 Rationale: https://github.com/etherfunlab/eros-engine/issues/210"
            ));
        }
        Ok(())
    }

    /// Boot gate for `[tasks.<name>.tiers.<tier>]` blocks under tasks that
    /// never resolve with a tier (issue #215).
    ///
    /// Only `TIER_CONSUMING_TASKS` ever reach a `TierConfig`; every other task
    /// resolves tier-free, so a tier block under one parses, boots, and can
    /// never be selected — dead config of exactly the kind
    /// `validate_prompt_variants` and `validate_affinity_prompt_unset` refuse,
    /// one level deeper in the tree.
    ///
    /// Gates the WHOLE block, not just `filter_prompt`: `model`, `fallback`,
    /// `allow_traits`, `output_filter`, `trigger`, `timing` and `retry_depth`
    /// are equally unreachable there.
    ///
    /// Task and tier names are visited in sorted order so the reported failure
    /// is deterministic across restarts (`self.tasks` is a `HashMap`), matching
    /// `validate_prompt_variants`.
    pub fn validate_tier_blocks(&self) -> Result<(), String> {
        let mut names: Vec<&String> = self.tasks.keys().collect();
        names.sort();
        for name in names {
            if TIER_CONSUMING_TASKS.contains(&name.as_str()) {
                continue;
            }
            let mut tier_names: Vec<&String> = self.tasks[name].tiers.keys().collect();
            tier_names.sort();
            let Some(tier_name) = tier_names.first() else {
                continue;
            };
            let allowed = TIER_CONSUMING_TASKS
                .iter()
                .map(|t| format!("[tasks.{t}]"))
                .collect::<Vec<_>>()
                .join(" and ");
            return Err(format!(
                "[tasks.{name}.tiers.{tier_name}] is a tier block under a task that never \
                 resolves with a tier — only {allowed} read tier blocks, so nothing in it could \
                 ever be selected. eros-engine refuses to boot rather than let it silently \
                 no-op. Move the settings to [tasks.{name}], or delete the block. Rationale: \
                 https://github.com/etherfunlab/eros-engine/issues/215"
            ));
        }
        Ok(())
    }
}

/// Tasks whose config the engine ever resolves with a tier. Everything else
/// resolves tier-free, so a `[tasks.<other>.tiers.*]` block is dead config —
/// see `ModelConfig::validate_tier_blocks`.
///
/// The two consumers are `resolve(task, tier)` (`chat_companion`, called from
/// `pipeline/handlers.rs`) and `resolve_output_filter(tier)`
/// (`chat_output_filter`, called from `pipeline/stream.rs`). Every other
/// resolver either passes `None` explicitly or takes no tier argument at all.
/// If a future task starts resolving with a tier, add its name here.
pub const TIER_CONSUMING_TASKS: &[&str] = &["chat_companion", "chat_output_filter"];

/// Every task name the chat/completions pipeline can present to the
/// body-rules matcher (`ChatRequest.task`). Used ONLY for the boot-time typo
/// warning on `[[providers.*.body]].tasks` — matching itself is plain string
/// equality, so an unlisted name warns, never errors.
///
/// When adding an engine task, add its name here AND set `ChatRequest.task`
/// at its pipeline call site.
pub const KNOWN_CHAT_TASKS: &[&str] = &[
    "chat_companion",
    "chat_output_filter",
    "chat_input_filter",
    "chat_voice",
    "chat_image_prompt_compose",
    "chat_product_qa",
    "pde_decision",
    "insight_extraction",
    "memory_extraction",
    "affinity_evaluation",
    "world_director",
    "world_stories_director",
    "world_comment",
    "world_reply",
];

/// Tasks that make outbound calls but build their own bodies — body rules
/// never reach them (spec: chat/completions `WireRequest` paths only).
const BODY_UNSUPPORTED_TASKS: &[&str] = &["chat_vision", "embedding"];

/// Pure warn-decision for one `[[providers.*.body]].tasks` entry —
/// unit-testable without a tracing subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyTaskWarning {
    /// A real engine task whose body this mechanism does not cover.
    Unsupported,
    /// Not an engine task name at all — probably a typo.
    Unknown,
}

fn body_rule_task_warning(task: &str) -> Option<BodyTaskWarning> {
    if BODY_UNSUPPORTED_TASKS.contains(&task) {
        Some(BodyTaskWarning::Unsupported)
    } else if !KNOWN_CHAT_TASKS.contains(&task) {
        Some(BodyTaskWarning::Unknown)
    } else {
        None
    }
}

impl ModelConfig {
    /// Boot gate for the `[providers]` block and every `@provider` model slug
    /// (spec 2026-08-01-embedding-providers §2/§4/§6/§7). Checks, in order:
    /// provider-table shape (charset, reserved name, non-empty URL);
    /// per-entry body-rule structure (§2026-08-02-provider-body-params);
    /// removed `[defaults]` provider-routing prefs (tombstone refusal); a
    /// task-aware LITERAL full scan of every candidate slug —
    /// `[defaults].fallback_model`, every task's and tier's
    /// `model`/`fallback`, `[tasks.embedding]`'s `model`/`model_read`/
    /// `model_write`, including every round-robin element and weighted-table
    /// key — where `@voyage` is legal only on embedding-task slugs and every
    /// other referenced provider must have a non-empty `<NAME>_API_KEY` in the
    /// environment; `[tasks.embedding]`'s structural rules (model vs.
    /// model_read/model_write exclusivity, no fallback/tiers, voyage-4+ gate
    /// on the read/write pair); and finally VOYAGE_API_KEY, required iff the
    /// resolved embedding config routes through the built-in Voyage client.
    pub fn validate_providers(&self) -> Result<(), String> {
        self.validate_providers_with(|k| std::env::var(k).ok())
    }

    /// Testable core: `env` abstracts `std::env::var` so tests inject keys
    /// without process-global mutation.
    fn validate_providers_with(&self, env: impl Fn(&str) -> Option<String>) -> Result<(), String> {
        // 1. Provider table shape. Sorted for deterministic messages.
        let mut names: Vec<&String> = self.providers.keys().collect();
        names.sort();
        for name in names {
            if name == "voyage" {
                return Err(
                    "[providers]: `voyage` is reserved — $VOYAGE_API_KEY already \
                     belongs to the built-in Voyage embeddings client, so a \
                     [providers] entry named `voyage` would read the same key. \
                     Pick another name."
                        .to_string(),
                );
            }
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                return Err(format!(
                    "[providers].{name}: provider names must match [a-z0-9_]+ — the name \
                     is uppercased into the `<NAME>_API_KEY` env var, so use underscores"
                ));
            }
            if self.providers[name].is_empty() {
                return Err(format!(
                    "[providers].{name}: entry is empty — declare at least one of \
                     `chat = \"<url>\"` or `embeddings = \"<url>\"` (or `headers` / \
                     `body` on the `openrouter` entry)"
                ));
            }
            if let Some(url) = self.providers[name].chat.as_deref() {
                if url.is_empty() {
                    return Err(format!("[providers].{name}.chat: URL is empty"));
                }
            }
            if let Some(url) = self.providers[name].embeddings.as_deref() {
                if url.is_empty() {
                    return Err(format!("[providers].{name}.embeddings: URL is empty"));
                }
            }
            self.providers[name].validate_headers(name)?;

            if let Some(rules) = &self.providers[name].body {
                if name != "openrouter" && self.providers[name].chat.is_none() {
                    return Err(format!(
                        "[providers].{name}: declares body rules but no `chat` URL — \
                         body params apply to chat/completions calls only (the reserved \
                         `openrouter` entry may omit `chat`; the built-in URL serves it)"
                    ));
                }
                for (i, rule) in rules.iter().enumerate() {
                    if rule.params.is_empty() {
                        return Err(format!(
                            "[providers].{name}.body[{i}].params: table is empty"
                        ));
                    }
                    for key in ["model", "messages", "stream"] {
                        if rule.params.contains_key(key) {
                            return Err(format!(
                                "[providers].{name}.body[{i}].params: `{key}` is engine-owned \
                                 wire structure and cannot be overridden"
                            ));
                        }
                    }
                    if let Some(tasks) = &rule.tasks {
                        if tasks.is_empty() {
                            return Err(format!(
                                "[providers].{name}.body[{i}].tasks: empty array — omit the \
                                 key to mean \"all chat tasks\""
                            ));
                        }
                        for t in tasks {
                            match body_rule_task_warning(t) {
                                Some(BodyTaskWarning::Unsupported) => tracing::warn!(
                                    provider = %name, task = %t,
                                    "[providers] body rule names a task this mechanism does \
                                     not cover — it will never apply"
                                ),
                                Some(BodyTaskWarning::Unknown) => tracing::warn!(
                                    provider = %name, task = %t,
                                    "[providers] body rule names an unknown task — matching \
                                     is exact, check for a typo"
                                ),
                                None => {}
                            }
                        }
                    }
                }
            }
        }

        // 1.5. Removed [defaults] keys (spec 2026-08-02-provider-body-params):
        // the fields still PARSE (tombstones) purely so an operator upgrading
        // across the removal gets this message instead of a silently dead key.
        if !self.defaults.ignore_providers.is_empty() || self.defaults.provider_sort.is_some() {
            return Err(
                "[defaults].ignore_providers / [defaults].provider_sort were removed: \
                 provider routing prefs are now ordinary body params on the reserved \
                 `openrouter` entry —\n\
                 [[providers.openrouter.body]]\n\
                 params = { provider = { ignore = [\"<slug>\"], sort = \"price\" } }\n\
                 (bare OpenRouter provider slugs — no @openrouter suffix; add \
                 tasks = [\"…\"] to scope the rule). Delete the [defaults] keys."
                    .to_string(),
            );
        }

        // 2. Literal full scan of every candidate slug. The third element flags
        // an embedding-task slug: `[tasks.embedding].model` (and, below,
        // `.model_read`/`.model_write`) are the only entries where `@voyage`
        // is legal and where routing demands an `embeddings` URL rather than
        // a `chat` one.
        let mut slugs: Vec<(String, &str, bool)> = Vec::new();
        if let Some(fb) = self.defaults.fallback_model.as_deref() {
            if !fb.is_empty() {
                slugs.push(("[defaults].fallback_model".to_string(), fb, false));
            }
        }
        let mut task_names: Vec<&String> = self.tasks.keys().collect();
        task_names.sort();
        for name in task_names {
            let task = &self.tasks[name];
            let is_embedding = name == "embedding";
            for id in task.model.candidate_ids() {
                slugs.push((format!("[tasks.{name}].model"), id, is_embedding));
            }
            if let Some(fb) = &task.fallback {
                for id in fb.candidate_ids() {
                    slugs.push((format!("[tasks.{name}].fallback"), id, is_embedding));
                }
            }
            if is_embedding {
                if let Some(m) = task.model_read.as_deref() {
                    if !m.is_empty() {
                        slugs.push(("[tasks.embedding].model_read".to_string(), m, true));
                    }
                }
                if let Some(m) = task.model_write.as_deref() {
                    if !m.is_empty() {
                        slugs.push(("[tasks.embedding].model_write".to_string(), m, true));
                    }
                }
            }
            let mut tier_names: Vec<&String> = task.tiers.keys().collect();
            tier_names.sort();
            for tier in tier_names {
                let t = &task.tiers[tier];
                if let Some(m) = &t.model {
                    for id in m.candidate_ids() {
                        slugs.push((format!("[tasks.{name}.tiers.{tier}].model"), id, false));
                    }
                }
                if let Some(fb) = &t.fallback {
                    for id in fb.candidate_ids() {
                        slugs.push((format!("[tasks.{name}.tiers.{tier}].fallback"), id, false));
                    }
                }
            }
        }

        for (at, slug, is_embedding) in slugs {
            let (_, provider) =
                crate::provider::split_model_slug(slug).map_err(|e| format!("{at}: {e}"))?;
            match provider {
                None => {}
                // Built-in alias: valid everywhere a provider suffix is legal,
                // no [providers] entry and no extra key check
                // ($OPENROUTER_API_KEY is unconditionally required for chat).
                Some("openrouter") => {}
                Some("voyage") if is_embedding => {}
                Some("voyage") => {
                    return Err(format!(
                        "{at}: `{slug}` routes to `voyage`, which serves embeddings only — \
                         `@voyage` is valid on [tasks.embedding] model fields and nowhere else"
                    ));
                }
                Some(p) => {
                    let Some(entry) = self.providers.get(p) else {
                        let mut declared: Vec<&str> =
                            self.providers.keys().map(String::as_str).collect();
                        declared.sort();
                        return Err(format!(
                            "{at}: `{slug}` names provider `{p}`, which is not declared in \
                             [providers] (declared: {declared:?}). If the `@` is part of the \
                             model id, escape it as `\\@` (in TOML double quotes: `\"\\\\@\"`)."
                        ));
                    };
                    if is_embedding && entry.embeddings.is_none() {
                        return Err(format!(
                            "{at}: `{slug}` routes embedding traffic to `{p}`, but \
                             [providers].{p} declares no `embeddings` URL"
                        ));
                    }
                    if !is_embedding && entry.chat.is_none() {
                        return Err(format!(
                            "{at}: `{slug}` routes chat traffic to `{p}`, but \
                             [providers].{p} declares no `chat` URL"
                        ));
                    }
                    let var = format!("{}_API_KEY", p.to_uppercase());
                    if env(&var).is_none_or(|v| v.is_empty()) {
                        return Err(format!(
                            "{at}: `{slug}` routes to provider `{p}` but ${var} is unset or \
                             empty — eros-engine refuses to boot rather than fail at request \
                             time"
                        ));
                    }
                }
            }
        }

        // 3. [tasks.embedding]-specific structure (spec §2/§6).
        if let Some(t) = self.tasks.get("embedding") {
            let model_set = !t.model.candidate_ids().is_empty();
            let pair = (t.model_read.as_deref(), t.model_write.as_deref());
            if model_set && (pair.0.is_some() || pair.1.is_some()) {
                return Err(
                    "[tasks.embedding]: `model` and `model_read`/`model_write` are mutually \
                     exclusive — configure one or the other"
                        .to_string(),
                );
            }
            if pair.0.is_some() != pair.1.is_some() {
                return Err(
                    "[tasks.embedding]: `model_read` and `model_write` must appear together \
                     (a lone half would leave the other path on an unspecified model)"
                        .to_string(),
                );
            }
            if model_set && !matches!(t.model, ModelSpec::Fixed(_)) {
                return Err(
                    "[tasks.embedding].model must be a single fixed id — round-robin and \
                     weighted forms would interleave incompatible vector spaces"
                        .to_string(),
                );
            }
            if t.fallback.is_some() {
                return Err(
                    "[tasks.embedding]: `fallback` is not supported — a fallback model would \
                     write vectors the primary cannot compare. Delete the key."
                        .to_string(),
                );
            }
            if !t.tiers.is_empty() {
                return Err(
                    "[tasks.embedding]: `tiers` are not supported on the embedding task. \
                     Delete the block."
                        .to_string(),
                );
            }
            for (field, slug) in [("model_read", pair.0), ("model_write", pair.1)] {
                let Some(slug) = slug else { continue };
                let (bare, provider) = crate::provider::split_model_slug(slug)
                    .map_err(|e| format!("[tasks.embedding].{field}: {e}"))?;
                if !matches!(provider, None | Some("voyage")) {
                    return Err(format!(
                        "[tasks.embedding].{field}: `{slug}` — model_read/model_write are \
                         Voyage-only (bare id or `@voyage`); other providers cannot \
                         guarantee a shared vector space"
                    ));
                }
                match voyage_model_generation(&bare) {
                    Some(n) if n >= 4.0 => {}
                    _ => {
                        return Err(format!(
                            "[tasks.embedding].{field}: `{bare}` — the read/write split \
                             requires the voyage-4 series or above (a shared vector space \
                             across model sizes); voyage-3.x and domain models refuse to \
                             boot rather than silently write incomparable vectors"
                        ));
                    }
                }
            }
        }

        // 4. VOYAGE_API_KEY: required iff a Voyage backend is in play (spec §7).
        let resolved = self.resolve_embedding();
        let voyage_in_play =
            resolved.read.route == EmbedRoute::Voyage || resolved.write.route == EmbedRoute::Voyage;
        if voyage_in_play && env("VOYAGE_API_KEY").is_none_or(|v| v.trim().is_empty()) {
            return Err(
                "VOYAGE_API_KEY is unset or empty but the embedding config resolves to \
                 the built-in Voyage client — eros-engine refuses to boot rather than \
                 silently disable embeddings"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Build the name → endpoint map handed to `OpenRouterClient` at boot.
    /// NO checks here — runs only after `validate_providers` passed. Only
    /// entries with a `chat` URL are included (chat routing is the only
    /// consumer of this map); an unreferenced entry without a key gets an
    /// empty api_key (unreachable at runtime, and `resolve_endpoint` guards
    /// it anyway).
    pub fn build_providers(&self) -> HashMap<String, crate::provider::ProviderEndpoint> {
        self.build_providers_with(|k| std::env::var(k).ok())
    }

    fn build_providers_with(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> HashMap<String, crate::provider::ProviderEndpoint> {
        self.providers
            .iter()
            .filter_map(|(name, entry)| {
                let chat = entry.chat.clone()?;
                let key = env(&format!("{}_API_KEY", name.to_uppercase())).unwrap_or_default();
                Some((
                    name.clone(),
                    crate::provider::ProviderEndpoint {
                        base_url: chat,
                        api_key: key,
                        headers: entry.header_map(),
                        body_rules: entry.body.clone().unwrap_or_default(),
                    },
                ))
            })
            .collect()
    }

    /// `[providers].openrouter.chat`, if declared — the built-in chat URL
    /// override (spec §4).
    pub fn openrouter_chat_url(&self) -> Option<String> {
        self.providers
            .get("openrouter")
            .and_then(|e| e.chat.clone())
    }

    /// `[providers].openrouter.headers` as a HeaderMap (empty when absent).
    /// The one home for OpenRouter attribution headers.
    pub fn openrouter_header_map(&self) -> reqwest::header::HeaderMap {
        self.providers
            .get("openrouter")
            .map(|e| e.header_map())
            .unwrap_or_default()
    }

    /// `[providers].openrouter.body` rules (empty when absent) — the built-in
    /// endpoint's body rules, installed at boot via
    /// `OpenRouterClient::with_openrouter_body_rules`.
    pub fn openrouter_body_rules(&self) -> Vec<BodyRule> {
        self.providers
            .get("openrouter")
            .and_then(|e| e.body.clone())
            .unwrap_or_default()
    }

    /// Reject config blocks for features that were removed, so an operator
    /// upgrading across the removal cannot silently keep a block that no
    /// longer does anything. Loud-fail, same shape as the other boot gates.
    pub fn validate_removed_tasks(&self) -> Result<(), String> {
        if self.tasks.contains_key("chat_image_generation") {
            return Err(
                "[tasks.chat_image_generation] was removed: the engine no longer draws \
                 images. The chat stream emits an `image_request` frame and the consumer \
                 calls its own image vendor; the draw endpoint \
                 POST /comp/chat/{session_id}/image/stream is gone. Delete this block."
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Boot gate, mirroring `validate_extraction_prompts`: any present world
    /// task section (`world_director` / `world_comment` / `world_reply` /
    /// `world_stories_director`) must carry a usable `filter_prompt`.
    /// `world_director` is always checked; the two town sections
    /// (`world_comment` / `world_reply`) are checked only when `include_town`
    /// is true. `include_town = false` (WORLD_TOWN_DISABLED) skips the two
    /// town sections so a staged/broken town config cannot block boot — same
    /// isolation rationale as WORLD_DISABLED for the whole block.
    /// `world_stories_director` is checked only when `include_stories` is
    /// true — `include_stories = false` (WORLD_STORIES_DISABLED) isolates a
    /// staged/broken stories config the same way.
    pub fn validate_world_prompts(
        &self,
        include_town: bool,
        include_stories: bool,
    ) -> Result<(), String> {
        let mut checks = vec![("world_director", self.resolve_world_director().is_none())];
        if include_town {
            checks.push(("world_comment", self.resolve_world_comment().is_none()));
            checks.push(("world_reply", self.resolve_world_reply().is_none()));
        }
        if include_stories {
            checks.push((
                "world_stories_director",
                self.resolve_world_stories_director().is_none(),
            ));
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

/// Parse the generation number out of a bare voyage model id: the numeric
/// segment (digits and dots) immediately after `voyage-`, ending at the next
/// `-` or end of string. `None` ⇒ no leading numeric segment (domain-named
/// models like `voyage-code-3`) or not a single number. Used by the
/// model_read/model_write boot gate: only the voyage-4 series and above
/// guarantee one shared vector space across model sizes.
fn voyage_model_generation(bare_id: &str) -> Option<f64> {
    let rest = bare_id.strip_prefix("voyage-")?;
    let segment: &str = rest
        .split('-')
        .next()
        .expect("split always yields at least one element");
    // Reject anything but ASCII digits and dots BEFORE parsing. `f64::from_str`
    // is far more permissive than a version number — it accepts `inf`, `nan`,
    // and scientific notation like `4e2` (== 400.0), any of which would let a
    // non-numeric or absurd segment sail through the `>= 4.0` gate below.
    if segment.is_empty() || !segment.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    let n = segment.parse::<f64>().ok()?;
    n.is_finite().then_some(n)
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
description  = "active — routes embed_query/embed_document via EmbeddingRouter"

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
            pde.filter_prompt.as_ref().and_then(PromptSpec::as_plain),
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
            pq.filter_prompt.as_ref().and_then(PromptSpec::as_plain),
            Some("Answer product questions from the docs.")
        );
        let rpq = cfg
            .resolve_product_qa()
            .expect("fixture product_qa resolves");
        assert_eq!(rpq.answer_prompt, "Answer product questions from the docs.");

        // embedding — active. `dimensions` was removed from `TaskConfig`
        // (spec 2026-08-01-embedding-providers §0: dims are fixed at 512);
        // the fixture keeps the `dimensions = 512` TOML line above to lock
        // the compat contract that a leftover key from an old config still
        // parses — it's just an ignored unknown key now, not a field.
        let emb = cfg.tasks.get("embedding").unwrap();
        assert_eq!(emb.model.as_fixed(), Some("voyage-3-lite"));

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
        assert_eq!(
            f.filter_prompt.as_ref().and_then(PromptSpec::as_plain),
            Some("Rewrite: {x}")
        );
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
            f.tiers["gold"]
                .filter_prompt
                .as_ref()
                .and_then(PromptSpec::as_plain),
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
    fn defaults_ignore_providers_absent_is_empty() {
        let toml = r#"
            [tasks.chat_companion]
            model = "x/y"
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse");
        assert!(cfg.defaults.ignore_providers.is_empty());
    }

    #[test]
    fn removed_defaults_prefs_refuse_boot() {
        for src in [
            "[defaults]\nignore_providers = [\"bad@openrouter\"]\n",
            "[defaults]\nprovider_sort = \"latency\"\n",
        ] {
            let cfg = ModelConfig::from_toml_str(src).unwrap();
            let err = cfg
                .validate_providers_with(|_| Some("k".into()))
                .unwrap_err();
            assert!(
                err.contains("[[providers.openrouter.body]]"),
                "removal error must teach the replacement: {err}"
            );
        }
    }

    #[test]
    fn body_rule_empty_params_refuses() {
        let cfg = ModelConfig::from_toml_str(
            "[providers.venice]\nchat = \"https://v/chat\"\n\
             [[providers.venice.body]]\nparams = { }\n",
        )
        .unwrap();
        let err = cfg
            .validate_providers_with(|_| Some("k".into()))
            .unwrap_err();
        assert!(err.contains("body[0].params"), "{err}");
    }

    #[test]
    fn body_rule_structural_keys_refuse() {
        for key in ["model", "messages", "stream"] {
            let cfg = ModelConfig::from_toml_str(&format!(
                "[providers.venice]\nchat = \"https://v/chat\"\n\
                 [[providers.venice.body]]\nparams = {{ {key} = \"x\" }}\n"
            ))
            .unwrap();
            let err = cfg
                .validate_providers_with(|_| Some("k".into()))
                .unwrap_err();
            assert!(err.contains("engine-owned"), "{key}: {err}");
        }
    }

    #[test]
    fn body_rule_empty_tasks_refuses() {
        let cfg = ModelConfig::from_toml_str(
            "[providers.venice]\nchat = \"https://v/chat\"\n\
             [[providers.venice.body]]\ntasks = []\nparams = { a = 1 }\n",
        )
        .unwrap();
        let err = cfg
            .validate_providers_with(|_| Some("k".into()))
            .unwrap_err();
        assert!(err.contains("tasks"), "{err}");
    }

    #[test]
    fn body_without_chat_refuses_for_custom_but_not_openrouter() {
        let custom = ModelConfig::from_toml_str(
            "[providers.venice]\nembeddings = \"https://v/emb\"\n\
             [[providers.venice.body]]\nparams = { a = 1 }\n",
        )
        .unwrap();
        assert!(custom
            .validate_providers_with(|_| Some("k".into()))
            .unwrap_err()
            .contains("no `chat` URL"));

        let openrouter =
            ModelConfig::from_toml_str("[[providers.openrouter.body]]\nparams = { a = 1 }\n")
                .unwrap();
        assert!(openrouter
            .validate_providers_with(|_| Some("k".into()))
            .is_ok());
    }

    #[test]
    fn body_rule_task_warning_semantics() {
        assert_eq!(body_rule_task_warning("chat_companion"), None);
        assert_eq!(
            body_rule_task_warning("chat_vision"),
            Some(BodyTaskWarning::Unsupported)
        );
        assert_eq!(
            body_rule_task_warning("embedding"),
            Some(BodyTaskWarning::Unsupported)
        );
        assert_eq!(
            body_rule_task_warning("chat_companon"),
            Some(BodyTaskWarning::Unknown)
        );
    }

    #[test]
    fn voyage_generation_parses_main_line() {
        assert_eq!(voyage_model_generation("voyage-4"), Some(4.0));
        assert_eq!(voyage_model_generation("voyage-4-lite"), Some(4.0));
        assert_eq!(voyage_model_generation("voyage-4.5-large"), Some(4.5));
        assert_eq!(voyage_model_generation("voyage-10"), Some(10.0));
        assert_eq!(voyage_model_generation("voyage-3.5-lite"), Some(3.5));
    }

    #[test]
    fn voyage_generation_rejects_unparseable() {
        assert_eq!(voyage_model_generation("voyage-code-3"), None);
        assert_eq!(voyage_model_generation("voyage-"), None);
        assert_eq!(voyage_model_generation("bge-m3"), None);
        assert_eq!(voyage_model_generation("voyage-4.5.1"), None); // not a single f64
    }

    #[test]
    fn voyage_generation_rejects_f64_leniency() {
        // `f64::from_str` accepts far more than a version number does —
        // segments must be ASCII digits/dots only, checked BEFORE parsing,
        // so none of these ever reach `str::parse::<f64>`.
        assert_eq!(voyage_model_generation("voyage-inf"), None);
        assert_eq!(voyage_model_generation("voyage-nan"), None);
        // "4e2" parses as 400.0 (scientific notation) — well past the N >= 4
        // gate, but "e" isn't a version-number character.
        assert_eq!(voyage_model_generation("voyage-4e2"), None);
    }

    #[test]
    fn defaults_provider_sort_absent_is_none() {
        let toml = r#"
            [tasks.chat_companion]
            model = "x/y"
        "#;
        let cfg = ModelConfig::from_toml_str(toml).expect("parse");
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

    // ─── StyleKey presets ───────────────────────────────────────────────────

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
        assert!(cfg.resolve_image_prompt_compose(None).is_none());
    }

    #[test]
    fn resolve_image_prompt_compose_uses_builtin_when_prompt_blank() {
        // task present but no usable filter_prompt → enabled with the built-in
        // default (NOT off — this is the deviation from the sibling tasks).
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = \"   \"\n",
        )
        .unwrap();
        let r = cfg.resolve_image_prompt_compose(None).unwrap();
        assert_eq!(r.compose_prompt, DEFAULT_COMPOSE_PROMPT);

        // also true when filter_prompt is omitted entirely
        let cfg2 = ModelConfig::from_toml_str("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n")
            .unwrap();
        assert_eq!(
            cfg2.resolve_image_prompt_compose(None)
                .unwrap()
                .compose_prompt,
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
            cfg.resolve_image_prompt_compose(None)
                .unwrap()
                .compose_prompt,
            "custom composer"
        );
    }

    #[test]
    fn resolve_image_prompt_compose_some_truncates_fallback() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = \"compose it\"\nfallback = [\"a\", \"b\", \"c\"]\nretry_depth = 1\n",
        )
        .unwrap();
        let r = cfg.resolve_image_prompt_compose(None).unwrap();
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
    fn from_toml_dir_preserves_provider_body_rule() {
        let tmp = tempfile::tempdir().unwrap();
        write_cfg(
            tmp.path(),
            "providers.toml",
            "[providers.venice]\nchat = \"https://v/chat\"\n\
             [[providers.venice.body]]\ntasks = [\"chat_companion\"]\n\
             params = { venice_parameters = { include_venice_system_prompt = false } }\n",
        );
        write_cfg(
            tmp.path(),
            "chat.toml",
            "[tasks.chat_companion]\nmodel = \"p/chat\"\n",
        );
        let cfg = ModelConfig::from_toml_dir(tmp.path()).unwrap();
        let body = cfg.providers["venice"].body.as_ref().unwrap();
        assert_eq!(body.len(), 1);
        assert_eq!(
            body[0].tasks.as_deref(),
            Some(&["chat_companion".to_string()][..])
        );
        assert_eq!(
            body[0].params["venice_parameters"]["include_venice_system_prompt"],
            serde_json::Value::Bool(false)
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

        // A window <= debounce leaves no eligible range ⇒ clamped to at least
        // one town-sweeper tick above the resolved debounce, so the eligible
        // band can never be narrower than the 30s tick that samples it
        // (issue #180: a +1s floor was almost always missed by the sweeper —
        // replies silently near-disabled instead of loudly misconfigured).
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_reply]\nmodel = \"w/r\"\nfilter_prompt = \"p\"\n\
             debounce_secs = 100\nreply_window_secs = 50\n",
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_world_reply().unwrap().reply_window_secs,
            130,
            "clamped to debounce + one 30s town tick"
        );

        // A window inside the tick-wide band is floored too.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_reply]\nmodel = \"w/r\"\nfilter_prompt = \"p\"\n\
             debounce_secs = 100\nreply_window_secs = 110\n",
        )
        .unwrap();
        assert_eq!(cfg.resolve_world_reply().unwrap().reply_window_secs, 130);

        // A sane window (> debounce + tick) is untouched.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_reply]\nmodel = \"w/r\"\nfilter_prompt = \"p\"\n\
             debounce_secs = 100\nreply_window_secs = 131\n",
        )
        .unwrap();
        assert_eq!(cfg.resolve_world_reply().unwrap().reply_window_secs, 131);
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
        assert!(
            cfg.validate_world_prompts(true, true).is_ok(),
            "absent ⇒ Ok"
        );
        assert!(
            cfg.validate_world_prompts(false, false).is_ok(),
            "absent ⇒ Ok"
        );

        // world_director errs regardless of include_town/include_stories
        // (never town/stories-gated).
        let cfg = ModelConfig::from_toml_str("[tasks.world_director]\nmodel = \"w/m\"\n").unwrap();
        for include_town in [true, false] {
            let err = cfg.validate_world_prompts(include_town, false).unwrap_err();
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
            let err = cfg.validate_world_prompts(true, false).unwrap_err();
            assert!(err.contains(section), "error names the section: {err}");
            assert!(
                cfg.validate_world_prompts(false, false).is_ok(),
                "include_town=false skips {section}"
            );
        }
    }

    #[test]
    fn resolve_world_stories_defaults_and_overrides() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_stories_director]\nmodel = \"w/s\"\nfilter_prompt = \"live a life\"\n",
        )
        .unwrap();
        let r = cfg.resolve_world_stories_director().expect("configured");
        assert_eq!(r.model, "w/s");
        assert_eq!(r.director_prompt, "live a life");
        assert!(r.structured_output, "default true");
        assert_eq!(r.interval_hours, 8, "stories default cadence is 8h");
        assert_eq!(r.retention_days, 30);
        assert_eq!(r.active_window_hours, 72);
        assert_eq!(r.context_days, 7);

        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_stories_director]\nmodel = \"w/s\"\nfilter_prompt = \"p\"\n\
             interval_hours = 4\nretention_days = 14\nactive_window_hours = 24\ncontext_days = 3\n",
        )
        .unwrap();
        let r = cfg.resolve_world_stories_director().unwrap();
        assert_eq!(r.interval_hours, 4);
        assert_eq!(r.retention_days, 14);
        assert_eq!(r.active_window_hours, 24);
        assert_eq!(r.context_days, 3);

        // 0 floors: interval/window/context of 0 would fire every tick or empty the evidence.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_stories_director]\nmodel = \"w/s\"\nfilter_prompt = \"p\"\n\
             interval_hours = 0\nactive_window_hours = 0\ncontext_days = 0\n",
        )
        .unwrap();
        let r = cfg.resolve_world_stories_director().unwrap();
        assert_eq!(r.interval_hours, 1);
        assert_eq!(r.active_window_hours, 1);
        assert_eq!(r.context_days, 1);
    }

    #[test]
    fn resolve_world_stories_none_when_absent_or_blank_prompt() {
        let cfg = ModelConfig::from_toml_str("").unwrap();
        assert!(
            cfg.resolve_world_stories_director().is_none(),
            "absent ⇒ None"
        );
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_stories_director]\nmodel = \"w/s\"\nfilter_prompt = \"  \"\n",
        )
        .unwrap();
        assert!(
            cfg.resolve_world_stories_director().is_none(),
            "blank ⇒ None"
        );
    }

    #[test]
    fn validate_world_prompts_checks_stories_only_when_included() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.world_stories_director]\nmodel = \"w/s\"\nfilter_prompt = \"\"\n",
        )
        .unwrap();
        assert!(
            cfg.validate_world_prompts(false, true).is_err(),
            "blank stories prompt refuses boot"
        );
        assert!(
            cfg.validate_world_prompts(false, false).is_ok(),
            "WORLD_STORIES_DISABLED skips the stories check"
        );
        // No stories section at all ⇒ fine either way.
        let cfg = ModelConfig::from_toml_str("").unwrap();
        assert!(cfg.validate_world_prompts(true, true).is_ok());
    }

    // ─── PromptSpec ──────────────────────────────────────────────────────

    #[derive(Deserialize)]
    struct SpecWrap {
        p: PromptSpec,
    }

    fn spec(src: &str) -> PromptSpec {
        toml::from_str::<SpecWrap>(src).expect("parse PromptSpec").p
    }

    #[test]
    fn prompt_spec_parses_three_shapes() {
        assert_eq!(spec(r#"p = "xxx""#), PromptSpec::Plain("xxx".into()));
        assert_eq!(
            spec(r#"p = ["aaa", "bbb"]"#),
            PromptSpec::Indexed(vec!["aaa".into(), "bbb".into()])
        );
        let mut m = std::collections::BTreeMap::new();
        m.insert("a".to_string(), "aaa".to_string());
        m.insert("b".to_string(), "bbb".to_string());
        assert_eq!(
            spec(r#"p = { a = "aaa", b = "bbb" }"#),
            PromptSpec::Keyed(m)
        );
    }

    #[test]
    fn prompt_spec_as_plain_only_for_plain() {
        assert_eq!(spec(r#"p = "xxx""#).as_plain(), Some("xxx"));
        assert_eq!(spec(r#"p = ["aaa"]"#).as_plain(), None);
        assert_eq!(spec(r#"p = { a = "aaa" }"#).as_plain(), None);
    }

    #[test]
    fn prompt_spec_plain_ignores_variant() {
        let s = spec(r#"p = "xxx""#);
        assert_eq!(s.select(None), Some("xxx"));
        assert_eq!(s.select(Some("b")), Some("xxx"));
        assert_eq!(s.select(Some("7")), Some("xxx"));
    }

    #[test]
    fn prompt_spec_indexed_selection() {
        let s = spec(r#"p = ["aaa", "bbb"]"#);
        assert_eq!(s.select(Some("0")), Some("aaa"));
        assert_eq!(s.select(Some("1")), Some("bbb"));
        // "01" parses as 1 — ordinary usize::from_str behavior, not special-cased.
        assert_eq!(s.select(Some("01")), Some("bbb"));
        // Every miss is None; the caller substitutes its built-in default.
        assert_eq!(s.select(None), None, "no variant selects nothing");
        assert_eq!(s.select(Some("5")), None, "out of range");
        assert_eq!(s.select(Some("a")), None, "non-numeric");
        assert_eq!(s.select(Some("-1")), None, "unparseable as usize");
    }

    #[test]
    fn prompt_spec_keyed_selection() {
        let s = spec(r#"p = { a = "aaa", b = "bbb", default = "ccc" }"#);
        assert_eq!(s.select(Some("a")), Some("aaa"));
        assert_eq!(s.select(Some("b")), Some("bbb"));
        // `default` is an ORDINARY key: it wins only on a literal "default".
        assert_eq!(s.select(Some("default")), Some("ccc"));
        assert_eq!(s.select(None), None, "no variant selects nothing");
        assert_eq!(s.select(Some("z")), None, "unknown key");
        assert_eq!(s.select(Some("A")), None, "key match is case-sensitive");
    }

    // ─── validate_prompt_variants ────────────────────────────────────────

    fn cfg(src: &str) -> ModelConfig {
        ModelConfig::from_toml_str(src).expect("parse ModelConfig")
    }

    #[test]
    fn variants_allowed_on_the_composer_task_only() {
        // Composer: array and table both accepted.
        assert!(cfg("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
                     filter_prompt = [\"aaa\", \"bbb\"]\n")
        .validate_prompt_variants()
        .is_ok());
        assert!(cfg("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
                     filter_prompt = { a = \"aaa\", b = \"bbb\" }\n")
        .validate_prompt_variants()
        .is_ok());
        // Any other task: rejected.
        let e = cfg("[tasks.chat_output_filter]\nmodel = \"m\"\n\
                     filter_prompt = [\"aaa\", \"bbb\"]\n")
        .validate_prompt_variants()
        .expect_err("non-composer variant must refuse to boot");
        assert!(e.contains("chat_output_filter"), "{e}");
        assert!(e.contains("refuses to boot"), "{e}");
    }

    #[test]
    fn plain_filter_prompts_always_validate() {
        assert!(cfg(
            "[tasks.chat_output_filter]\nmodel = \"m\"\nfilter_prompt = \"p\"\n\
                     [tasks.chat_output_filter.tiers.gold]\nfilter_prompt = \"tier p\"\n\
                     [tasks.world_reply]\nmodel = \"m\"\nfilter_prompt = \"   \"\n"
        )
        .validate_prompt_variants()
        .is_ok());
        // No filter_prompt anywhere is also fine.
        assert!(cfg("[tasks.chat_companion]\nmodel = \"m\"\n")
            .validate_prompt_variants()
            .is_ok());
    }

    #[test]
    fn tier_blocks_never_carry_variants() {
        // Rejected even under the composer task, because the composer resolves
        // with `tier = None` and could never select it.
        let e = cfg(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = \"p\"\n\
                     [tasks.chat_image_prompt_compose.tiers.gold]\n\
                     filter_prompt = [\"aaa\"]\n",
        )
        .validate_prompt_variants()
        .expect_err("tier variant must refuse to boot");
        assert!(e.contains("tiers.gold"), "{e}");
    }

    #[test]
    fn empty_variant_containers_refuse_to_boot() {
        for src in [
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = []\n",
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = {}\n",
        ] {
            let e = cfg(src)
                .validate_prompt_variants()
                .expect_err("empty variant container must refuse to boot");
            assert!(e.contains("empty"), "{e}");
        }
    }

    #[test]
    fn blank_variant_entries_refuse_to_boot() {
        let e = cfg("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
                     filter_prompt = [\"aaa\", \"   \"]\n")
        .validate_prompt_variants()
        .expect_err("blank array entry must refuse to boot");
        assert!(e.contains("index 1"), "{e}");

        let e = cfg("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
                     filter_prompt = { a = \"aaa\", b = \"  \" }\n")
        .validate_prompt_variants()
        .expect_err("blank table value must refuse to boot");
        assert!(e.contains("blank value for key \"b\""), "{e}");

        let e = cfg("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
                     filter_prompt = { \"  \" = \"aaa\" }\n")
        .validate_prompt_variants()
        .expect_err("blank table key must refuse to boot");
        assert!(e.contains("blank key"), "{e}");
    }

    #[test]
    fn raw_is_an_ordinary_variant_key_and_boots() {
        // "raw" used to be refused as a reserved wire escape. With no seed to
        // draw verbatim, the escape is gone and the key is configurable.
        for key in ["raw", "Raw", "RAW"] {
            let toml = format!(
                "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = {{ {key} = \"RAW PROMPT\" }}\n"
            );
            let cfg = ModelConfig::from_toml_str(&toml).expect("parses");
            cfg.validate_prompt_variants()
                .unwrap_or_else(|e| panic!("key {key:?} must boot now: {e}"));
        }
    }

    #[test]
    fn whitespace_padded_keyed_key_refuses_to_boot() {
        // A key like " a " parses and boots fine as TOML, but `select` matches
        // a client's (already-trimmed) `image.prompt_variant` exactly — so
        // neither "a" nor " a " could ever select it. Distinct from the
        // all-whitespace ("blank key") case above: this key is non-blank,
        // just padded, and the blank-key check must not misfire on it.
        let e = cfg("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
                     filter_prompt = { \" a \" = \"aaa\", b = \"bbb\" }\n")
        .validate_prompt_variants()
        .expect_err("whitespace-padded key must refuse to boot");
        assert!(e.contains("whitespace-padded"), "{e}");
        assert!(
            !e.contains("blank key"),
            "must not be reported as the blank-key case: {e}"
        );
    }

    // ─── validate_affinity_prompt_unset ──────────────────────────────────
    #[test]
    fn affinity_filter_prompt_absent_boots() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.affinity_evaluation]\nmodel = \"m\"\ntemperature = 0.3\n",
        )
        .unwrap();
        assert!(cfg.validate_affinity_prompt_unset().is_ok());
    }

    #[test]
    fn affinity_task_absent_boots() {
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel = \"m\"\n").unwrap();
        assert!(cfg.validate_affinity_prompt_unset().is_ok());
    }

    #[test]
    fn affinity_filter_prompt_set_refuses_to_boot() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.affinity_evaluation]\nmodel = \"m\"\nfilter_prompt = \"score the turn\"\n",
        )
        .unwrap();
        let err = cfg
            .validate_affinity_prompt_unset()
            .expect_err("a set filter_prompt must refuse to boot");
        assert!(
            err.contains("[tasks.affinity_evaluation].filter_prompt"),
            "{err}"
        );
        assert!(err.contains("refuses to boot"), "{err}");
        assert!(
            err.contains("issues/210"),
            "error must point at the issue: {err}"
        );
    }

    #[test]
    fn affinity_blank_filter_prompt_also_refuses_to_boot() {
        // Blank is NOT the lenient "commented out" case here: the key is dead
        // config in every shape, so silence would be the very failure #210 is
        // about.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.affinity_evaluation]\nmodel = \"m\"\nfilter_prompt = \"   \"\n",
        )
        .unwrap();
        assert!(cfg.validate_affinity_prompt_unset().is_err());
    }

    #[test]
    fn affinity_variant_shaped_filter_prompt_refuses_with_the_affinity_message() {
        // A table-shaped value would ALSO trip validate_prompt_variants, but
        // main runs this gate first so the operator is told the accurate
        // thing: affinity's prompt is not configurable at all.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.affinity_evaluation]\nmodel = \"m\"\nfilter_prompt = { a = \"X\" }\n",
        )
        .unwrap();
        let err = cfg
            .validate_affinity_prompt_unset()
            .expect_err("must refuse");
        assert!(err.contains("engine-owned"), "{err}");
    }

    // ─── validate_tier_blocks ────────────────────────────────────────────

    #[test]
    fn tier_blocks_boot_on_the_two_tier_consuming_tasks() {
        // chat_companion: resolve(task, tier). chat_output_filter:
        // resolve_output_filter(tier). Those two, and only those two, ever
        // reach a TierConfig.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel = \"m\"\n\
             [tasks.chat_companion.tiers.gold]\nmodel = \"m2\"\n\
             [tasks.chat_output_filter]\nmodel = \"m\"\nfilter_prompt = \"p\"\n\
             [tasks.chat_output_filter.tiers.gold]\nfilter_prompt = \"tier p\"\n",
        )
        .unwrap();
        assert!(cfg.validate_tier_blocks().is_ok());
    }

    #[test]
    fn no_tier_blocks_anywhere_boots() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel = \"m\"\n\
             [tasks.insight_extraction]\nmodel = \"m\"\nfilter_prompt = \"p\"\n",
        )
        .unwrap();
        assert!(cfg.validate_tier_blocks().is_ok());
    }

    #[test]
    fn tier_block_under_a_non_tiering_task_refuses_to_boot() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.insight_extraction]\nmodel = \"m\"\nfilter_prompt = \"p\"\n\
             [tasks.insight_extraction.tiers.premium]\nfilter_prompt = \"never read\"\n",
        )
        .unwrap();
        let err = cfg
            .validate_tier_blocks()
            .expect_err("a tier block under a non-tiering task must refuse to boot");
        assert!(
            err.contains("[tasks.insight_extraction.tiers.premium]"),
            "error must name task and tier: {err}"
        );
        assert!(err.contains("refuses to boot"), "{err}");
        assert!(
            err.contains("issues/215"),
            "error must point at the issue: {err}"
        );
    }

    #[test]
    fn tier_block_without_a_filter_prompt_also_refuses_to_boot() {
        // The whole block is unreachable, not just `filter_prompt` — model /
        // fallback / allow_traits / retry_depth / trigger / timing /
        // output_filter are equally dead there.
        for body in [
            "model = \"m2\"",
            "fallback = [\"m3\"]",
            "allow_traits = [\"nsfw_boost\"]",
            "retry_depth = 2",
        ] {
            let toml = format!(
                "[tasks.chat_voice]\nmodel = \"m\"\n[tasks.chat_voice.tiers.gold]\n{body}\n"
            );
            let cfg = ModelConfig::from_toml_str(&toml).unwrap();
            let err = match cfg.validate_tier_blocks() {
                Ok(()) => panic!("must refuse: {toml}"),
                Err(e) => e,
            };
            assert!(err.contains("[tasks.chat_voice.tiers.gold]"), "{err}");
        }
    }

    #[test]
    fn the_composers_own_tier_block_refuses_to_boot() {
        // resolve_image_prompt_compose takes no tier, and stream.rs resolves
        // the composer as resolve(COMPOSE_TASK, None) — the same deadness
        // validate_prompt_variants already calls out for variant shapes,
        // extended to every field.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = \"p\"\n\
             [tasks.chat_image_prompt_compose.tiers.gold]\nfilter_prompt = \"p2\"\n",
        )
        .unwrap();
        assert!(cfg.validate_tier_blocks().is_err());
    }

    #[test]
    fn tier_block_failure_is_deterministic() {
        // self.tasks is a HashMap; the reported pair must not depend on its
        // iteration order. Sorted-first task, then sorted-first tier.
        let toml = "[tasks.world_reply]\nmodel = \"m\"\nfilter_prompt = \"p\"\n\
                    [tasks.world_reply.tiers.zeta]\nmodel = \"m2\"\n\
                    [tasks.world_reply.tiers.alpha]\nmodel = \"m2\"\n\
                    [tasks.pde_decision]\nmodel = \"m\"\nfilter_prompt = \"p\"\n\
                    [tasks.pde_decision.tiers.gold]\nmodel = \"m2\"\n";
        for _ in 0..8 {
            let cfg = ModelConfig::from_toml_str(toml).unwrap();
            let err = cfg.validate_tier_blocks().expect_err("must refuse");
            assert!(err.contains("[tasks.pde_decision.tiers.gold]"), "{err}");
        }
    }

    #[test]
    fn embedding_tier_block_keeps_the_embedding_specific_message() {
        // `validate_providers` already refuses [tasks.embedding.tiers.*] with
        // a task-specific message, and `main` runs it BEFORE
        // `validate_tier_blocks` so the operator sees the specific reason
        // rather than the generic "never resolves with a tier" one. Same
        // ordering principle as validate_affinity_prompt_unset before
        // validate_prompt_variants.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.embedding]\nmodel = \"voyage-3-lite\"\n\
             [tasks.embedding.tiers.pro]\nmodel = \"voyage-4\"\n",
        )
        .unwrap();
        let specific = cfg
            .validate_providers_with(|_| Some("k".into()))
            .expect_err("embedding tiers must refuse to boot");
        assert!(
            specific.contains("not supported on the embedding task"),
            "{specific}"
        );
        // The generic gate also errors here — it just must not speak first.
        assert!(cfg.validate_tier_blocks().is_err());
    }

    #[test]
    fn tier_consuming_allowlist_is_exactly_the_two_resolvers() {
        assert_eq!(
            TIER_CONSUMING_TASKS,
            ["chat_companion", "chat_output_filter"],
            "adding a task here requires a resolver that actually passes a tier"
        );
    }

    // ─── resolve_image_prompt_compose: variants + raw ────────────────────

    #[test]
    fn compose_indexed_variant_selection() {
        let c = cfg("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
                     filter_prompt = [\"AAA\", \"BBB\"]\n");
        assert_eq!(
            c.resolve_image_prompt_compose(Some("1"))
                .unwrap()
                .compose_prompt,
            "BBB"
        );
        // No variant, and every miss, fall through to the built-in prompt.
        for v in [None, Some("5"), Some("a"), Some("-1")] {
            assert_eq!(
                c.resolve_image_prompt_compose(v).unwrap().compose_prompt,
                DEFAULT_COMPOSE_PROMPT,
                "variant {v:?} should fall back to the built-in prompt"
            );
        }
    }

    #[test]
    fn compose_keyed_variant_selection() {
        let c = cfg("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
                     filter_prompt = { a = \"AAA\", default = \"CCC\" }\n");
        assert_eq!(
            c.resolve_image_prompt_compose(Some("a"))
                .unwrap()
                .compose_prompt,
            "AAA"
        );
        // `default` is an ordinary key — it needs a literal "default" to win.
        assert_eq!(
            c.resolve_image_prompt_compose(Some("default"))
                .unwrap()
                .compose_prompt,
            "CCC"
        );
        for v in [None, Some("z")] {
            assert_eq!(
                c.resolve_image_prompt_compose(v).unwrap().compose_prompt,
                DEFAULT_COMPOSE_PROMPT
            );
        }
    }

    #[test]
    fn compose_plain_prompt_ignores_the_variant() {
        let c = cfg("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
                     filter_prompt = \"custom composer\"\n");
        for v in [None, Some("a"), Some("3")] {
            assert_eq!(
                c.resolve_image_prompt_compose(v).unwrap().compose_prompt,
                "custom composer"
            );
        }
    }

    #[test]
    fn compose_blank_plain_prompt_still_falls_back() {
        // Pre-existing behavior, unchanged: a blank plain string is "commented
        // out", not a boot failure.
        let c = cfg("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = \"   \"\n");
        assert_eq!(
            c.resolve_image_prompt_compose(None).unwrap().compose_prompt,
            DEFAULT_COMPOSE_PROMPT
        );
    }

    #[test]
    fn raw_variant_selects_a_configured_prompt() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = { raw = \"RAW PROMPT\" }\n",
        )
        .expect("parses");
        let r = cfg
            .resolve_image_prompt_compose(Some("raw"))
            .expect("composer resolves — \"raw\" no longer skips it");
        assert_eq!(r.compose_prompt, "RAW PROMPT");
        assert_eq!(r.variant_key.as_deref(), Some("raw"));
    }

    #[test]
    fn raw_variant_without_a_matching_key_falls_back_to_the_builtin() {
        // An unknown variant is a miss, never a skip and never an error.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = { a = \"A PROMPT\" }\n",
        )
        .expect("parses");
        let r = cfg
            .resolve_image_prompt_compose(Some("raw"))
            .expect("composer still resolves");
        assert_eq!(r.compose_prompt, DEFAULT_COMPOSE_PROMPT);
        assert_eq!(r.variant_key, None);
    }

    #[test]
    fn compose_resolves_none_only_when_the_task_is_absent() {
        let absent =
            ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel = \"m\"\n").expect("parses");
        assert!(absent.resolve_image_prompt_compose(None).is_none());
        assert!(absent.resolve_image_prompt_compose(Some("raw")).is_none());

        let present =
            ModelConfig::from_toml_str("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n")
                .expect("parses");
        assert!(present.resolve_image_prompt_compose(None).is_some());
        assert!(present.resolve_image_prompt_compose(Some("raw")).is_some());
        assert!(present
            .resolve_image_prompt_compose(Some("anything"))
            .is_some());
    }

    #[test]
    fn no_variant_uses_the_builtin_for_variant_shapes() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = { a = \"A PROMPT\" }\n",
        )
        .expect("parses");
        let r = cfg.resolve_image_prompt_compose(None).expect("resolves");
        assert_eq!(r.compose_prompt, DEFAULT_COMPOSE_PROMPT);

        // A PLAIN filter_prompt is the deployment's single chosen prompt, not a
        // variant miss — it still wins with no variant supplied.
        let plain = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\nfilter_prompt = \"PLAIN PROMPT\"\n",
        )
        .expect("parses");
        assert_eq!(
            plain
                .resolve_image_prompt_compose(None)
                .unwrap()
                .compose_prompt,
            "PLAIN PROMPT"
        );
    }

    #[test]
    fn blank_variant_string_is_treated_as_absent() {
        let c = cfg("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
                     filter_prompt = [\"AAA\", \"BBB\"]\n");
        assert_eq!(
            c.resolve_image_prompt_compose(Some("   "))
                .unwrap()
                .compose_prompt,
            DEFAULT_COMPOSE_PROMPT
        );
    }

    /// `variant_key` surfaces WHICH variant selected the prompt — the audit
    /// value persisted to `metadata.image.compose_variant`. `Some` iff a
    /// Keyed/Indexed entry was actually hit; `Plain` and the built-in
    /// fallback have a single prompt (no "which variant" to answer).
    #[test]
    fn resolve_image_prompt_compose_variant_key_semantics() {
        // Keyed: hit → Some(key); miss → None (built-in prompt fallback).
        let keyed = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
             filter_prompt = { a = \"AAA\", b = \"BBB\" }\n",
        )
        .unwrap();
        assert_eq!(
            keyed
                .resolve_image_prompt_compose(Some("b"))
                .unwrap()
                .variant_key,
            Some("b".to_string())
        );
        assert_eq!(
            keyed
                .resolve_image_prompt_compose(Some("zzz"))
                .unwrap()
                .variant_key,
            None,
            "miss falls back to the built-in prompt — no variant to audit"
        );
        assert_eq!(
            keyed
                .resolve_image_prompt_compose(None)
                .unwrap()
                .variant_key,
            None
        );
        // Whitespace-padded variant is trimmed before select — key must match.
        assert_eq!(
            keyed
                .resolve_image_prompt_compose(Some(" b \n"))
                .unwrap()
                .variant_key,
            Some("b".to_string())
        );

        // Indexed: hit → Some(index string).
        let indexed = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
             filter_prompt = [\"ZERO\", \"ONE\"]\n",
        )
        .unwrap();
        assert_eq!(
            indexed
                .resolve_image_prompt_compose(Some("1"))
                .unwrap()
                .variant_key,
            Some("1".to_string())
        );
        assert_eq!(
            indexed
                .resolve_image_prompt_compose(Some("9"))
                .unwrap()
                .variant_key,
            None
        );

        // Plain: always None, even when a variant was supplied (select() returns
        // the prompt regardless — there is no variant selection to audit).
        let plain = ModelConfig::from_toml_str(
            "[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n\
             filter_prompt = \"PLAIN\"\n",
        )
        .unwrap();
        assert_eq!(
            plain
                .resolve_image_prompt_compose(Some("b"))
                .unwrap()
                .variant_key,
            None
        );

        // No filter_prompt at all → built-in prompt, no variant.
        let bare = ModelConfig::from_toml_str("[tasks.chat_image_prompt_compose]\nmodel = \"m\"\n")
            .unwrap();
        assert_eq!(
            bare.resolve_image_prompt_compose(None).unwrap().variant_key,
            None
        );
    }

    // ─── compose_variant_log_event: the warn/debug guards, pinned directly ──
    // (no tracing subscriber needed — the decision is a pure function)

    #[test]
    fn compose_variant_log_event_warns_only_on_an_explicit_unmatched_variant() {
        // The common case: no prompt_variant was supplied at all. Falling
        // back to the built-in prompt is silent — nothing was asked for, so
        // nothing failed to be found.
        assert_eq!(compose_variant_log_event(None, None, true), None);
        // A container exists but no filter_prompt at all was configured —
        // can't happen via the public resolver (selected implies a
        // filter_prompt), but the guard must not fire even so.
        assert_eq!(compose_variant_log_event(None, Some("z"), false), None);
        // The one case that DOES warn: a variant was supplied, a
        // filter_prompt container exists, and nothing matched.
        assert_eq!(
            compose_variant_log_event(None, Some("z"), true),
            Some(ComposeVariantLogEvent::Mismatch)
        );
    }

    #[test]
    fn compose_variant_log_event_debug_only_when_a_variant_was_supplied_and_matched() {
        assert_eq!(
            compose_variant_log_event(Some("AAA"), Some("0"), true),
            Some(ComposeVariantLogEvent::Selected)
        );
        // No variant supplied ⇒ nothing to report, even though `selected` is
        // `Some` (e.g. a Plain filter_prompt, which always resolves).
        assert_eq!(compose_variant_log_event(Some("AAA"), None, true), None);
    }

    // ─── cap_for_log ──────────────────────────────────────────────────────

    #[test]
    fn cap_for_log_passes_short_strings_through_unchanged() {
        assert_eq!(cap_for_log("abc", 64), "abc");
        assert_eq!(cap_for_log("", 64), "");
        // Exactly at the cap: no truncation marker.
        assert_eq!(cap_for_log("aaaa", 4), "aaaa");
    }

    #[test]
    fn cap_for_log_truncates_on_a_char_boundary_and_marks_it() {
        assert_eq!(cap_for_log("aaaaa", 4), "aaaa…");
        // Multi-byte chars: counted/truncated by char, never mid-codepoint
        // (which would panic on a byte-index slice).
        let s = "你好世界和平"; // 6 chars
        assert_eq!(cap_for_log(s, 3), "你好世…");
    }

    // ---- [providers] block + validate_providers (multi-provider spec §1/§7) ----

    /// env closure: no provider key exists.
    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn providers_table_value_parses() {
        let cfg = ModelConfig::from_toml_str(
            "[providers]\n\
             venice = { chat = \"https://api.venice.ai/api/v1/chat/completions\" }\n\
             local  = { embeddings = \"http://127.0.0.1:8080/v1/embeddings\" }\n\
             [providers.proxy]\n\
             chat    = \"https://proxy.internal/v1/chat/completions\"\n\
             headers = { \"X-Team\" = \"companion\" }\n",
        )
        .unwrap();
        assert_eq!(
            cfg.providers["venice"].chat.as_deref(),
            Some("https://api.venice.ai/api/v1/chat/completions")
        );
        assert!(cfg.providers["venice"].embeddings.is_none());
        assert_eq!(
            cfg.providers["local"].embeddings.as_deref(),
            Some("http://127.0.0.1:8080/v1/embeddings")
        );
        assert_eq!(
            cfg.providers["proxy"].headers.as_ref().unwrap()["X-Team"],
            "companion"
        );
    }

    #[test]
    fn providers_string_value_is_rejected() {
        // 0.9.3's plain-string shape is dropped with no compat layer (spec §0).
        // The error must teach the table form, not surface a generic serde
        // "expected struct ProviderEntry" message.
        let err = ModelConfig::from_toml_str(
            "[providers]\nvenice = \"https://api.venice.ai/api/v1/chat/completions\"\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("[providers] values must be tables"),
            "error should teach the table form: {msg}"
        );
        assert!(
            msg.contains("chat = ") && msg.contains("embeddings = ") && msg.contains("headers ="),
            "error should show the table shape (chat/embeddings/headers): {msg}"
        );
        assert!(
            msg.contains("0.9.3 string form was removed"),
            "error should explain why the old shape no longer works: {msg}"
        );
        assert!(
            !msg.contains("expected struct ProviderEntry"),
            "error should not leak the generic serde message: {msg}"
        );
    }

    #[test]
    fn providers_unknown_key_is_rejected() {
        assert!(ModelConfig::from_toml_str(
            "[providers]\nvenice = { chat = \"https://x/v1/chat/completions\", api_key = \"nope\" }\n",
        )
        .is_err());
    }

    #[test]
    fn providers_absent_is_empty_and_valid() {
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel=\"m\"\n").unwrap();
        assert!(cfg.providers.is_empty());
        // No [tasks.embedding] ⇒ resolves to the default Voyage route, which
        // now needs VOYAGE_API_KEY.
        assert!(cfg
            .validate_providers_with(|k| (k == "VOYAGE_API_KEY").then(|| "k".to_string()))
            .is_ok());
    }

    #[test]
    fn providers_openrouter_entry_is_now_legal() {
        let cfg = ModelConfig::from_toml_str(
            "[providers]\nopenrouter = { embeddings = \"http://proxy/v1/embeddings\" }\n",
        )
        .unwrap();
        assert!(cfg
            .validate_providers_with(|k| (k == "VOYAGE_API_KEY").then(|| "k".to_string()))
            .is_ok());
    }

    #[test]
    fn provider_name_voyage_is_reserved() {
        let cfg = ModelConfig::from_toml_str("[providers]\nvoyage = { chat = \"https://x/v1\" }\n")
            .unwrap();
        let msg = cfg.validate_providers_with(no_env).unwrap_err();
        assert!(msg.contains("reserved"));
        assert!(msg.contains("VOYAGE_API_KEY"));
    }

    #[test]
    fn provider_name_charset_is_constrained() {
        // Dash is rejected: the name uppercases into an env var, no mangling.
        let cfg = ModelConfig::from_toml_str(
            "[providers]\n\"venice-ai\" = { chat = \"https://x/v1\" }\n",
        )
        .unwrap();
        let msg = cfg.validate_providers_with(no_env).unwrap_err();
        assert!(msg.contains("a-z0-9_"), "should state the charset: {msg}");
    }

    #[test]
    fn provider_empty_url_refuses_boot() {
        let cfg = ModelConfig::from_toml_str("[providers]\nvenice = { chat = \"\" }\n").unwrap();
        assert!(cfg
            .validate_providers_with(no_env)
            .unwrap_err()
            .contains("empty"));
    }

    #[test]
    fn unreferenced_provider_needs_no_key() {
        // The examples/ config ships a sample [providers] entry; deployments
        // that copy it without using it must still boot.
        let cfg = ModelConfig::from_toml_str(
            "[providers]\nvenice = { chat = \"https://x/v1\" }\n[tasks.chat_companion]\nmodel=\"plain/m\"\n",
        )
        .unwrap();
        // No [tasks.embedding] ⇒ resolves to the default Voyage route, which
        // now needs VOYAGE_API_KEY.
        assert!(cfg
            .validate_providers_with(|k| (k == "VOYAGE_API_KEY").then(|| "k".to_string()))
            .is_ok());
    }

    #[test]
    fn referenced_provider_missing_key_refuses_boot() {
        let cfg = ModelConfig::from_toml_str(
            "[providers]\nvenice = { chat = \"https://x/v1\" }\n[tasks.chat_companion]\nmodel=\"m@venice\"\n",
        )
        .unwrap();
        let msg = cfg.validate_providers_with(no_env).unwrap_err();
        assert!(
            msg.contains("VENICE_API_KEY"),
            "must name the env var: {msg}"
        );
    }

    #[test]
    fn referenced_provider_empty_key_refuses_boot() {
        let cfg = ModelConfig::from_toml_str(
            "[providers]\nvenice = { chat = \"https://x/v1\" }\n[tasks.chat_companion]\nmodel=\"m@venice\"\n",
        )
        .unwrap();
        let env = |k: &str| (k == "VENICE_API_KEY").then(String::new);
        assert!(cfg.validate_providers_with(env).is_err());
    }

    #[test]
    fn referenced_provider_with_key_is_green() {
        let cfg = ModelConfig::from_toml_str(
            "[providers]\nvenice = { chat = \"https://x/v1\" }\n[tasks.chat_companion]\nmodel=\"m@venice\"\nfallback=[\"other/m\"]\n",
        )
        .unwrap();
        // No [tasks.embedding] ⇒ resolves to the default Voyage route, which
        // now needs VOYAGE_API_KEY alongside VENICE_API_KEY.
        let env =
            |k: &str| (k == "VENICE_API_KEY" || k == "VOYAGE_API_KEY").then(|| "sk-v".to_string());
        assert!(cfg.validate_providers_with(env).is_ok());
    }

    #[test]
    fn undeclared_provider_in_slug_refuses_boot() {
        // Also the escape-typo case: "aaa@bb" points at the \@ escape.
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel=\"aaa@bb\"\n").unwrap();
        let msg = cfg.validate_providers_with(no_env).unwrap_err();
        assert!(msg.contains("`bb`"));
        assert!(msg.contains("\\@"), "should teach the escape: {msg}");
    }

    #[test]
    fn slug_grammar_error_propagates_with_location() {
        let cfg =
            ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel=\"a@b@venice\"\n").unwrap();
        let msg = cfg.validate_providers_with(no_env).unwrap_err();
        assert!(
            msg.contains("[tasks.chat_companion].model"),
            "location: {msg}"
        );
    }

    #[test]
    fn weighted_table_keys_are_all_scanned() {
        // resolve() picks at random — validation must see every key.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel = { \"good/m\" = 0.9, \"bad@nope\" = 0.1 }\n",
        )
        .unwrap();
        assert!(cfg
            .validate_providers_with(no_env)
            .unwrap_err()
            .contains("nope"));
    }

    #[test]
    fn tier_fallback_is_scanned() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel=\"m\"\n[tasks.chat_companion.tiers.premium]\nfallback=[\"x@nope\"]\n",
        )
        .unwrap();
        let msg = cfg.validate_providers_with(no_env).unwrap_err();
        assert!(msg.contains("[tasks.chat_companion.tiers.premium].fallback"));
    }

    #[test]
    fn defaults_fallback_model_is_scanned() {
        let cfg = ModelConfig::from_toml_str("[defaults]\nfallback_model=\"x@nope\"\n").unwrap();
        let msg = cfg.validate_providers_with(no_env).unwrap_err();
        assert!(msg.contains("[defaults].fallback_model"));
    }

    #[test]
    fn compose_task_accepts_provider_suffix() {
        // Regression lock: the compose task must keep accepting `@provider` —
        // it is an ordinary chat-shaped task, not an OpenRouter-only executor.
        let cfg = ModelConfig::from_toml_str(
            "[providers]\nvenice = { chat = \"https://x/v1\" }\n[tasks.chat_image_prompt_compose]\nmodel=\"m@venice\"\nfilter_prompt=\"compose it\"\n",
        )
        .unwrap();
        // No [tasks.embedding] ⇒ resolves to the default Voyage route, which
        // now needs VOYAGE_API_KEY alongside VENICE_API_KEY.
        let env =
            |k: &str| (k == "VENICE_API_KEY" || k == "VOYAGE_API_KEY").then(|| "sk-v".to_string());
        assert!(cfg.validate_providers_with(env).is_ok());
    }

    #[test]
    fn removed_image_generation_task_refuses_boot() {
        let cfg =
            ModelConfig::from_toml_str("[tasks.chat_image_generation]\nmodel=\"img/m\"\n").unwrap();
        let err = cfg.validate_removed_tasks().unwrap_err();
        assert!(
            err.contains("chat_image_generation"),
            "message names the block: {err}"
        );
        assert!(
            err.contains("image_request"),
            "message points at the delegation frame: {err}"
        );
    }

    #[test]
    fn validate_removed_tasks_ok_without_block() {
        let cfg = ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel=\"x\"\n").unwrap();
        assert!(cfg.validate_removed_tasks().is_ok());
    }

    #[test]
    fn build_providers_maps_declared_entries() {
        let cfg = ModelConfig::from_toml_str(
            "[providers.venice]\n\
             chat = \"https://x/v1\"\n\
             [providers.other]\n\
             chat = \"https://y/v1\"\n\
             [[providers.venice.body]]\n\
             params = { reasoning = { max_tokens = 64 } }\n",
        )
        .unwrap();
        let env = |k: &str| (k == "VENICE_API_KEY").then(|| "sk-v".to_string());
        let map = cfg.build_providers_with(env);
        assert_eq!(map["venice"].base_url, "https://x/v1");
        assert_eq!(map["venice"].api_key, "sk-v");
        // Unreferenced/keyless entry still present, with an empty key —
        // resolve_endpoint's runtime guard covers the (unreachable) miss.
        assert_eq!(map["other"].api_key, "");
        // build_providers_with carries the parsed [[providers.<name>.body]]
        // rules onto ProviderEndpoint.body_rules verbatim.
        assert_eq!(map["venice"].body_rules.len(), 1);
        assert_eq!(
            map["venice"].body_rules[0].params["reasoning"]["max_tokens"],
            64
        );
        assert!(map["other"].body_rules.is_empty(), "no body ⇒ empty vec");
    }

    #[test]
    fn openrouter_body_rules_returns_parsed_rules_or_empty() {
        let with_rules = ModelConfig::from_toml_str(
            "[providers.openrouter]\n\
             [[providers.openrouter.body]]\n\
             params = { transforms = [\"middle-out\"] }\n",
        )
        .unwrap();
        let rules = with_rules.openrouter_body_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].params["transforms"][0], "middle-out");

        let without = ModelConfig::from_toml_str("").unwrap();
        assert!(without.openrouter_body_rules().is_empty());
    }

    #[test]
    fn providers_empty_entry_is_rejected() {
        let cfg = ModelConfig::from_toml_str("[providers]\nvenice = {}\n").unwrap();
        let msg = cfg.validate_providers_with(|_| None).unwrap_err();
        assert!(msg.contains("[providers].venice"), "{msg}");
    }

    #[test]
    fn providers_reserved_header_names_are_rejected() {
        for header in ["Authorization", "authorization", "Content-Type"] {
            let cfg = ModelConfig::from_toml_str(&format!(
                "[providers]\nvenice = {{ chat = \"https://x/c\", headers = {{ \"{header}\" = \"boom\" }} }}\n",
            ))
            .unwrap();
            let msg = cfg.validate_providers_with(|_| None).unwrap_err();
            assert!(msg.contains("engine-owned"), "{header}: {msg}");
        }
    }

    #[test]
    fn providers_invalid_header_value_is_rejected() {
        let cfg = ModelConfig::from_toml_str(
            "[providers]\nvenice = { chat = \"https://x/c\", headers = { \"X-Bad\" = \"line\\nbreak\" } }\n",
        )
        .unwrap();
        assert!(cfg.validate_providers_with(|_| None).is_err());
    }

    #[test]
    fn build_providers_skips_embedding_only_entries_and_carries_headers() {
        let cfg = ModelConfig::from_toml_str(
            "[providers]\n\
             venice = { chat = \"https://x/c\", headers = { \"X-Team\" = \"companion\" } }\n\
             local  = { embeddings = \"http://e/v1/embeddings\" }\n",
        )
        .unwrap();
        let map = cfg.build_providers_with(|_| Some("k".to_string()));
        assert!(map.contains_key("venice"));
        assert!(!map.contains_key("local"));
        assert_eq!(map["venice"].headers.get("X-Team").unwrap(), "companion");
    }

    #[test]
    fn provider_body_rules_parse() {
        let cfg = ModelConfig::from_toml_str(
            "[providers.venice]\nchat = \"https://v/chat\"\n\
             [[providers.venice.body]]\ntasks = [\"chat_companion\"]\n\
             params = { venice_parameters = { include_venice_system_prompt = false } }\n\
             [[providers.venice.body]]\n\
             params = { reasoning = { max_tokens = 512 } }\n",
        )
        .unwrap();
        let body = cfg.providers["venice"].body.as_ref().unwrap();
        assert_eq!(body.len(), 2);
        assert_eq!(
            body[0].tasks.as_deref(),
            Some(&["chat_companion".to_string()][..])
        );
        assert_eq!(
            body[0].params["venice_parameters"]["include_venice_system_prompt"],
            serde_json::Value::Bool(false)
        );
        assert!(body[1].tasks.is_none(), "omitted tasks parses as None");
        assert_eq!(body[1].params["reasoning"]["max_tokens"], 512);
    }

    #[test]
    fn provider_body_rule_unknown_key_is_rejected() {
        let err = ModelConfig::from_toml_str(
            "[providers.venice]\nchat = \"https://v/chat\"\n\
             [[providers.venice.body]]\nparam = { a = 1 }\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("`param`"),
            "unknown rule key must be named: {err}"
        );
    }

    #[test]
    fn provider_entry_with_only_body_is_not_empty() {
        let cfg = ModelConfig::from_toml_str(
            "[[providers.openrouter.body]]\nparams = { transforms = [\"middle-out\"] }\n",
        )
        .unwrap();
        assert!(!cfg.providers["openrouter"].is_empty());
    }

    // ---- [tasks.embedding] routing + resolve_embedding (spec 2026-08-01-embedding-providers §2/§3/§6) ----

    /// env closure granting every key, for tests that target non-key rules.
    fn env_all(k: &str) -> Option<String> {
        let _ = k;
        Some("key".to_string())
    }

    #[test]
    fn at_openrouter_alias_passes_on_chat_tasks() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.chat_companion]\nmodel = \"x-ai/grok-4.20@openrouter\"\n\
             fallback = [\"deepseek/deepseek-v4-flash@openrouter\"]\n",
        )
        .unwrap();
        // No [tasks.embedding] ⇒ resolves to the default Voyage route, which
        // needs VOYAGE_API_KEY — this test targets the @openrouter alias, not
        // the key check.
        assert!(cfg
            .validate_providers_with(|k| (k == "VOYAGE_API_KEY").then(|| "k".to_string()))
            .is_ok());
    }

    #[test]
    fn at_voyage_on_chat_task_is_rejected() {
        let cfg =
            ModelConfig::from_toml_str("[tasks.chat_companion]\nmodel = \"voyage-4@voyage\"\n")
                .unwrap();
        let msg = cfg.validate_providers_with(env_all).unwrap_err();
        assert!(
            msg.contains("embedding"),
            "should say voyage is embeddings-only: {msg}"
        );
    }

    #[test]
    fn chat_ref_to_embeddings_only_entry_is_rejected() {
        let cfg = ModelConfig::from_toml_str(
            "[providers]\nlocal = { embeddings = \"http://e/v1/embeddings\" }\n\
             [tasks.chat_companion]\nmodel = \"m@local\"\n",
        )
        .unwrap();
        let msg = cfg.validate_providers_with(env_all).unwrap_err();
        assert!(msg.contains("chat"), "{msg}");
    }

    #[test]
    fn embedding_ref_to_chat_only_entry_is_rejected() {
        let cfg = ModelConfig::from_toml_str(
            "[providers]\nvenice = { chat = \"https://x/c\" }\n\
             [tasks.embedding]\nmodel = \"bge-m3@venice\"\n",
        )
        .unwrap();
        let msg = cfg.validate_providers_with(env_all).unwrap_err();
        assert!(msg.contains("embeddings"), "{msg}");
    }

    #[test]
    fn embedding_bare_model_means_voyage_and_needs_voyage_key() {
        let cfg =
            ModelConfig::from_toml_str("[tasks.embedding]\nmodel = \"voyage-3-lite\"\n").unwrap();
        // No VOYAGE_API_KEY in env ⇒ refuse.
        let msg = cfg.validate_providers_with(|_| None).unwrap_err();
        assert!(msg.contains("VOYAGE_API_KEY"), "{msg}");
        // With the key ⇒ pass.
        assert!(cfg
            .validate_providers_with(|k| (k == "VOYAGE_API_KEY").then(|| "k".to_string()))
            .is_ok());
    }

    #[test]
    fn embedding_at_openrouter_does_not_need_voyage_key() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.embedding]\nmodel = \"openai/text-embedding-3-small@openrouter\"\n",
        )
        .unwrap();
        assert!(cfg.validate_providers_with(|_| None).is_ok());
    }

    #[test]
    fn embedding_model_and_pair_are_mutually_exclusive() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.embedding]\nmodel = \"voyage-3-lite\"\nmodel_read = \"voyage-4-lite\"\nmodel_write = \"voyage-4\"\n",
        )
        .unwrap();
        assert!(cfg.validate_providers_with(env_all).is_err());
    }

    #[test]
    fn embedding_half_pair_is_rejected() {
        let cfg = ModelConfig::from_toml_str("[tasks.embedding]\nmodel_read = \"voyage-4-lite\"\n")
            .unwrap();
        let msg = cfg.validate_providers_with(env_all).unwrap_err();
        assert!(msg.contains("model_write"), "{msg}");
    }

    #[test]
    fn embedding_pair_below_voyage_4_is_rejected() {
        for bad in [
            "voyage-3.5-lite",
            "voyage-code-3",
            "bge-m3",
            "voyage-inf",
            "voyage-4e2",
        ] {
            let cfg = ModelConfig::from_toml_str(&format!(
                "[tasks.embedding]\nmodel_read = \"{bad}\"\nmodel_write = \"voyage-4\"\n"
            ))
            .unwrap();
            assert!(
                cfg.validate_providers_with(env_all).is_err(),
                "{bad} must refuse"
            );
        }
    }

    #[test]
    fn embedding_pair_rejects_non_voyage_provider_suffix() {
        for bad in ["voyage-4@openrouter", "voyage-4@local"] {
            let cfg = ModelConfig::from_toml_str(&format!(
                "[providers]\nlocal = {{ embeddings = \"http://e/v1/embeddings\" }}\n\
                 [tasks.embedding]\nmodel_read = \"{bad}\"\nmodel_write = \"voyage-4\"\n"
            ))
            .unwrap();
            assert!(
                cfg.validate_providers_with(env_all).is_err(),
                "{bad} must refuse"
            );
        }
    }

    #[test]
    fn embedding_pair_accepts_explicit_at_voyage() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.embedding]\nmodel_read = \"voyage-4-lite@voyage\"\nmodel_write = \"voyage-4\"\n",
        )
        .unwrap();
        assert!(cfg
            .validate_providers_with(|k| (k == "VOYAGE_API_KEY").then(|| "k".to_string()))
            .is_ok());
    }

    #[test]
    fn embedding_model_rejects_round_robin_weighted_fallback_tiers() {
        for toml in [
            "[tasks.embedding]\nmodel = [\"voyage-3-lite\", \"voyage-4\"]\n",
            "[tasks.embedding]\nmodel = { \"voyage-3-lite\" = 0.5, \"voyage-4\" = 0.5 }\n",
            "[tasks.embedding]\nmodel = \"voyage-3-lite\"\nfallback = \"voyage-4\"\n",
            "[tasks.embedding]\nmodel = \"voyage-3-lite\"\n[tasks.embedding.tiers.pro]\nmodel = \"voyage-4\"\n",
        ] {
            let cfg = ModelConfig::from_toml_str(toml).unwrap();
            assert!(
                cfg.validate_providers_with(env_all).is_err(),
                "must refuse: {toml}"
            );
        }
    }

    #[test]
    fn stale_dimensions_key_is_an_ignored_unknown_field() {
        // `dimensions` was removed from `TaskConfig` (spec
        // 2026-08-01-embedding-providers §0: dims are hard-coded 512, no
        // config knob). `TaskConfig` doesn't `deny_unknown_fields`, so a
        // leftover `dimensions = 512` line from a pre-removal config must
        // still parse — same compat contract as any other stale key.
        let cfg = ModelConfig::from_toml_str(
            "[tasks.embedding]\nmodel = \"voyage-3-lite\"\ndimensions = 512\n",
        )
        .expect("stale `dimensions` key must not break parsing");
        assert_eq!(
            cfg.tasks["embedding"].model.as_fixed(),
            Some("voyage-3-lite")
        );
    }

    #[test]
    fn resolve_embedding_defaults_to_voyage_4_lite() {
        for toml in ["", "[tasks.embedding]\n"] {
            let cfg = ModelConfig::from_toml_str(toml).unwrap();
            let r = cfg.resolve_embedding();
            assert_eq!(r.read.model, "voyage-4-lite");
            assert_eq!(r.write.model, "voyage-4-lite");
            assert!(matches!(r.read.route, EmbedRoute::Voyage));
            assert!(matches!(r.write.route, EmbedRoute::Voyage));
        }
    }

    #[test]
    fn resolve_embedding_single_model_routes() {
        let cfg = ModelConfig::from_toml_str(
            "[providers]\nlocal = { embeddings = \"http://e/v1/embeddings\" }\n\
             [tasks.embedding]\nmodel = \"bge-m3@local\"\n",
        )
        .unwrap();
        let r = cfg.resolve_embedding();
        assert_eq!(r.read.model, "bge-m3");
        assert!(matches!(r.read.route, EmbedRoute::Custom(ref n) if n == "local"));
        assert_eq!(r.write.model, "bge-m3");
    }

    #[test]
    fn resolve_embedding_pair_splits_read_write() {
        let cfg = ModelConfig::from_toml_str(
            "[tasks.embedding]\nmodel_read = \"voyage-4-lite\"\nmodel_write = \"voyage-4\"\n",
        )
        .unwrap();
        let r = cfg.resolve_embedding();
        assert_eq!(r.read.model, "voyage-4-lite");
        assert_eq!(r.write.model, "voyage-4");
        assert!(matches!(r.read.route, EmbedRoute::Voyage));
    }
}

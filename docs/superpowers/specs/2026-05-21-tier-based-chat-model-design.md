# Tier-Based Chat Model & Trait Gating — Design

**Status:** Draft for review
**Date:** 2026-05-21
**Owner:** @enriquephl

## Problem

The downstream already has a per-user **tier** mechanism (e.g. `free` / `gold`).
A common monetization pattern is to hand different tiers different model
quality and different content permissions. The engine today has no notion of
tier: `model_config.resolve("chat_companion", persona_override)` returns one
model/fallback for everyone, and caller-supplied `prompt_traits` are injected
**without any gating** (`docs/prompt-traits.md` says allow-listing is the
caller's job).

We want, for the `chat_companion` task only:

1. **Per-tier (primary, fallback) model selection** — each tier maps to its own
   `model` + `fallback` chain.
2. **Per-tier prompt-trait allow-listing** — each tier declares which trait
   `tag`s it permits; traits outside that list are dropped (not injected) while
   the reply is still generated normally.

Tier names and `allow_traits` names are **downstream-defined**; the only
contract is that the `tier` string a request carries matches a key in
`model_config.toml`.

## Decisions (from brainstorming)

1. **`tier` arrives as a request body field.** New `tier: Option<String>` on
   the chat request, threaded like `prompt_traits` / `audit` (per-request
   passthrough). Same plumbing pattern, no auth/JWT or DB coupling.
2. **`allow_traits` is a per-tier whitelist of trait tags; over-permission tags
   are silently dropped.** If a (possibly misbehaving) client force-sends a tag
   the tier does not allow, the engine drops only that trait and **still returns
   a normal reply** (the dropped trait's text is simply not injected). Dropped
   tags are logged. This is a server-side guard; the frontend also blocks.
3. **`persona_override` is removed entirely.** The engine no longer reads
   `genome.art_metadata.model`. Rationale: model slugs churn fast so a pin is
   unreliable, and `persona_override` was originally a user-layering lever that
   `tier` + `prompt_traits` now supersede. The `art_metadata.model` field may
   remain in persona JSONB (downstream data) — the engine just ignores it.

### Scope note

This is a **breaking change** (removes `persona_override`; prompt-traits become
subject to gating). There is a single downstream (eros-chat), owned by us, that
will always send `tier`. Preserving other hypothetical downstreams is explicitly
**not** a goal. Ship behind a normal minor version bump + changelog note.

## Non-Goals

- **No per-tier `temperature` / `max_tokens` in v1.** Only `model` / `fallback`
  / `allow_traits` vary by tier. `temperature` / `max_tokens` stay task-level
  (shared across tiers). Schema stays forward-compatible if we add them later.
- **No tier awareness for non-chat tasks.** `insight_extraction`,
  `memory_extraction`, `affinity_evaluation`, `pde_decision`, `embedding` are
  backend/post-process tasks with no user tier relevance; they resolve with
  `tier = None`.
- **No tier awareness for the Gift / Proactive paths in v1.** `Event::Gift`
  carries no tier, so gift reactions resolve via the default block. (The gift
  path injects no `prompt_traits`, so trait gating is moot there.)
- **No persistence.** `tier` is ephemeral per-request, like `prompt_traits`.
  Not written to any table; no migration.
- **No change to `prompt_traits` validation rules** (count ≤ 8, tag regex,
  text caps). Gating happens *after* validation, at resolve time.

## Design

### 1. Config schema — nested tables keyed by tier name

Tiers live as sub-tables under the task. The **top-level `[tasks.chat_companion]`
block is the default** used when no `tier` is sent or the `tier` does not match a
configured key.

```toml
[tasks.chat_companion]
# ── default block: used when tier is absent or unmatched ──
model        = "x-ai/grok-4.20"
fallback     = ["thedrummer/cydonia-24b-v4.1", "x-ai/grok-4.3"]
allow_traits = ["allow_politics"]          # new field
temperature  = 0.8                          # task-level, shared across tiers
max_tokens   = 1200

[tasks.chat_companion.tiers.free]
model        = "qwen/qwen3.6-flash"
fallback     = ["deepseek/deepseek-v4-flash"]
allow_traits = ["allow_politics"]

[tasks.chat_companion.tiers.gold]
model        = "x-ai/grok-4.20"
fallback     = ["thedrummer/cydonia-24b-v4.1", "x-ai/grok-4.3"]
allow_traits = ["allow_nsfw", "allow_politics"]
```

Why nested tables (not `[[tasks.chat_companion.tiers]]` array-of-tables): tier
names are naturally unique keys → the request `tier` string maps directly to a
table key, no duplicate-name ambiguity, no array scan.

### 2. Type changes (`eros-engine-llm/src/model_config.rs`)

```rust
/// One tier's overrides. Every field is optional; absent fields inherit from
/// the enclosing task block (the "default block").
#[derive(Debug, Clone, Deserialize)]
pub struct TierConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub fallback: Option<FallbackSpec>,
    /// Allow-listed prompt-trait tags for this tier. Three-state, mirroring
    /// `fallback`'s absent≠empty convention:
    ///   - field absent  → None       → no gating (all traits pass)
    ///   - `[]`          → Some(vec![])→ empty whitelist (all traits dropped)
    ///   - `["a","b"]`   → Some([a,b]) → keep only tags in the set
    #[serde(default)]
    pub allow_traits: Option<Vec<String>>,
}

pub struct TaskConfig {
    pub model: String,
    // ...existing fields (temperature, max_tokens, description, fallback, dimensions)...
    /// Task-level trait allow-list (the "default block" value). Same
    /// three-state semantics as TierConfig::allow_traits.
    #[serde(default)]
    pub allow_traits: Option<Vec<String>>,
    /// Per-tier overrides. Empty for tasks that don't tier (the default).
    #[serde(default)]
    pub tiers: HashMap<String, TierConfig>,
}

pub struct ResolvedModel {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub temperature: f64,
    pub max_tokens: u32,
    /// None → no gating; Some(set) → keep only prompt_traits whose tag ∈ set.
    pub allow_traits: Option<Vec<String>>,
}
```

`FallbackSpec`, `DefaultConfig`, and `ModelConfig` are unchanged in shape.

### 3. Resolution precedence (`resolve` signature change)

```rust
// Before: resolve(&self, task: &str, persona_override: Option<&str>)
pub fn resolve(&self, task: &str, tier: Option<&str>) -> ResolvedModel
```

Per-field inheritance — a matched tier overrides only what it specifies; the
rest falls back to the task's default block:

| field          | precedence                                                                      |
|----------------|---------------------------------------------------------------------------------|
| `model`        | `tier.model` → `task.model` → `defaults.fallback_model` → compiled-in           |
| `fallback`     | `tier.fallback` → `task.fallback` → `defaults.fallback_model` (existing rules)  |
| `allow_traits` | `tier.allow_traits` → `task.allow_traits` → `None`                              |
| `temperature`  | `task.temperature` → `defaults.fallback_temperature` → compiled-in (no tier)    |
| `max_tokens`   | `task.max_tokens` → `defaults.fallback_max_tokens` → compiled-in (no tier)      |

Tier lookup:

- `tier = Some(name)` and `task.tiers` contains `name` → use that `TierConfig`
  for the per-field overrides above.
- `tier = Some(name)` but no such key → log `warn!(task, tier, "unknown tier,
  using default block")` and resolve from the task block only.
- `tier = None` → resolve from the task block only (the default block).

`allow_traits` three-state is preserved through resolution: a matched
`tier.allow_traits` of `Some([])` wins over the task's `allow_traits` (explicit
empty whitelist), exactly as `fallback = []` suppresses defaults today.

### 4. Request → Event plumbing

- **`StreamSendRequest`** (`routes/companion_stream.rs`) gains
  `#[serde(default)] tier: Option<String>`.
- **Validation** (in `validate_payload` or a sibling helper): when present,
  `tier` must match `^[a-z0-9_]{1,32}$` (same charset as a trait tag); violation
  → `400` pre-stream error (`code: "invalid_payload"`). A well-formed tier that
  is not configured is **not** an error — it falls through to the default block
  (per §3).
- **`Event::UserMessage`** (`eros-engine-core/src/types.rs`) gains
  `#[serde(default)] tier: Option<String>`. Defaulting keeps existing
  deserialization tests valid (legacy bodies → `None`).
- **`PersistedUserMessage`** (`pipeline/stream.rs`) gains `tier: Option<String>`,
  set from the request and copied into both `Event::UserMessage` constructions
  (foreground + background) in `run_stream`.

### 5. Trait gating — where it happens

Today `build_reply_request` calls `build_prompt(...prompt_traits)` *before*
`assemble_chat_request` (which is where `resolve` runs). To gate, we must resolve
first. Restructure `build_reply_request`:

1. Read `tier` off the event (`Event::UserMessage { tier, .. }`).
2. `let resolved = state.model_config.resolve(CHAT_TASK, tier.as_deref());`
3. Filter the event's `prompt_traits` against `resolved.allow_traits`:
   - `None` → keep all (current behavior).
   - `Some(set)` → `retain(|t| set.contains(&t.tag))`; collect dropped tags.
   - Log at `info`/`debug`: `tier`, `kept_count`, `dropped_tags` (tags only,
     never `text`).
4. `build_prompt(..., &kept_traits)` → system prompt.
5. `assemble_chat_request` is changed to accept the already-`resolved`
   `ResolvedModel` instead of resolving internally (avoids a second resolve and
   keeps a single source of truth for the turn).

`assemble_chat_request(state, input, resolved, system_prompt, history, audit)` —
drops its internal `resolve` call and the `persona_model_override` lookup.

Gift path (`build_gift_request`): resolves with `tier = None` (Gift event has no
tier) and passes `&[]` traits as today, so no gating change.

### 6. Remove `persona_override`

- Delete `persona_model_override()` (`pipeline/handlers.rs`).
- Drop the `persona_override` parameter from `resolve` (replaced by `tier`).
- Engine no longer reads `art_metadata.model`. The field may remain in persona
  JSONB; it is simply ignored.
- Remove the now-dead `model = "x-ai/grok-4-fast"` from the example personas
  (`examples/personas/{aria,miel,kenji}.toml`) to avoid implying it still works.

## Tests

`eros-engine-llm` (`model_config.rs`):

- Parse a config with `[tasks.chat_companion.tiers.free]` /
  `.gold` → both tiers present with expected fields.
- `resolve("chat_companion", Some("gold"))` → gold's model/fallback/allow_traits.
- `resolve("chat_companion", Some("free"))` → free's values.
- `resolve("chat_companion", Some("platinum"))` (unmatched) → default block
  values (and the warn path, asserted by behavior not log capture).
- `resolve("chat_companion", None)` → default block values.
- Per-field inheritance: a tier that sets `model` only → `fallback` /
  `allow_traits` inherited from the task block.
- `allow_traits` three-state: absent → `None`; `[]` → `Some(vec![])`;
  `["a","b"]` → `Some(["a","b"])`. Including a tier `allow_traits = []`
  overriding a non-empty task-level list.
- Update **`COMPAT_FIXTURE`** + `compat_fixture_locks_full_schema` to add the new
  optional fields (`tiers`, `allow_traits`) and assert they round-trip; confirm
  the existing locked fields are untouched.
- Update every existing `resolve(..., Some("..."))` / `resolve(..., None)` call
  site for the new `tier` parameter meaning, and delete the
  `test_resolve_persona_override_wins` /
  `test_resolve_override_with_unknown_task_uses_defaults_for_params` tests (or
  rewrite them as tier tests).

`eros-engine-server`:

- **Trait gating (unit, `handlers.rs`):** a small pure helper
  `filter_traits(traits, allow_traits) -> (kept, dropped)` so it's testable
  without a live DB:
  - `allow_traits = None` → all kept, none dropped.
  - `allow_traits = Some(["allow_politics"])` with traits
    `["allow_politics","allow_nsfw"]` → keeps `allow_politics`, drops
    `allow_nsfw`.
  - `allow_traits = Some([])` → all dropped.
- **Request plumbing (route test, `companion_stream.rs`):**
  - Body with a malformed `tier` (e.g. `"Gold!"`) → `400` `invalid_payload`.
  - Body with a well-formed unknown `tier` → stream proceeds (default block).
- **Event default (unit, `types.rs`):** legacy body without `tier` →
  `Event::UserMessage { tier: None, .. }`.

## Risks / Open Questions

1. **`tier` is opaque + downstream-trusted.** The engine never authenticates the
   tier; it trusts whatever the request carries. A compromised client could send
   `tier: "gold"`. This matches the existing trust model (the downstream is the
   guard), but it means tier-based monetization is enforced *only* as far as the
   downstream's auth is trusted. Documented, not mitigated, in v1.
2. **Unknown tier = silent default, not error.** Chosen so a typo or a
   not-yet-configured tier degrades to the baseline rather than failing the
   chat. The `warn` log is the signal. If we'd rather hard-fail, it's a one-line
   change.
3. **Per-field inheritance vs whole-block override.** We inherit per field
   (tier overrides only what it lists). Alternative is "tier must be complete."
   Per-field is less repetitive and matches the existing task→defaults
   inheritance style.

## Acceptance Criteria

- [ ] `cargo test -p eros-engine-llm -p eros-engine-server` green
- [ ] `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check` clean
- [ ] `resolve("chat_companion", Some("gold"|"free"))` returns that tier's
      model/fallback/allow_traits; unmatched/`None` returns the default block
- [ ] Per-request `tier` flows request → `Event::UserMessage` → resolve
- [ ] A trait tag outside the resolved `allow_traits` is dropped (not injected),
      the reply is still produced, and the dropped tag is logged
- [ ] `allow_traits` three-state (absent / `[]` / list) behaves as specified
- [ ] Engine reads no `art_metadata.model`; `persona_override` parameter gone
- [ ] `COMPAT_FIXTURE` updated and passing; docs updated
      (`docs/model-config.md{,.zh}`, `docs/prompt-traits.md{,.zh}`,
      `docs/api-reference.md{,.zh}`) including the precedence/stability-commitment
      rewrite and a changelog note for the `persona_override` removal

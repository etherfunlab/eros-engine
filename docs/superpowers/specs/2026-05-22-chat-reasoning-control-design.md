# eros-engine — Per-task reasoning control (Spec)

**Status**: design, pending implementation plan
**Target release**: 0.3.x patch (additive, non-breaking)
**Audience**: anyone implementing the engine-side reasoning toggle for `tasks.chat_companion`

---

## 0. Background

`tasks.chat_companion` can resolve to reasoning-capable models (e.g.
`qwen/qwen3.6-flash`, the `z-ai/glm-*` family). On those models OpenRouter
emits a `reasoning` token stream that the engine's chat path does **not**
consume — it only accumulates `delta.content`. The reasoning tokens add
latency and cost to every companion turn while contributing nothing to the
visible reply.

We want a config knob to turn reasoning **off** for the chat task, without
touching the structured-extraction tasks (insight/affinity/memory) that may
legitimately benefit from it later.

### OpenRouter API (empirically confirmed 2026-05-22)

The request accepts a `reasoning` object. Probed against the engine's own
key at `max_tokens=400`:

| request | `reasoning` chars | outcome |
|---|---|---|
| qwen, no `reasoning` param | 3110 | model default — heavy reasoning |
| qwen, `reasoning:{enabled:false}` | 0 | content present, `finish=stop` — **truly disabled** |
| qwen, `reasoning:{exclude:true}` | 0 in stream | still generated server-side (hidden, still billed) — *not* what we want |
| cydonia (non-reasoning), `reasoning:{enabled:false}` | 0 | HTTP 200, no error — harmless no-op |
| minimax-m2-her, `reasoning:{enabled:false}` | 0 | HTTP 200 — harmless |

**Conclusion:** `reasoning: { "enabled": false }` is the correct primitive.
It suppresses reasoning on reasoning-capable models and is a harmless no-op
on models that don't support it (no 4xx).

---

## 1. Goal / Non-goals

**Goal:** a TOML-driven, task-level reasoning config that makes
`tasks.chat_companion` send `reasoning:{enabled:false}` on every turn (all
tiers). The config mirrors OpenRouter's `reasoning` object so operators can
also express e.g. `{ exclude = true }`.

**Non-goals:**
- Per-tier reasoning override (task-level only; tiers inherit).
- `effort`/`max_tokens` reasoning fields (the struct carries `enabled` +
  `exclude` only; extend later if needed).
- Changing reasoning for insight/affinity/memory tasks (left at model default).
- Consuming/persisting reasoning tokens (the engine still ignores the
  `reasoning` channel).

---

## 2. Design

### 2.1 Config object (`ReasoningConfig`)

A single struct mirrors OpenRouter's `reasoning` object — parsed from TOML and
serialized to the wire unchanged, so config and request shape stay aligned:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReasoningConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<bool>,
}
```

`TaskConfig.reasoning: Option<ReasoningConfig>` (not on `TierConfig`).
Three-state, like `temperature`/`max_tokens` being task-level only:

- absent → `None` → omit the wire param (model default; current behavior)
- present → forwarded verbatim, inner absent fields omitted

```toml
[tasks.chat_companion]
model = "x-ai/grok-4.20"
reasoning = { enabled = false }   # disables reasoning for ALL tiers
# or: reasoning = { exclude = true }   # generate but hide it
```

### 2.2 Resolver (`ResolvedModel` + `resolve()`)

Add `reasoning: Option<ReasoningConfig>` to `ResolvedModel`. In `resolve()` it
is read (cloned) straight from the task block:
`task_cfg.and_then(|t| t.reasoning.clone())`. **Task-level only** — tiers
inherit it unchanged (no `TierConfig` field), exactly like
`temperature`/`max_tokens`.

### 2.3 Request plumbing (`ChatRequest` + build sites)

Add `reasoning: Option<ReasoningConfig>` to `ChatRequest` (derives `Default` →
`None`). Thread `resolved.reasoning` into every `ChatRequest` build site
(`assemble_chat_request` for chat, plus the affinity / insight / memory
builders). Tasks with no `reasoning` in config resolve to `None` → field
omitted → behavior unchanged.

### 2.4 Wire serialization (`openrouter.rs`)

`ChatRequest.reasoning: Option<ReasoningConfig>` is forwarded directly — no
bespoke wire struct. `WireRequest` borrows it:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
reasoning: Option<&'a ReasoningConfig>,
```

set from `req.reasoning.as_ref()` (streaming) and the `req_reasoning` param
(sync `call_once`, passed `req.reasoning.as_ref()` by `execute`). Because the
streaming and sync paths were unified onto `WireRequest` (the 2026-05-22
`user: null` bugfix), this single field covers **both** paths.

### 2.5 Streaming retry depth + fallback guidance

Raise `MAX_STREAM_FALLBACK_DEPTH` 2 → 3 (1 primary + up to 2 fallbacks). The
frontend masks attempts beyond the first behind a "thinking" affordance, so the
extra attempt adds resilience without looking buggy. Correspondingly, the
example config recommends `fallback` ≤ 2 entries (primary + 2 = the cap);
entries past the third are never tried on the chat path.

---

## 3. Testing

- **model_config resolve:** task `reasoning = { enabled = false }` →
  `Some(ReasoningConfig{enabled:Some(false),..})` (and a no-override tier
  inherits it); absent → `None`; `{ exclude = true }` parses into the struct.
- **openrouter wire:** `WireRequest` with a `Some(&ReasoningConfig{enabled:
  Some(false),..})` serializes `"reasoning":{"enabled":false}`; `None` omits
  the key entirely.
- **committed example config:** `resolve("chat_companion", …)` (and the free
  tier) → `reasoning == Some({enabled:false})`; `insight_extraction` → `None`.

---

## 4. Rollout

- `examples/model_config.toml`: add `reasoning = { enabled = false }` to
  `[tasks.chat_companion]`; trim default `fallback` to ≤ 2 entries and document
  the guidance.
- `MAX_STREAM_FALLBACK_DEPTH` 2 → 3.
- Regenerate the committed OpenAPI snapshot only if any `ToSchema` type
  changed (it does not — `ChatRequest`/`WireRequest`/`ReasoningConfig` are not
  exposed schemas).
- Additive and non-breaking: existing configs without `reasoning` keep
  model-default behavior.

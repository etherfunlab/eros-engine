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

**Goal:** a TOML-driven, task-level toggle that makes `tasks.chat_companion`
send `reasoning:{enabled:false}` on every turn (all tiers).

**Non-goals:**
- Per-tier reasoning override (task-level only; tiers inherit).
- Reasoning *effort*/*max_tokens* tuning (only the on/off `enabled` flag).
- Changing reasoning for insight/affinity/memory tasks (left at model default).
- Consuming/persisting reasoning tokens (the engine still ignores the
  `reasoning` channel).

---

## 2. Design

### 2.1 Config (`TaskConfig`)

Add `reasoning: Option<bool>` to `TaskConfig` (not `TierConfig`). Three-state,
mirroring how `temperature`/`max_tokens` are task-level only:

- absent → `None` → omit the wire param (model default; current behavior)
- `false` → `Some(false)` → send `reasoning:{enabled:false}`
- `true` → `Some(true)` → send `reasoning:{enabled:true}`

```toml
[tasks.chat_companion]
model = "x-ai/grok-4.20"
reasoning = false   # disables reasoning for ALL chat_companion turns (every tier)
```

### 2.2 Resolver (`ResolvedModel` + `resolve()`)

Add `reasoning: Option<bool>` to `ResolvedModel`. In `resolve()` it is read
straight from the task block: `task_cfg.and_then(|t| t.reasoning)`. It is
**task-level only** — tiers inherit it unchanged (no `TierConfig` field, no
per-tier override), exactly like `temperature`/`max_tokens`.

### 2.3 Request plumbing (`ChatRequest` + build sites)

Add `reasoning: Option<bool>` to `ChatRequest` (derives `Default` → `None`).
Thread `resolved.reasoning` into every `ChatRequest` build site
(`assemble_chat_request` for chat, plus the affinity / insight / memory
builders). Tasks with no `reasoning` in config resolve to `None` → field
omitted → behavior unchanged.

### 2.4 Wire serialization (`openrouter.rs`)

Add to `WireRequest`:

```rust
#[derive(Debug, Serialize)]
struct WireReasoning { enabled: bool }

// in WireRequest:
#[serde(skip_serializing_if = "Option::is_none")]
reasoning: Option<WireReasoning>,
```

Map `ChatRequest.reasoning: Option<bool>` → `Option<WireReasoning>`
(`Some(b) => Some(WireReasoning{enabled:b})`, `None => None`). Because the
streaming and sync paths were unified onto `WireRequest` (the 2026-05-22
`user: null` bugfix), this single field covers **both** paths — no separate
`json!` edit.

---

## 3. Testing

- **model_config resolve:** task `reasoning=false` → `Some(false)`; absent →
  `None`; a tier with no override inherits the task's `reasoning`.
- **openrouter wire:** `WireRequest` with `reasoning=Some(false)` serializes
  `"reasoning":{"enabled":false}`; `None` omits the key entirely.
- **committed example config:** `resolve("chat_companion", …)` →
  `reasoning == Some(false)`.

---

## 4. Rollout

- `examples/model_config.toml`: add `reasoning = false` to
  `[tasks.chat_companion]`.
- Regenerate the committed OpenAPI snapshot only if any `ToSchema` type
  changed (it does not — `ChatRequest`/`WireRequest` are not exposed schemas).
- Additive and non-breaking: existing configs without `reasoning` keep
  model-default behavior.

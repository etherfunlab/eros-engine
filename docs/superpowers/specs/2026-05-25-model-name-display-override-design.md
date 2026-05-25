# eros-engine — Client-facing model name display override (Spec)

**Status**: design, pending implementation plan
**Target release**: 0.x patch (additive schema; **changes the default SSE wire behavior** — see §1)
**Audience**: anyone implementing the engine-side `model_name_display_override` knob for `tasks.*`

---

## 0. Background

The streaming chat path emits a `meta` frame per assistant bubble whose
`model` field tells the client which model produced the reply. Today that
field always carries the **real** OpenRouter model id — for every live
attempt in the fallback chain, and again on history replay (read back from
the DB).

Operators want to control what the client *sees* here, independently of what
the engine actually calls and stores: hide it entirely, pin it to a brand
name, randomize across a pool, or remap specific ids to display names. This
is purely cosmetic/branding — it must not touch the OpenRouter request, the
persisted DB row, or any usage logging.

### Where a model name reaches a client (confirmed)

Only the **streaming SSE path** (`crates/eros-engine-server/src/pipeline/stream.rs`):

| Site | Code | Key (real model id) used for lookup |
|---|---|---|
| Live burst Meta | `drive_chat_burst`, ~L142 | the attempted chain entry `model_id` |
| Replay Meta | `replay_stream`, ~L537 | DB `row.model` |
| Ghost Meta (live + replay) | ~L345 / ~L517 | none — no model involved |

Not client-facing (left untouched):
- `pipeline/mod.rs` sync `run` builds `ChatResponse.model` (~L177) but is
  **unrouted dead code**; the gift route returns `reply: None`. We apply the
  override there too (trivial, keeps the contract honest if re-wired) but it
  has no live effect today.
- Background tasks (affinity / memory / dreaming) build requests with the
  resolved model and never expose it to a client.

---

## 1. Goal / Non-goals

**Goal:** a TOML-driven, **task-level** `model_name_display_override` that
controls the `model` value sent to clients in chat `meta` frames. Four forms:
`boolean | string | array | dict`.

**Default behavior change (approved):** the compiled-in / field-absent default
is **`false` → omit the `model` field**. This flips today's "always show the
real id" behavior. To preserve the current experience for OSS users, the
shipped `examples/model_config.toml.example` sets
`model_name_display_override = true` on `chat_companion`.

**Non-goals:**
- Per-tier override (task-level only; tiers inherit, exactly like
  `reasoning`/`temperature`/`max_tokens`).
- Changing the model sent to OpenRouter (`per_model_req.model`), the persisted
  `AssistantInsert.model`, or any usage logging — all keep the **real** id.
- Persisting the display name (so the DB stays the source of truth for the
  real model; see the replay note in §2.5).

---

## 2. Design

### 2.1 Config object (`DisplayOverride`)

New untagged enum in `model_config.rs`, parsed the same way as `ModelSpec`
(TOML bool vs string vs array vs inline-table are unambiguous to serde):

```rust
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum DisplayOverride {
    Bool(bool),                    // false → omit; true → real id
    Fixed(String),                 // always this string
    Random(Vec<String>),           // random pick per emit
    Map(HashMap<String, String>),  // key = real id; reserved key "default"
}
```

TOML forms (model ids contain `/` and `.`, so map keys must be quoted; the
bareword `default` key is reserved):

```toml
[tasks.chat_companion]
model = "deepseek/deepseek-v4-flash"

# pick ONE of:
model_name_display_override = false                       # omit model field
model_name_display_override = true                        # real id (today's behavior)
model_name_display_override = "Aria"                      # always "Aria"
model_name_display_override = ["Aria", "Nova"]            # random per bubble
model_name_display_override = { "deepseek/deepseek-v4-flash" = "Aria", "thedrummer/cydonia-24b-v4.1" = "Nova", default = "Companion" }
```

### 2.2 Resolver (`TaskConfig` + `ResolvedModel` + `resolve()`)

- `TaskConfig.model_name_display_override: Option<DisplayOverride>`
  (`#[serde(default)]`). **Not** on `TierConfig`.
- Add `model_name_display_override: Option<DisplayOverride>` to
  `ResolvedModel`. In `resolve()` read it straight from the task block:
  `task_cfg.and_then(|t| t.model_name_display_override.clone())`.
- **Task-level only — tiers inherit unchanged.** A consequence we rely on:
  the resolved override value is **tier-independent**, so any code path that
  lacks a tier (replay) can resolve with `(CHAT_TASK, None)` and still get the
  correct override.

### 2.3 Apply helper

```rust
impl ResolvedModel {
    /// Map the real model id to the value sent to the client.
    /// `None` => omit the `model` field from the frame.
    pub fn display_model(&self, actual_model: &str) -> Option<String> { ... }
}
```

| Config | Result |
|---|---|
| absent (`None`) / `false` | `None` → **omit field** |
| `true` | `Some(actual_model.to_string())` |
| `"Aria"` | `Some("Aria")` (empty string → `None`) |
| `["a","b"]` | random `Some(..)`; empty list → `None` |
| `{ "m1"="n1", default="nd" }` | `get(actual)` else `get("default")` else `None` |

`default` is looked up by the literal key `"default"`. (Edge: a real model id
literally named `default` is not a thing in OpenRouter's `vendor/model`
namespace, so the collision is ignored.)

### 2.4 Wire change (`ProtocolFrame::Meta.model`)

`model` changes `String` → `Option<String>`:

```rust
Meta {
    message_id: String,
    action_type: FrameActionType,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    continues_from: Option<String>,
}
```

- When hidden, the `model` **key is absent** from the JSON (not `null`/`""`).
- Ghost frames, previously `model: ""`, now pass `None` → omitted.
- Frontend must tolerate a missing `model` field. (Sync `ChatResponse.model`
  is already `Option<String>`.)

This is a change to the SSE contract documented in
`docs/superpowers/specs/2026-05-19-sse-streaming-chat-0.2-design.md` §1.5
(`model` was always present). The TOML schema change is purely additive and
within the stated 0.x stability commitments.

### 2.5 Apply sites

In `stream.rs`:

- **Live burst** (`drive_chat_burst`): the resolved override is on the
  `ResolvedModel` used to build `req`; thread it (or the `ResolvedModel`) into
  the burst driver and emit
  `model: resolved.display_model(model_id)` for each attempt's Meta. The
  request still uses the real `model_id`; the persisted row still stores the
  real `model_id`.
- **Replay** (`replay_stream`): resolve `(CHAT_TASK, None)` once, then emit
  `model: resolved.display_model(row.model.as_deref().unwrap_or_default())`
  per row. Both `reply` and `gift_reaction` rows are `chat_companion` (gift
  builds via `resolve(CHAT_TASK, None)` too), so one resolve covers all rows.
- **Ghost** (both live and replay): `model: None`.

In `pipeline/mod.rs` (currently dead sync path): apply
`resolved.display_model(llm_resp.model.as_deref().unwrap_or(&resolved.model))`
when building `ChatResponse.model`, for contract consistency.

### 2.6 Replay consistency (explicit consequence)

The override is **re-applied on replay** — otherwise a client reconnecting
would read the real model id straight from the DB, defeating `false`/dict-hide.
Because the display name is intentionally **not** persisted, the `array` form
**re-randomizes** on replay: a bubble shown as `Aria` live may show `Nova`
after a reload. `bool` / `string` / `dict` are deterministic across live and
replay. This is acceptable for a cosmetic field.

### 2.7 "All task types"

The field is valid on every task block (it lives on `TaskConfig`) and is
carried on every `ResolvedModel`, but it only has an **observable effect** for
`chat_companion`, the only task that streams a model to a client. On
background tasks it is inert.

---

## 3. Testing

- **Parsing:** all four forms deserialize into the right `DisplayOverride`
  variant; quoted map keys + bareword `default` parse.
- **Resolve + inheritance:** task `model_name_display_override` resolves onto
  `ResolvedModel`; a no-override tier inherits the task value; absent → `None`.
- **`display_model`:** one assertion per row of the §2.3 table, including
  dict-hit, dict-miss→default, dict-miss→no-default→`None`, `true`/`false`,
  empty array → `None`, empty string → `None`.
- **Live burst:** with `false`, the Meta frame omits `model`; with a dict, the
  attempted fallback id maps to its display name (or `default`).
- **Replay:** override is applied to DB rows (real id never leaks when hidden);
  ghost replay omits `model`.
- **Meta serialization:** `Some(..)` serializes `"model":"…"`; `None` omits the
  key entirely. Update existing `meta_frame_*` constructor tests for the
  `Option` type change.
- **Committed example config:** `resolve("chat_companion", …).display_model(id)`
  returns the real id (because the example sets `= true`); an untouched task
  with no override → `None`.

---

## 4. Rollout

- `examples/model_config.toml.example`: add
  `model_name_display_override = true` to `[tasks.chat_companion]`, with a
  doc-comment block covering all four forms (quote map keys; `default`
  reserved).
- `docs/model-config.md`: document the field and the SSE implication ("the
  `meta.model` field may be omitted depending on this setting"); note it under
  the stability commitments as an added optional field.
- Update the SSE streaming spec note that `meta.model` is now optional.
- Additive TOML schema; **default wire behavior changes** (model omitted when
  unset) — the shipped example opts back into showing the real id so OSS users
  see no behavior change out of the box.

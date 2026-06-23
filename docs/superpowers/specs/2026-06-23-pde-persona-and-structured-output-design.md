# eros-engine — PDE persona injection + structured output (adopt audit 22 §6)

**Status**: design, pending implementation plan
**Target release**: `0.6.x` dev track. **No schema migration.**
**Scope**: adopt the two code recommendations from `eros-audit` report 22 (PDE
conservatism). The PDE judge currently makes character-blind, JSON-fragile
decisions; this spec gives it (1) a **compact persona brief** so judgments
differentiate by character, and (2) **`response_format: json_schema`** on the
PDE request plus a one-line **parse-error chain-walk** so a malformed verdict
tries the rest of the fallback chain before dropping to the rule engine.

Out of scope (deployment-owned config, already applied live per audit 22 §4/§5):
the「互动导演」`filter_prompt` rewrite and the Hermes/Mistral/Llama model chain.
Those live in a deployment's `MODEL_CONFIG_PATH`; the OSS `examples/model_config.toml`
still ships `pde_decision` **off** (no `filter_prompt`).

---

## 0. Background

### 0.1 What audit 22 found

Across 11 live `companion_decision_events` (early PDE rollout), the judge
de-escalated **7/7** charged turns. Three root causes; this spec addresses the
two that are code (not model/prompt config):

- **Root cause C — no persona in the PDE input.** `build_pde_ctx`
  (`stream.rs:1126`) feeds only transcript + 6 affinity axes + signals + latest
  message. With no character to reason from, every persona gets the same generic
  "理性、温和、尊重边界" disposition. The audit's fix: inject a compact persona.
- **JSON fragility.** The PDE request is free-text parsed
  (`parse_pde_verdict`, `stream.rs:925`: `from_str` → `find_json_block`). The
  audit's fix: add `response_format: json_schema` to raise adherence; optionally
  also let a parse-error try the next model.

### 0.2 What is already in place (shrinks the work)

- **`DecisionInput` already carries the full persona.** `DecisionInput.persona:
  CompanionPersona` (`types.rs:146`) is already threaded into `build_pde_ctx`'s
  caller — `build_pde_ctx` simply ignores it. So **no DecisionInput threading is
  needed** (contra the audit's wording); only rendering.
- **`parse_pde_verdict` is already belt-and-suspenders.** It tries
  `serde_json::from_str` then `find_json_block`. So `response_format` is **purely
  additive**: a provider that honors it → cleaner JSON; a provider that ignores
  it → free text still parses → no regression.
- **The `art_metadata` pluck helpers exist** in the same crate
  (`prompt.rs:273/282/292`: `meta_str` / `meta_i32` / `meta_string_array_joined`),
  currently private. `build_pde_ctx` is in the same server crate → reuse via
  `pub(crate)`.

### 0.3 The genome fields that actually exist

`PersonaGenome` (`persona.rs`): `name`, `system_prompt`, `tip_personality:
Option<String>`, `art_metadata: Value` (JSONB carrying `gender/age/mbti/
backstory/speech_style/quirks/topics/model`). The audit's illustrative columns
`personality / initiative_style / conflict_style / image_habit` **do not exist**
and are **not** added — the brief is built from existing fields only.

---

## 1. Change A — compact persona brief in the PDE ctx

### 1.1 New pure helper

Add a pure, unit-testable function (in `stream.rs`, beside `build_pde_ctx`):

```rust
/// Build a compact persona disposition block for the PDE judge from EXISTING
/// genome fields. Empty fields are omitted; an all-empty persona yields "".
fn build_persona_brief(persona: &CompanionPersona) -> String;
```

Rendered shape (fields omitted when blank):

```
[角色人格] {name}，{gender} {age}岁，{mbti}
说话风格：{speech_style}
口癖：{quirks}
打赏人格：{tip_personality}
```

- **Sources**: `genome.name`; `art_metadata` → `gender` / `age` / `mbti` /
  `speech_style` / `quirks`; `genome.tip_personality` (only when `Some` and
  non-blank). Reuse `crate::prompt::{meta_str, meta_i32, meta_string_array_joined}`
  (changed to `pub(crate)`).
- **Deliberately NOT injected**: `system_prompt` (long; may re-import the chat
  prompt's customer-service / boundary framing into the judge — the exact
  conservatism this spec fights) and `topics` (irrelevant to disposition).
- The audit's `initiative_style / conflict_style / image_habit` have no backing
  data → **not invented**. `name + gender/age + mbti + speech_style + quirks +
  tip_personality` is enough disposition for the judge to differentiate a bold
  vs. a shy character. The「互动导演」prompt (deployment config) already asks for
  character-driven judgment; this brief is the character it drives from.

### 1.2 Wiring into `build_pde_ctx`

`build_pde_ctx` renders the brief at the **top** of the ctx (before
`[最近对话]` / `[关系状态] / [信号] / [用户最新消息]`), so the judge reads
"who am I" before "what's happening". When the brief is empty, the block is
omitted and the ctx is byte-identical to today (a deployment with bare-bones
personas is unaffected). No new function parameters — `build_pde_ctx` already
receives `&DecisionInput`.

The ctx is the per-turn **user** message (rebuilt every turn), so there is no
prompt-cache interaction. The brief is not persisted (the audit table stores the
verdict, not the ctx).

---

## 2. Change B — `response_format: json_schema` + parse-error chain-walk

### 2.1 Wire layer (`openrouter.rs`) — sync path only

- `ChatRequest` gains `pub response_format: Option<serde_json::Value>` — an
  opaque passthrough, mirroring the existing `metadata` field. `None` ⇒ omitted.
- `WireRequest<'a>` gains `#[serde(skip_serializing_if = "Option::is_none")]
  response_format: Option<&'a serde_json::Value>`, threaded through the
  `execute` / `call_once` WireRequest construction.
- **Sync path only.** The PDE uses `state.openrouter.execute(req)` (non-stream);
  `execute_stream` is **not** touched. A deployment that sets nothing produces a
  byte-identical body to today.

### 2.2 The schema (`run_pde_decision`)

Pure helper `fn pde_response_format() -> serde_json::Value` builds the verdict
schema (OpenAI/OpenRouter `json_schema` form):

```json
{
  "type": "json_schema",
  "json_schema": {
    "name": "pde_verdict",
    "strict": true,
    "schema": {
      "type": "object",
      "additionalProperties": false,
      "required": ["action", "inner_state", "image_prompt", "reason"],
      "properties": {
        "action": { "type": "string",
          "enum": ["reply_text", "ghost", "reply_image", "reply_text_image"] },
        "inner_state": { "type": "string" },
        "image_prompt": { "type": ["string", "null"] },
        "reason": { "type": ["string", "null"] }
      }
    }
  }
}
```

- `strict: true` requires every property in `required`; the optional
  `image_prompt` / `reason` are made **nullable** so the model returns `null`,
  which deserializes to `PdeVerdict`'s `Option` fields as `None`. `action` /
  `inner_state` stay effectively required.
- A provider that doesn't support `strict` falls back to free text →
  `parse_pde_verdict` still parses (§0.2). So the schema is a best-effort
  adherence lever, never a hard dependency.

`run_pde_decision` sets `response_format: Some(pde_response_format())` on the
`ChatRequest` **only when `structured_output` is enabled** (§3).

### 2.3 Parse-error chain-walk (the second change)

Today (`stream.rs:1058`) a `ParseError` **returns immediately** — it skips the
rest of the model chain and the caller drops to the rule engine. Change: a
parse-error **records and continues** to the next model, exactly like
`Empty` / `Error` / `Timeout`:

- Replace the immediate `return PdeDecisionRun{ status: ParseError, .. }` with
  setting `last = PdeStatus::ParseError` and **retaining the attempt's `raw` /
  `model` / `usage` / `generation_id`** in locals, then `continue`.
- A later model that parses `Ok` short-circuits and returns `Ok` (unchanged).
- If the **whole chain** fails to parse, the chain-exhausted return at the
  bottom (`stream.rs:1072`) must now carry the **last** parse-error's `raw` +
  audit trio (not all-`None`), so the audit row stays faithful (`status =
  parse_error`, `payload = raw`, `proposed_action = NULL`). This is the one
  non-trivial part of the otherwise one-line change: thread the last
  parse-error's data to the bottom return.
- Net semantic change: the「内容级回复即终结」rule (mirrored from
  `run_input_filter`) is relaxed **for parse-errors only** — a malformed verdict
  now tries the diverse fallback chain (Hermes→Mistral→Llama) before the rule
  engine. `Empty` / `Error` / `Timeout` behavior is unchanged.

Fail-open is preserved throughout: any terminal non-`Ok` status → rule engine.

---

## 3. Config — `structured_output` knob (config-gated, default on)

`[tasks.pde_decision]` gains `structured_output: Option<bool>` on the shared
`TaskConfig` (other tasks ignore it, like `ghosting`). Resolver:

```rust
// ResolvedPde gains:
pub structured_output: bool,   // resolved value
// resolve_pde():
structured_output: self.tasks.get("pde_decision")
    .and_then(|t| t.structured_output).unwrap_or(true),
```

- **Default `true`** (absent ⇒ on) — the audit's recommendation is the default.
- **`false`** — `run_pde_decision` leaves `response_format = None` (byte-identical
  body to pre-this-spec). The escape hatch for a deployment whose provider
  *errors* on an unknown `response_format` param, matching the OSS-deployment-
  safety style of the `ghosting` kill-switch and `ignore_providers`.
- Task-level only (no per-tier override), consistent with `resolve_pde`.
- **Schema lock**: add `structured_output` to `pde_decision` in `COMPAT_FIXTURE`
  (additive — does not break the lock) and assert `resolve_pde` surfaces it.
- **OSS template** (`examples/model_config.toml`): a comment under
  `[tasks.pde_decision]` documenting `structured_output` (default `true`). No
  behavior change to the shipped default (pde stays off without a `filter_prompt`).

---

## 4. Error handling

- Persona brief: pure, infallible; empty string on no data → block omitted.
- `response_format`: additive; ignored-by-provider is harmless (parser fallback);
  errored-by-provider walks the chain (and the `structured_output=false` knob
  disables it entirely). No new error path.
- Parse-error chain-walk: stays within the existing fail-open contract — a fully
  failed chain resolves to the rule engine, never blocks a turn.
- No new logs beyond the existing per-attempt `tracing::warn!`; no migration.

---

## 5. Testing

- **core/server `build_persona_brief`**: all-fields render; missing fields
  omitted (no stray separators); `tip_personality = None` omitted; all-empty
  persona → `""`; does not panic on absent `art_metadata` keys.
- **`build_pde_ctx`**: persona block renders at the top (before `[关系状态]`);
  empty brief ⇒ ctx byte-identical to today.
- **`openrouter`**: `response_format = Some(..)` ⇒ present in serialized
  `WireRequest`; `None` ⇒ key omitted (body byte-identical to today); threaded
  through the sync `execute`/`call_once` path, not `execute_stream`.
- **`pde_response_format`**: schema shape — `action` enum has the four actions,
  `required` lists all four, `image_prompt`/`reason` nullable, `strict = true`.
- **`run_pde_decision`** (the behavior change):
  - parse-error on model #1 → tries model #2; model #2 `Ok` ⇒ overall `Ok`.
  - parse-error on the **whole** chain ⇒ `status = ParseError` **with the last
    attempt's `raw` + model/usage/generation_id** preserved (not all-`None`).
  - model #1 `Ok` still short-circuits (no extra calls).
  - `Empty`/`Error`/`Timeout` chain behavior unchanged.
  - `structured_output = false` ⇒ request carries no `response_format`.
- **`model_config`**: `resolve_pde` surfaces `structured_output` (absent → true;
  `false` → false); `COMPAT_FIXTURE` asserts the new field.
- **Pre-PR gate**: `fmt` / `clippy` / `test` / `openapi` (no API surface change
  expected; run `openapi` to confirm).

---

## 6. File-by-file change list

| File | Change |
| --- | --- |
| `crates/eros-engine-server/src/pipeline/stream.rs` | `build_persona_brief` (new pure fn); `build_pde_ctx` renders it at the top; `pde_response_format` (new pure fn); `run_pde_decision` sets `response_format` when `structured_output`; **parse-error → `continue` + carry last raw/audit-trio to the chain-exhausted return**. |
| `crates/eros-engine-server/src/prompt.rs` | `meta_str` / `meta_i32` / `meta_string_array_joined` → `pub(crate)`. |
| `crates/eros-engine-llm/src/openrouter.rs` | `ChatRequest.response_format: Option<serde_json::Value>`; `WireRequest.response_format` (skip-if-none); threaded through the sync `execute`/`call_once` path only. |
| `crates/eros-engine-llm/src/model_config.rs` | `TaskConfig.structured_output: Option<bool>`; `ResolvedPde.structured_output: bool` + `resolve_pde` (default true); `COMPAT_FIXTURE` + tests. |
| `examples/model_config.toml` | `[tasks.pde_decision]`: comment documenting `structured_output` (default `true`). |

---

## 7. Out of scope / future

- The「互动导演」`filter_prompt` and the Hermes/Mistral/Llama model chain
  (audit 22 §4/§5) — deployment config, already live; not OSS code.
- Real structured persona columns (`initiative_style` etc.) — only worth it if a
  future need shows the existing-field brief is insufficient (YAGNI for now).
- Making `Empty`/`Error`/`Timeout` (already chain-walking) or `ghost`-action
  audit semantics change — untouched.
- The audit's watch-items (re-pull `companion_decision_events`; confirm Hermes
  JSON adherence + gate latency) — operational follow-up, not code.

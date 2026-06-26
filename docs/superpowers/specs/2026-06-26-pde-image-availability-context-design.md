# eros-engine — PDE image-availability context signal (Spec)

**Status**: design, pending implementation plan
**Target release**: `0.6.4` dev track. **No migration**, no new config keys, no
HTTP-contract change.
**Audience**: anyone working on the PDE judge (`run_pde_decision` /
`build_pde_ctx`) on the streaming chat path.
**Builds on**: `2026-06-04-llm-based-pde-design.md` (the judge),
`2026-06-23-pde-persona-and-structured-output-design.md` (judge context +
structured output), and `2026-06-23-reply-image-executor-design.md` (the
`reply_image` / `reply_text_image` executor + the per-turn `image` opt-in).

---

## 0. Background

### What already exists

The per-turn PDE judge picks one action —
`reply_text` | `ghost` | `reply_image` | `reply_text_image` — from a context
string built by `build_pde_ctx()` (`stream.rs:1328`) and the operator's
`[tasks.pde_decision]` prompt.

Image generation is **opt-in per turn**: a caller includes an `image` block on
the user message; the engine resolves an executor chain only then
(`stream.rs:1995-2003`):

```rust
let req_image = user_msg.image.as_ref();
let image_chain = req_image.and_then(|i| effective_image_chain(i.model.as_deref(), …));
let image_executor_available = image_chain.is_some();
```

When no executor is available, `guard_action()` (`stream.rs:1225-1250`) **silently
degrades** `reply_image` / `reply_text_image` → `reply_text`. That degrade is
correct and stays.

### The problem

`build_pde_ctx()` never tells the judge whether an image executor is available
this turn — the context contains persona, transcript, affinity, and signals, but
nothing about image capability. Consequences:

1. **Blind proposals.** When images are unavailable the judge still proposes
   image actions; they are degraded downstream. The judge has no way to know its
   image proposals were dropped, so the operator prompt cannot reliably steer it.
2. **No positive lever.** When images *are* available, the operator prompt has no
   signal to condition on. It cannot say "only choose an image action when image
   output is actually possible this turn," nor lean into image actions on the
   turns where they would land. In practice the judge under-selects image actions
   even when the caller opted in.

This is the symptom downstream operators observe as "the judge almost never
chooses `reply_image`."

### Why this is the right fix here (not operator-side)

`image_executor_available` is engine state derived from the request + resolved
config; only the engine can compute it. The judge can only condition on it if the
engine puts it in the judge context. The operator prompt then references it. So
the engine owns emitting the signal; the operator owns how to react to it.

---

## 1. Goal

Thread the already-computed `image_executor_available` flag into
`build_pde_ctx()` and emit it as a stable, documented line in the judge context,
so the `pde_decision` prompt can condition image-action selection on real
per-turn availability.

Non-goals (unchanged):
- `guard_action()` degrade behaviour — stays as the safety net.
- The HTTP request/response contract and the `image` opt-in semantics.
- The operator's prompt wording (lives in their `model_config.toml`); this spec
  only fixes the example/docs and defines the contract they key off.

---

## 2. The change

### 2.1 `build_pde_ctx` gains an `image_available` parameter

`stream.rs:1328`

```rust
fn build_pde_ctx(
    transcript: &str,
    input: &eros_engine_core::types::DecisionInput,
    image_available: bool,
) -> String {
    …
}
```

Append one line to the emitted context, **always present** (so the judge gets a
clear negative as well as positive signal), placed right after the `[信号]` line
and before `[用户最新消息]`:

```
[图片能力] 本轮可发图=是      // when image_available == true
[图片能力] 本轮可发图=否      // when image_available == false
```

Implementation: add `[图片能力] 本轮可发图={}\n` to the `format!` template with
`if image_available { "是" } else { "否" }`. No other lines change; the order of
existing lines is preserved.

### 2.2 Call site passes the flag (already in scope — no reordering)

`stream.rs:2027`. `image_executor_available` is computed at `2003`, *before* the
judge call at `2027-2028`, so it is already in scope:

```rust
let ctx = build_pde_ctx(&pde_transcript, &input, image_executor_available);
```

### 2.3 Optional extension (call out, default OUT of scope)

`force_image` (`stream.rs:2006`) is a separate explicit-request path that already
forces an image downstream. v1 emits availability only. If a later turn wants the
judge to also author a good `image_prompt` under force, a second token
(`强制发图=是`) can be added the same way — deferred unless a consumer needs it.

---

## 3. Contract for prompt authors (consumers)

The judge context now contains exactly one of:

```
[图片能力] 本轮可发图=是
[图片能力] 本轮可发图=否
```

Prompt authors should:
- Treat `本轮可发图=否` as a hard constraint: never choose `reply_image` /
  `reply_text_image` on that turn (they would be degraded anyway, wasting tokens
  and skewing audits).
- Treat `本轮可发图=是` as the gate that *permits* image actions, then decide by
  persona/context (the engine does not force an image just because it is
  possible).

The token string (`[图片能力] 本轮可发图=是/否`) is the stable interface — keep it
verbatim if a downstream overlay references it.

---

## 4. Files touched

| File | Change |
|------|--------|
| `crates/eros-engine-server/src/pipeline/stream.rs` | `build_pde_ctx` signature + new line (`1328`); call site passes `image_executor_available` (`2027`) |
| `crates/eros-engine-server/src/pipeline/stream.rs` (tests) | Update `build_pde_ctx` unit tests (`6628`, `6660`) for the new arg; add assertions for both `=是` and `=否` |
| `examples/model_config.toml` | Document the new `[图片能力]` line in the `[tasks.pde_decision]` example/comments so the example prompt demonstrates conditioning on it |
| `docs/model-config.md` + `docs/model-config.zh.md` | Note the `本轮可发图` context line in the PDE section |

No changes to `guard_action`, `ActionPlan`, `effective_image_chain`, types, or
the HTTP layer.

## 5. Testing

- **Unit (`build_pde_ctx`)**: assert the rendered context contains
  `本轮可发图=是` when `image_available == true` and `本轮可发图=否` when `false`;
  assert existing lines (persona/transcript/affinity/signals/latest) are
  unchanged and ordered. Extend the two existing tests at `6628` and `6660`.
- **Existing integration tests** that assert the single judge call carries the
  `build_pde_ctx` context (`5626`, `5791`) must still pass — update any string
  match if they pin the full context body.
- `cargo test -p eros-engine-server` green; `cargo clippy` clean.

## 6. Release & rollout

- Lands on the `0.6.4` dev track. No migration, no config-key change — a
  pure-additive context line.
- Backward compatible: an operator prompt that ignores the new line behaves
  exactly as today (the judge still proposes; `guard_action` still degrades).
- On merge → tag `v0.6.4` → `release-docker.yml` publishes
  `ghcr.io/<org>/eros-engine:0.6.4`, which downstream Fly overlays consume by
  bumping their `ENGINE_VERSION`. Operators get the new behaviour only once they
  also update their `pde_decision` prompt to reference `本轮可发图`.

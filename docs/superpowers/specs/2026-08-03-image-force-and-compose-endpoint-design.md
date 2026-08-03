# eros-engine — image.force redefined, standalone compose endpoint

Two changes to how a consumer gets an image prompt out of the engine, plus a
correction to the architecture docs that this work exposed.

`image.force` stops being "override the judge and let me skip giving context"
and becomes an ordinary `reply_image` turn: content required, `mode` gone, and
a refusal when the deployment has no composer. Separately, a consumer that
wants a prompt for arbitrary text — not a chat turn — gets a dedicated
endpoint instead of round-tripping a degenerate turn through the chat stream.

Follows #216 (the judge went seedless, making the composer the only prompt
source) and #218 (which documented the current behaviour honestly enough for
the problem to be visible).

---

## 0. Why now

The engine has no drawing capability — the draw endpoint was removed and the
chat stream only ever emits an `image_request` frame for the consumer to act
on. Given that, a consumer that wants an image already has to draw it itself.
Forcing the turn through the chat pipeline buys nothing and costs something:

- **`force` + `image_only` permits an empty `content`.** The turn exists to
  produce an image, but the composer's strongest input — the user's message —
  is blank by design. The engine is compelled to draw while being denied the
  context that would make the drawing specific.
- **The persisted row is incomplete and unhelpful.** A forced image turn writes
  an assistant row that contributes nothing usable to later chat context.
- **`force` bypasses the image-capability gate.** With no composer configured
  the turn still emits an `image_request`, carrying a prompt assembled from the
  style preset and persona appearance alone — a generic portrait that ignores
  whatever the user said. This is the one path in the engine where the gate is
  bypassed, and `stream.rs` says so in a comment.

The client-supplied-prompt design was already removed in an earlier change, and
the judge stopped writing seeds in #216. `force` is the last piece still shaped
around the older model.

## 1. Decisions (settled during brainstorm)

- **`force = true` resolves to `ActionType::ReplyImage`, always.** Image only,
  no text reply. This costs exactly one LLM call (the composer); `ReplyImage`
  is delegate-only and makes no chat call. If a consumer sets `force`, it wants
  an image — delivering the image matters more than also delivering text.
- **`content` becomes required on forced turns.** The empty-content special
  case is deleted.
- **`mode` is deleted.** Its only real function was gating that empty-content
  exemption; the remaining "pick `ReplyImage` vs `ReplyTextImage`" job belongs
  to the judge, which the consumer has already overridden by setting `force`.
- **A leftover `mode` in a request is silently ignored**, not tombstoned. The
  struct has no `deny_unknown_fields`, so this needs no code. Downstream
  consumers are few enough that a clean break beats accumulating legacy field
  tombstones this early. (This reasoning is specific to the current stage of the
  project, not a general policy.)
- **`force` with no `[tasks.chat_image_prompt_compose]` returns `422`**, a
  pre-stream error, alongside the existing image validations.
- **The standalone endpoint is persona-instance-scoped, not session-scoped.**
  It is explicitly not a chat turn: nothing is persisted, no affinity runs, no
  memory is written. Dragging the recent conversation into it would contradict
  that — and "another image from the current scene" is what `force` is for.
- **The endpoint takes an optional `scene`.** `scene` is a composer *input
  slot*, not the prompt: the composer reads it and writes its own subject, and
  the final `composed_prompt` is still style preset + persona appearance +
  composer-written subject. The consumer has no verbatim injection channel,
  exactly as with `content`. Omitted, it renders as `（无）`.
- **The endpoint doubles as a composer test surface.** This is why the response
  carries `model` and `generation_id`, and why streaming passes the composer's
  raw output through: the most common failure when tuning a `filter_prompt` is
  the model not emitting valid JSON, and the operator needs to see what it
  actually returned rather than the post-fallback result.
- **`stream` defaults to `true`.** Beyond the OpenAI-family convention, a
  streamed call that gets truncated still yields partial output instead of
  losing the whole response and the tokens spent on it.
- **The endpoint shares the existing `StreamSlots` pool** (≤3 in-flight per
  user, `429` over). It is an LLM entry point any authenticated user can
  trigger; chat and voice are already capped by that pool and this bounds the
  same resource.
- **New top-level `/persona/*` namespace.** Future instance-scoped routes hang
  under it rather than being wedged into `/comp/*`.

## 2. `image.force` — new contract

### 2.1 Request shape

`ImageReplyParams` (`routes/companion_stream.rs`) loses `mode`:

```rust
pub struct ImageReplyParams {
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub style: Option<StyleKey>,
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    #[serde(default)]
    pub prompt_variant: Option<String>,
}
```

`ImageMode` is deleted along with it.

### 2.2 Resolution

`force_image` (`stream.rs`) resolves unconditionally to `ActionType::ReplyImage`.
The `mode`-dependent match goes away. Everything else about the forced-image
override is unchanged: it still wins over the PDE/ghost result, and it is still
applied *after* the ghosting kill-switch so a consumer-forced image is never
suppressed to ghost.

### 2.3 Validation

Two changes in `validate_stream_request`:

- The `force && mode == ImageOnly` empty-content exemption is removed. `content`
  follows the ordinary non-empty rule on every forced turn. A tip turn or an
  `image_url` attachment still permits empty content as today.
- New: `image.force == true` while `[tasks.chat_image_prompt_compose]` is
  absent returns `422`, naming the missing task. Pre-stream, so no user row is
  persisted.

The existing `force` + `tips_amount_usd` → `422` rule is unchanged.

**Why `422` here but `501` at the endpoint (§3.6).** Not an inconsistency: the
two describe different things. `501` means *this endpoint does not
functionally exist on this deployment* — the shape the voice endpoint already
established for an absent `[tasks.chat_voice]`. `422` means *this chat request
asked for something this deployment cannot do*; the chat stream itself is very
much alive, and the error joins the existing pre-stream validation family
alongside a bad `aspect_ratio`.

### 2.4 Consequence for the portrait fallback

`build_delegated_image_prompt`'s no-subject branch currently distinguishes two
causes: the composer is unconfigured, or the composer chain failed. §2.3 makes
the first unreachable from a forced turn, and the capability gate already makes
it unreachable from a judged one. The branch stays — it is still the fail-open
for a configured composer whose call fails — but its "no
`[tasks.chat_image_prompt_compose]` configured" arm becomes dead for real
traffic. Keep the arm and its warning as defence in depth; do not restructure
the function.

## 3. `POST /persona/{instance_id}/image/compose`

### 3.1 Placement and auth

A new `routes/persona.rs` module, merged into the same authed sub-router as
`world_town` in `routes/mod.rs`. `require_auth` is attached to that merged
router rather than to a path prefix, so the new namespace is authenticated
without extra wiring. The handler additionally verifies the instance belongs
to the JWT user, returning `403` otherwise and `404` when it does not exist.

Registered in `router_for_openapi` too, so the OpenAPI snapshot covers it.

### 3.2 Request

```json
{
  "content": "在海边，黄昏",
  "scene": "（可选）一段对话片段，喂给 [最近场景]",
  "style": "realistic",
  "aspect_ratio": "3:4",
  "prompt_variant": "0",
  "stream": true
}
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `content` | `String` | yes | Non-empty after trim. Lands in `[对方最新消息]`. Same 4096-char cap as the voice endpoint's `content`. |
| `scene` | `String` | no | Lands in `[最近场景]`. Omitted or blank ⇒ `（无）`. Capped at 8192 chars — roomy enough to paste a real transcript slice (the chat path feeds it 8 rows) without becoming an unbounded prompt-injection surface. `422` over the cap. |
| `style` | `StyleKey` | no | Same three presets as the chat path; default `realistic`. |
| `aspect_ratio` | `String` | no | Same allow-list as the chat path; `422` on anything else. |
| `prompt_variant` | `String` | no | Same variant selection as the chat path, including the unknown-key-falls-back-to-built-in rule. |
| `stream` | `bool` | no | Default `true`. |

### 3.3 Composer payload

Identical to the chat path's five slots, so one `filter_prompt` contract serves
both callers and a deployment's custom prompt needs no changes:

```
[人物外观] {appearance}      ← from the persona instance
[最近场景] {scene ?? （无）}  ← from the request
[对方最新消息] {content}      ← from the request
[风格] {style}
[画幅] {aspect_ratio}
```

### 3.4 Response

Both modes carry the same five fields:

| Field | Meaning |
|---|---|
| `composed_prompt` | Style preset + persona appearance + subject — the string to hand an image vendor |
| `subject` | The composer's own prompt field, before assembly |
| `caption` | The composer's short caption, `null` when it produced none |
| `model` | The model that actually answered |
| `generation_id` | For reconciling against provider logs |

`stream: false` returns them as one JSON body.

`stream: true` returns `text/event-stream`:

- `{"type":"delta","content":"…"}` frames carrying the composer's raw output as
  it arrives, verbatim and unparsed — this is what makes the endpoint usable
  for diagnosing a `filter_prompt` whose model emits malformed JSON.
- one `{"type":"done", …}` terminal frame carrying the five fields, so its
  payload minus the `type` discriminator is byte-identical to the
  `stream: false` body.
- a single `{"type":"error", …}` frame on failure, matching the chat stream's
  in-band error convention once the first byte is out.

Frame names deliberately reuse the chat stream's `delta` / `done` / `error`
vocabulary. There is no `meta` frame: `model` is not known early enough to be
worth a separate frame, and it rides the terminal frame with everything else.

A consumer that only wants the result ignores the deltas and reads the terminal
frame.

### 3.5 Non-JSON composer output

The chat path already treats a successful-but-non-JSON composer reply as the
whole reply becoming the prompt, with no caption. This endpoint keeps that
behaviour so the two paths cannot disagree: `subject` is the raw reply,
`caption` is `null`, and `composed_prompt` is assembled from it as usual.

### 3.6 Failure modes

| Condition | Response |
|---|---|
| `[tasks.chat_image_prompt_compose]` absent | `501`, mirroring the voice endpoint's `501` for an absent `[tasks.chat_voice]` |
| Instance not owned by the JWT user | `403` |
| Instance not found | `404` |
| Blank `content`, bad `aspect_ratio` | `422` |
| Over the per-user in-flight cap | `429` |
| Composer call fails | `502`, or an in-band `error` frame if streaming has begun. **No portrait fallback here** — the fallback exists to keep a chat turn moving, and this endpoint has no turn to protect. Returning a generic portrait to a caller that asked for a specific prompt would be worse than an error. |

**`502` is a new status code for this repo.** `AppError` has no upstream-failure
variant today (`Internal` is reserved and unconstructed, and maps to `500`), so
this change adds one — `AppError::Upstream(String)` → `502` — alongside the
existing variants in `error.rs`, with the same `{error, message}` body shape the
other non-stream routes return.

The reason to grow the status surface rather than reuse `500`: this endpoint is
also a composer test surface, and an operator tuning a `filter_prompt` needs to
tell "the provider rejected or failed my call" apart from "the engine broke".
Reusing `500` collapses that distinction on exactly the endpoint built to
expose it.

Scope the new variant to this endpoint. Do not retrofit existing call sites
onto it — the chat path's provider failures already have their own handling
(fallback chain, then the pseudo-ghost), and rerouting them is a separate
concern with its own blast radius.

### 3.7 Persistence and audit

Nothing is written to any table. The call emits
`log_openrouter_usage("chat_image_prompt_compose", …)` like every other
outbound path, and returns `model` / `generation_id` so the caller can
reconcile against its provider's logs.

## 4. Architecture doc corrections

This work exposed two stale claims in `docs/architecture.md` / `.zh.md`. Fix
them in the same change rather than leaving them for another audit.

- **The auth claim is wrong.** The doc says the middleware "is layered onto
  `/comp/*` only". `routes/mod.rs` attaches `require_auth` to a merged
  sub-router that also contains `/world/*` and `/bff/v1/*`; the only unauthed
  surfaces are `/healthz` (the health router, merged outside the layer) and
  `/docs` (merged in `main.rs`). Restate it as "everything except `/healthz`
  and `/docs`", and note the layer attaches to the merged router rather than to
  a path prefix — that is precisely why `/persona/*` needs no new wiring.
- **The `routes/` listing is incomplete.** It reads
  `health / companion / companion_stream / debug / mod`, omitting `voice`,
  `world_town`, `bff`, and `dto`. Update it, and add `persona`.

Both language versions. New Chinese text in 简体中文; do not touch the
pre-existing Traditional prose around it.

## 5. Docs impact

- `docs/api-reference.md` / `.zh.md` — rewrite the `force` row and delete the
  `mode` row from the `ImageReplyParams` table; drop the "`image_only` permits
  an empty `content`" sentence; add the new `422`; document the new endpoint in
  full, including the streaming frame shapes.
- `docs/model-config.md` / `.zh.md` — the `[tasks.chat_image_prompt_compose]`
  section gains the standalone endpoint as a second consumer of the same task
  and the same `filter_prompt` contract, and states the five-slot payload is
  shared.
- `docs/architecture.md` / `.zh.md` — §4, plus the new `/persona/*` namespace in
  the data-flow section.
- `examples/model_config.toml` — the composer block's comment mentions it now
  serves both the chat path and the compose endpoint.

## 6. Verification

- Unit: `force` resolves to `ReplyImage` regardless of any leftover `mode` key
  in the JSON; blank content on a forced turn is rejected; `force` without the
  composer task returns `422`; `force` + tips still `422`.
- Unit: a request carrying `"mode": "image_only"` parses and is ignored — pins
  the silent-ignore decision so it cannot regress into a deserialization error.
- Endpoint: happy path in both modes returns the same five fields; `scene`
  omitted renders `（无）` in the payload while `scene` supplied does not — the
  mirror of the existing `forced_image_without_pde_still_feeds_the_scene` test;
  ownership `403`; absent composer `501`; over-cap `429`; non-JSON composer
  output yields raw `subject` and `null` caption.
- OpenAPI snapshot regenerated and committed — CI diffs it.
- Full `cargo fmt` / `clippy -D warnings` / `test --workspace --all-features`.

## 7. Breaking — ships in 1.0.1

Three consumer-visible changes on the chat stream:

1. `force = true` no longer produces a text reply — it is always `reply_image`.
2. `content` is required on forced turns; the `force` + `image_only`
   empty-content exemption is gone.
3. `mode` no longer does anything. A leftover key deserializes and is ignored
   (§1), so a consumer still sending `"mode": "text_image"` silently gets an
   image-only turn instead of text + image.

**The version number will not signal any of this**, so the release notes carry
the whole burden. Item 3 is the dangerous one: it fails silently on the
consumer side rather than erroring, which is the exact trade accepted in §1 in
exchange for not accumulating tombstone fields this early. Notes must state it
explicitly rather than leaving it to a table row.

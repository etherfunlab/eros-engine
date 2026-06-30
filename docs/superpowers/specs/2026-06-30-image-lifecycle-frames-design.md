# Image-generation lifecycle frames (image_pending / image_attempt / image_failed)

**Date:** 2026-06-30
**Issue:** #131 — chat stream: add image-generation lifecycle frames so SSE consumers can render a generating state
**Scope:** one PR. New `ProtocolFrame` variants + emission wiring in `pipeline/stream.rs`, a streaming seam in `execute_image`, and docs. Additive on the wire.

## Problem

The chat SSE stream gives consumers no signal *during* image generation, and
no signal at all when generation fails. The two image actions behave
differently, and one of them has a multi-second blind window:

- **`reply_image`** (`stream.rs:2492`) generates the image **first**, then emits
  `meta → image → done` as a tight burst. The image is already present when the
  bubble's first frame arrives. A consumer that shows a "generating" indicator
  while the message is unsettled toggles it cleanly. But the wait happens
  *before* `meta`, so it is invisible — the consumer can't show progress during
  it — and on failure the whole turn silently degrades to a plain `reply_text`.

- **`reply_text_image`** (`stream.rs:2964`) streams the text, emits the text
  **`done`**, then **blocks on `execute_image(...).await`** (which also includes
  an LLM `run_image_prompt_compose` call), then emits **`image`**, then `final`.
  Between `done` and `image` there is a multi-second gap with **no frame at
  all**. A consumer that gates its "generating image" indicator on the message
  still being unsettled (on `done` not having fired) loses the signal at `done`,
  while the image is still generating.

- On **image-gen failure** for `reply_text_image` (chain exhausted / zero images
  / config error), **no frame is emitted** — the image is silently dropped (only
  a `ChainExhausted` diagnostic is persisted to row metadata). A consumer can't
  clear its pending state or render a "couldn't generate" state; it waits until
  `final`.

The only robust "image is generating" signal a consumer has today is inferred
from `action_type` + frame timing, and for `reply_text_image` that inference
(`done` not yet fired) is *wrong*, because `done` fires before generation.

## Goal

Add explicit image-generation lifecycle frames so a consumer can drive a
generating → done / failed state deterministically and consistently across both
image actions:

1. `image_pending` — the engine has committed to generating an image for a
   message; start the spinner.
2. `image_attempt` — live fallback-chain progress ("trying model X (i/N)").
3. `image` — success terminal (existing frame; unchanged).
4. `image_failed` — the fallback chain gave up; clear pending, render failed.

## Background — the existing image paths

- Both image arms live in one match arm of the `run_stream` generator
  (`stream.rs:2469`, `ActionType::ReplyText | ReplyImage | ReplyTextImage`).
- **`reply_image`**: generate first. On success a new `msg_ulid` is created
  **after** the gen returns (`stream.rs:2586`), the row is persisted, and
  `meta → image → done` is emitted (`stream.rs:2626`). On failure
  (`ChainExhausted`) an `image_failed` diagnostic is stashed in
  `image_failed_meta` and the arm falls through to the normal text path, which
  produces a separate text row and attaches the diagnostic to it
  (`stream.rs:2923`). `Config` / zero-image failures only `warn` and fall
  through.
- **`reply_text_image`**: the text burst runs first (`drive_chat_burst`,
  `stream.rs:2887`) producing `meta → delta* → done` and a persisted assistant
  row. Then, inside `if let (Some(last), Some((primary, fallback)))`
  (`stream.rs:2965`), the image is generated and merged onto that **same** row
  (`merge_assistant_image_meta`), and the `image` frame is yielded before
  `final`. On failure: `ChainExhausted` persists `image_failed` onto the row;
  `Config` / zero-image only `warn`. No frame is emitted in any failure case.
- **`execute_image`** (`openrouter.rs:822`) walks `plan_attempts(candidates,
  prompt, prompt_original)` — a precomputed list of `(model, variant, prompt)`
  tuples (so the attempt **count is known up front**). Each iteration does one
  HTTP post and pushes an `ImageAttempt { model, variant, outcome }`. First
  attempt with ≥1 image returns `Ok(ImageGenResponse { .., attempts,
  winning_variant })`; full exhaustion returns `Err(ChainExhausted {
  attempts })`; pre-flight problems return `Err(Config(_))`. The per-attempt
  detail therefore exists already — it is just only available **at the end**.
- The SSE protocol is **not** modeled in `openapi.json` (confirmed: zero
  matches). It is documented in `docs/api-reference.md`. So no OpenAPI
  regeneration is needed for new frame types.
- `execute_image` has exactly two callers, both in `stream.rs` — so its
  signature can be extended freely.

## Design

### 1. New `ProtocolFrame` variants

Added to the `ProtocolFrame` enum (`stream.rs:42`, `#[serde(tag = "type",
rename_all = "snake_case")]`). Each frame has exactly one job, so payloads stay
minimal.

```rust
ImagePending {
    message_id: String,
},
ImageAttempt {
    message_id: String,
    model: String,
    variant: PromptVariant,   // composed | original | single (existing enum)
    index: u32,               // 1-based position in the attempt plan
    total: u32,               // total planned attempts
},
ImageFailed {
    message_id: String,
    reason: ImageFailReason,
},
```

New small enum (lives next to the frame types, snake_case Serialize):

```rust
pub enum ImageFailReason {
    ChainExhausted,  // every candidate failed
    ZeroImages,      // defensive: a success return carried zero images
    ConfigError,     // no api key / no models configured
}
```

These map 1:1 onto the three non-success arms of the existing `match
execute_image(...)` blocks.

**Payload rationale (deliberate minimalism):**

- `image_pending` carries only `message_id`. Its single job is "an image is
  coming for this message." The candidate model can differ from the served model
  (fallback), so naming a model here would mislead; the model narrative belongs
  to `image_attempt`, and the authoritative model/aspect/prompt arrive on the
  terminal `image` frame.
- `image_attempt` carries the per-attempt `model` + `variant` + `index`/`total`
  — enough for "trying A (1/3)…", "trying A alt-prompt (2/3)…", "trying B
  (3/3)…". This is **start-only**: one frame as each attempt begins. The
  attempt's failure outcome is implied by the next `image_attempt` (or by the
  terminal `image` / `image_failed`); the full per-attempt outcome
  (transport / status+code / zero_images / decode) is still persisted to DB
  metadata for diagnostics, unchanged.
- `image_failed` carries `message_id` + a coarse `reason`. The per-model failure
  story lives in the `image_attempt` frames and the persisted diagnostic, so the
  frame itself stays small.

`PromptVariant` is reused verbatim from `openrouter.rs:113` (already
`Serialize`, snake_case). `ImageAttempt`/`AttemptOutcome` row diagnostics are
unchanged.

### 2. Streaming seam in `execute_image`

To surface attempts live without a detached task, the existing fallback loop is
extracted so a caller can observe each attempt as it starts:

```rust
// new progress payload (llm crate)
pub struct ImageAttemptProgress {
    pub model: String,
    pub variant: PromptVariant,
    pub index: u32,
    pub total: u32,
}

// extracted core: existing loop, plus an on_attempt hook fired
// immediately BEFORE each HTTP post.
async fn execute_image_inner(
    &self,
    req: ImageGenRequest,
    mut on_attempt: impl FnMut(ImageAttemptProgress),
) -> Result<ImageGenResponse, ImageGenError> { /* today's loop */ }

// unchanged public API: no-op hook, existing callers untouched
pub async fn execute_image(&self, req: ImageGenRequest)
    -> Result<ImageGenResponse, ImageGenError>
{
    self.execute_image_inner(req, |_| {}).await
}
```

`total` is `plan.len()`; `index` is the 1-based loop position. The hook is a
**sync** `FnMut` — it never `await`s — so it cannot reorder or block the gen
loop.

**Driving it once, via a `drive_image_gen` stream helper.** Rather than
duplicate the channel/`select!` plumbing in both image arms, the server wraps it
in a single free function (in `stream.rs`) returning a stream of events:

```rust
// server-internal
enum ImageGenEvent {
    Attempt(ImageAttemptProgress),
    Done(Result<ImageGenResponse, ImageGenError>),
}

fn drive_image_gen(
    client: Arc<OpenRouterClient>,
    req: ImageGenRequest,
) -> impl Stream<Item = ImageGenEvent> {
    async_stream::stream! {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ImageAttemptProgress>();
        let gen = client.execute_image_inner(req, move |p| { let _ = tx.send(p); });
        tokio::pin!(gen);
        let result = loop {
            tokio::select! {
                Some(p) = rx.recv() => yield ImageGenEvent::Attempt(p),
                r = &mut gen => {
                    while let Ok(p) = rx.try_recv() { yield ImageGenEvent::Attempt(p); }
                    break r;
                }
            }
        };
        yield ImageGenEvent::Done(result);
    }
}
```

Each image arm then forwards events with ~10 lines of glue — mapping `Attempt`
→ an `image_attempt` frame stamped with that arm's own `message_id`, and
capturing the single `Done`:

```rust
let mut events = Box::pin(drive_image_gen(state.openrouter.clone(), req));
let mut outcome = None;
while let Some(ev) = events.next().await {
    match ev {
        ImageGenEvent::Attempt(p) => yield ProtocolFrame::ImageAttempt {
            message_id: img_mid.clone(),
            model: p.model, variant: p.variant, index: p.index, total: p.total },
        ImageGenEvent::Done(r) => outcome = Some(r),
    }
}
match outcome.expect("drive_image_gen yields exactly one Done") { /* terminal arms */ }
```

The helper owns the `Arc<OpenRouterClient>` and polls `execute_image_inner` in
place; while it `await`s each HTTP roundtrip, the `select!` drains queued
progress into `Attempt` events. The channel is `tokio::sync::mpsc::unbounded`
(`futures-channel` is not a workspace dependency). If the SSE client
disconnects, the generator drops the boxed helper stream → the in-flight gen
future drops → the HTTP call is cancelled. The concurrency lives in **one**
place; only the ~10-line forwarding glue (differing solely by `message_id`)
appears in each arm. The helper is independently unit-tested: collect its events
against a wiremock that 500s every attempt → N `Attempt` events + one
`Done(Err(ChainExhausted))`.

### 3. Emission points & frame sequences

**`reply_text_image`** (`stream.rs:2964` block). Emit `image_pending` at the
**top** of the `if let (Some(last), Some(chain))` commit block — **before**
`run_image_prompt_compose`, because compose is itself an LLM call inside the
gap. `message_id` is the existing produced row id (`last.message_id`). Then run
the streaming seam; on each terminal arm:

| Arm | Action |
|---|---|
| `Ok(resp)` with ≥1 image | yield existing `image` frame (unchanged) |
| `Err(ChainExhausted)` | persist `image_failed` meta (unchanged) **and** yield `image_failed { reason: chain_exhausted }` |
| `Ok(_)` zero images | warn (unchanged) **and** yield `image_failed { reason: zero_images }` |
| `Err(Config)` | warn (unchanged) **and** yield `image_failed { reason: config_error }` |

Sequence:

```
meta → delta* → done → image_pending → image_attempt* → (image | image_failed) → final
```

If `image_chain` is `None` or the burst produced no row (`produced.last()` is
`None`), the commit block is skipped entirely → **none** of the three new frames
are emitted (there is nothing to attach an image to). Unchanged guard.

**`reply_image`** (`stream.rs:2492` block). Pre-allocate `msg_ulid` **before**
generation (today it is created after success). Emit `image_pending { message_id:
X }` before compose/gen, run the streaming seam, then:

- **Success:** keep `meta → image → done` using the **same** pre-allocated `X`
  (the persisted row uses `X` too). Then `image_only_done` path emits `final`.

  ```
  image_pending(X) → image_attempt*(X) → meta(X) → image(X) → done(X) → final
  ```

- **Failure** (any of the three arms): yield `image_failed { message_id: X,
  reason }`, then **fall through to the text path** exactly as today, which
  produces a separate text row `Y` and emits a normal text reply. The
  `ChainExhausted` diagnostic still persists onto `Y` (unchanged).

  ```
  image_pending(X) → image_attempt*(X) → image_failed(X) → meta(reply,Y) → delta* → done(Y) → final
  ```

  **Contract point: `X` ≠ `Y`.** `X` is the never-persisted *intended-image*
  id that `image_pending`/`image_attempt`/`image_failed` reference; `Y` is the
  real text message that the turn degrades to. A consumer clears the pending
  state for `X` on `image_failed(X)`, then renders `Y` as an ordinary text
  reply. (The persisted `image_failed` diagnostic lives on `Y`'s row — frames
  describe the failed *attempt*; the DB record is for reconstruction.)

### 4. Backward compatibility

All three new frames are new `type` discriminants. Existing consumers already
ignore unknown frame types (`docs/api-reference.md` §"Compatibility"), so they
continue to see exactly today's streams:

- `reply_image` success: `meta → image → done → final` (new frames interleaved
  but ignored).
- `reply_image` failure: `meta(reply) → delta* → done → final`.
- `reply_text_image`: `meta → delta* → done → image → final` (success);
  `… → done → final` with no `image` (failure).

No existing frame's shape changes. This is purely additive.

### 5. Error handling

- The progress channel is best-effort: `unbounded_send` never blocks; send
  errors are ignored and never fail the turn.
- The streaming seam reuses the existing fallback/diagnostic logic verbatim —
  the only new behavior is the `on_attempt` hook firing and the new `yield`s.
  All DB persistence (`merge_assistant_image_meta`,
  `merge_assistant_metadata_key`, the image-only row insert) is unchanged.
- `config_error` / `zero_images` for `reply_text_image` previously emitted
  nothing; they now also emit `image_failed`. This is the intended fix (the
  "silently dropped" gap), not a regression.

## Documentation

- **`docs/api-reference.md`**: add frame tables for `image_pending`,
  `image_attempt`, `image_failed` (+ the `reason` value list); update the "Full
  SSE frame sequences" block for both actions; update the "Failed-image client
  contract" section to describe the `reply_image` `X` ≠ `Y` flow.
- **`docs/api-reference.zh.md`**: mirror the above; new sections written in
  simplified Chinese (per repo doc-language convention).
- No `openapi.json` change (SSE frames are not modeled there).

## Testing

The image client is a concrete `OpenRouterClient` (not a trait), so there is no
mock seam for an end-to-end `run_stream` sequence test. The new concurrency,
however, is isolated in `drive_image_gen`, which IS testable on its own with the
`wiremock` harness both crates already use.

1. **Frame serialization** (`stream.rs` `#[cfg(test)]` module): serialize each
   new variant and assert the `type` tag + field shape, mirroring the existing
   `meta` / `done` serialization tests. Cover `ImageFailReason` rendering
   (`chain_exhausted` / `zero_images` / `config_error`) and `PromptVariant` in
   an `image_attempt`.
2. **`execute_image_inner` hook** (llm crate): a `wiremock` server that 500s
   every request → each candidate fails (`Status`) → `ChainExhausted`. Assert
   the `on_attempt` hook fires exactly `total` times with `index` 1..=`total`
   and correct `total`, and that the call returns `ChainExhausted` with the same
   number of attempts.
3. **`drive_image_gen` helper** (server `stream.rs` tests): a `wiremock` server
   that 500s every request; collect the event stream and assert N `Attempt`
   events + exactly one terminal `Done(Err(ChainExhausted))` (N = candidate
   count). This is the automated test for the streaming/cancellation seam.
4. **Happy path** (`reply_text_image` / `reply_image` success → `image`, and the
   `reply_image` `X` ≠ `Y` failure flow): manual verification against a live key,
   as with the existing image features.

## Out of scope

- `image_attempt` outcome frames (start-only was chosen; per-attempt outcomes
  remain in DB metadata only).
- Modeling the SSE protocol in OpenAPI.
- Any change to `execute_image`'s fallback/retry/compose logic — this PR only
  observes it.
```

# Delegate-only image drawing + engine draw endpoint

**Date:** 2026-07-06
**Scope:** engine (this repo). **Breaking.** Removes the `image.delegate` flag and
the inline chat-stream draw path, making the `image_request` frame the only
behavior; relocates the engine's image-generation machinery into a new
session-scoped SSE endpoint `POST /comp/chat/{session_id}/image/stream` that a
consumer may optionally call to have the engine draw the composed prompt.
`[tasks.chat_image_generation]` becomes optional and gates only the endpoint.

> This is a public repository. Keep the spec and all code/commits
> product-identity-free — refer to "the consumer" / "the downstream consumer",
> never a specific product, brand, or URL.

**Supersedes/extends** the E1 additive design
(`2026-07-05-delegate-image-drawing-to-client-design.md`). E1 shipped
`image.delegate` (default off) + the `image_request` frame + a delegated branch
in both image arms + a minimal `metadata.image` marker, **retaining** the inline
draw path behind the flag and deferring its removal to "E2". This change performs
that removal — and, rather than merely deleting the draw machinery, relocates it
into a callable endpoint so a consumer that does not want to build its own image
pipeline can still have the engine draw for it.

## Problem

After E1 the engine has two image paths gated by `image.delegate`:

- **delegated** (`true`): compose → emit `image_request` (base64 composed prompt +
  `image_ref` + `aspect_ratio`) → end the turn; no draw.
- **inline** (`false`, default): compose → draw on the chat stream
  (`execute_image_inner`, the model×variant retry chain) → stream
  `image_pending`/`image_attempt`/`image`/`image_failed` → persist the full
  `metadata.image` draw result; a write-back endpoint (`set_generated_image_url`)
  records the durable URL.

Carrying both is complexity the codebase no longer needs: there is a single
downstream consumer and it is migrating to the delegated model. Keeping the
default-inline path means the 15–60s draw still blocks the chat turn for the
default configuration, and the two branches must be kept in sync.

But fully deleting the draw machinery would force **every** consumer to build its
own image pipeline (call an image provider, walk the retry chain, handle
failure). That is the right long-term shape for a consumer that owns its own
storage, but it is a large ask, and some deployers will not want to run image
generation themselves at all.

## Goal

1. **Delete the flag and the inline path.** The chat stream never draws; it always
   emits `image_request`. One image path, not two.
2. **Keep engine-side drawing available — as an opt-in service, decoupled from the
   chat turn.** A new endpoint hosts the (relocated) draw machinery. On receiving
   `image_request`, the consumer chooses: call the endpoint (engine draws the
   composed prompt and streams the result), or draw the composed prompt itself.
3. **Make image generation a deployer choice.** `[tasks.chat_image_generation]`
   becomes optional; its absence disables only the endpoint (the engine won't
   draw), never the chat stream's `image_request` emission.

## Background — current state (post-E1, on `dev`)

- **Chat arms** — `crates/eros-engine-server/src/pipeline/stream.rs`, one combined
  `match plan.action_type` arm with two `if matches!` guards: `reply_image`
  (image-only) and `reply_text_image`. Each computes `subject`/`style`/
  `final_subject` via `run_image_prompt_compose` + `build_image_gen_request`
  (which calls `compose_image_prompt` to layer style preset + persona appearance +
  subject into the final wire prompt), then forks on `delegate`:
  - delegated → `build_image_request_frame` (base64 of `req.prompt`) +
    `build_delegated_image_marker` (minimal `{"image":{"prompt":<seed>,
    "aspect_ratio"?}}`) + `delegated_image_only_frames`.
  - inline (`else`) → `drive_image_gen` → `execute_image_inner`, the
    `image_pending`/`image_attempt`/`image`/`image_failed` frames, full
    `metadata.image` persistence.
- **Frame** — `ProtocolFrame::ImageRequest { message_id, composed_prompt (b64),
  image_ref: ImageRef, aspect_ratio: Option<String> }`. `ImageRef` serializes
  snake_case (`face`/`previous`).
- **Request DTO** — `ImageReplyParams` (`routes/companion_stream.rs`): `force`,
  `mode: ImageMode`, `style`, `model`, `image_prompt`, `aspect_ratio`,
  `resolution`, `face_ref_url`, `prev_image_url`, `delegate`.
- **Availability gate** — `image_executor_available = image_chain.is_some()`,
  where `image_chain = req_image.and_then(|i| effective_image_chain(i.model,
  resolve_image_gen()))`. Depends on both the request `image` block **and**
  `[tasks.chat_image_generation]`.
- **Draw helpers** (relocated by this change) — `select_image_ref`,
  `build_image_gen_request`, `drive_image_gen`, `execute_image_inner`
  (`crates/eros-engine-llm/src/openrouter.rs`), `plan_attempts`,
  `build_image_body`.
- **Write-back** — `set_generated_image_url` (`companion_stream.rs`) +
  `set_assistant_image_url` (`crates/eros-engine-store/src/chat.rs`).
- **Config** — `resolve_image_gen()` returns `Option<ResolvedImageGen>` (`None`
  when `[tasks.chat_image_generation]` absent); `effective_image_chain(req_model,
  resolved)` builds the model chain; `resolve_image_prompt_compose()` gates the
  composer.
- **OpenAPI** — `crates/eros-engine-server/openapi.json`, snapshot-checked by CI
  (`print-openapi`). `ImageReplyParams`/`StreamSendRequest` are modeled; the new
  route + body will be too. (SSE *frames* are not modeled.)

## Design

### 1. Delegate-only chat stream (remove the flag + the inline path)

The two image arms collapse to what was E1's delegated branch, now
**unconditional**. For an image turn the chat stream: judge → compose (unchanged)
→ emit `image_request` + persist the minimal seed marker → end. No `delegate`
fork, no `drive_image_gen`, no inline draw frames, no full-result persistence.

Uniform sequences (identical to E1's delegated sequences):

- image-only: `meta(reply_image) → done → image_request → final`
- text+image: `meta(reply_text_image) → delta* → done → image_request → final`

The `image_request` frame, `build_image_request_frame`, and
`build_delegated_image_marker` are unchanged from E1. The minimal marker (`{"image":
{"prompt": <seed subject>, "aspect_ratio"?}}`) is still written on every image
turn, preserving the PDE image-awareness transcript (`assistant_transcript_line`).

### 2. Availability gate flip

Image-action availability keys on the **presence of the `image` block** in the
request alone — the consumer signalling "I handle images this turn". It no longer
consults `[tasks.chat_image_generation]`. `guard_action`'s
`image_executor_available` becomes `req_image.is_some()` (the block's presence),
independent of the model config. The composer still runs when
`resolve_image_prompt_compose()` is `Some` and fails open to the seed otherwise.

### 3. `ImageReplyParams` (chat send) shrinks

Remove the fields the chat stream no longer uses (they are draw-time concerns that
move to the endpoint, §4): `model`, `face_ref_url`, `prev_image_url`,
`resolution`, and `delegate`. What remains is what the decide/compose stage needs:

```rust
pub struct ImageReplyParams {
    pub force: bool,             // force an image action
    pub mode: ImageMode,         // ImageOnly ⇒ reply_image; else reply_text_image
    pub style: Option<StyleKey>, // composer style
    pub image_prompt: Option<String>, // seed subject override
    pub aspect_ratio: Option<String>, // PDE/override aspect (composer + frame)
}
```

### 4. New draw endpoint — `POST /comp/chat/{session_id}/image/stream`

A dedicated, streaming (SSE) endpoint hosting the relocated draw machinery,
decoupled from the chat turn. The consumer opens it (per image message) when it
receives `image_request` and wants the engine to draw.

- **Auth / ownership.** Authenticated; assert the `{session_id}` belongs to the
  caller — the same guard `set_generated_image_url` used against
  `engine.chat_sessions`. Reject otherwise (`403`/`404`).
- **Request body** — E1 frame contents echoed back + today's draw knobs:

  ```rust
  pub struct DrawImageRequest {
      pub message_id: String,          // the assistant message id X from image_request
      pub composed_prompt: String,     // base64(STANDARD) of the final wire prompt (from the frame)
      pub image_ref: ImageRef,         // "face" | "previous" (from the frame)
      pub face_ref_url: Option<String>,
      pub prev_image_url: Option<String>,
      pub model: Option<String>,       // model override
      pub aspect_ratio: Option<String>,
      pub resolution: Option<String>,
  }
  ```

- **Behavior.** Decode `composed_prompt` (base64 → UTF-8); resolve the reference
  URL via `select_image_ref(image_ref, {face_ref_url, prev_image_url})`; build the
  provider body with the model chain
  (`effective_image_chain(model, resolve_image_gen())`) + the **verbatim** decoded
  prompt (no compose) + the ref URL + `aspect_ratio`/`resolution`; drive
  `execute_image_inner` (the existing model×variant retry chain). The endpoint is
  **persona-free** — it draws the given prompt and never re-composes, so it loads
  no persona.
- **Output (SSE).** The exact frames that used to be inline, now sourced here:
  `image_pending → image_attempt* → (image | image_failed)`. `image` carries the
  base64 data URL (the engine has no blob store); `message_id` echoes `X` so the
  consumer correlates the draw to its chat bubble.
- **Stateless persistence.** The endpoint persists **nothing** — no
  `metadata.image` upgrade, no write-back. The chat stream already wrote the
  minimal seed marker; the composed prompt, model, generation id, and
  success/failure are the consumer's to store (it owns its bucket and history).
- **Config-disabled path.** If `[tasks.chat_image_generation]` is absent
  (`resolve_image_gen()` is `None`), the engine cannot draw → respond with a
  **pre-stream HTTP error** (`409 Conflict` / `501 Not Implemented`, decide in the
  plan) so the consumer distinguishes "engine won't draw" (fall back to
  self-draw) from "a draw failed" (`image_failed`). The chat stream is unaffected.

### 5. Config: `chat_image_generation` optional, endpoint-only

`[tasks.chat_image_generation]` is now optional and consulted **only** by the
endpoint (§4) and by `effective_image_chain` there. The chat-stream gate (§2) no
longer consults it. A deployer who omits the block gets: chat stream emits
`image_request` normally; the draw endpoint returns the config-disabled error;
consumers must self-draw. No behavior change is needed in the composer task.

### 6. Removals

- `ImageReplyParams.delegate` and the shrunk fields (§3).
- The `delegate` fork and the entire inline-draw `else` block in both arms
  (`drive_image_gen` call, the `match img_outcome` arms, the inline
  `image_pending`/`image_attempt`/`image`/`image_failed` emission, the full
  `metadata.image` persistence, `build_image_failed_meta` if unused elsewhere).
- The write-back endpoint `set_generated_image_url` + the store method
  `set_assistant_image_url` (nothing writes a durable URL engine-side anymore).
- **Kept and relocated** to the endpoint: `select_image_ref`, `drive_image_gen`,
  `execute_image_inner`, `plan_attempts`, `build_image_body`, and a draw-request
  builder that takes the **already-composed** prompt (a variant of
  `build_image_gen_request` that skips `compose_image_prompt`).

## Breaking change & rollout

This is a hard break with no transition flag: the instant the engine deploys,
every image turn emits `image_request` and no inline `Image` frame is ever
produced. This is acceptable because there is a **single downstream consumer**,
migrated in lockstep (it either wires `image_request` → the new endpoint, or
draws the composed prompt itself). Ship the engine and the consumer together. The
paired consumer-side change lives in the consumer's own repo/spec.

## Error handling

- Composer fail-open unchanged (compose failure → seed subject → frame).
- The chat stream has no draw outcome; there is no `image_failed` on the chat
  stream — draw failure is entirely on the endpoint.
- Endpoint: base64 decode failure (malformed body) → pre-stream `400`; missing
  image config → pre-stream config error (§4); provider chain exhausted →
  `image_failed { reason: chain_exhausted }`; zero images → `image_failed
  { reason: zero_images }`; ref resolve (no `previous` URL) → `select_image_ref`
  falls back to `face`.
- Endpoint ownership failure → `403`/`404` before any provider call.

## Documentation

- `docs/api-reference.md` + `.zh.md` (simplified Chinese): drop the inline draw
  frames from the chat-stream section; state the chat stream always emits
  `image_request` (no `image.delegate`); document the new endpoint
  (`POST /comp/chat/{session_id}/image/stream`: request body, the
  `image_pending`/`image_attempt`/`image`/`image_failed` output frames, the
  config-disabled error); note `[tasks.chat_image_generation]` is now optional and
  gates only the endpoint. Product-identity-free.
- `openapi.json`: **regenerate** — the new route + `DrawImageRequest` body schema
  are modeled, and `ImageReplyParams` shrank. (The SSE frames remain unmodeled.)

## Testing

- **Chat arms**: adapt E1's integration tests — drop `delegate`; assert both arms
  **unconditionally** emit `image_request` (image-only and text+image sequences)
  and persist the minimal marker, with `[tasks.chat_image_generation]` **absent**
  (proving the gate flip: the block is no longer required for `image_request`).
- **Endpoint** (integration, wiremock provider): success → `image_pending →
  image_attempt* → image` with base64 + correct `message_id`; chain-exhausted →
  `image_failed`; `[tasks.chat_image_generation]` absent → pre-stream config
  error; ownership rejection; `previous`→`face` ref fallback; model override
  reaches the provider body; **no** DB row is mutated (stateless assertion).
- **Removals**: the old inline-draw tests and the `set_generated_image_url` tests
  are deleted; confirm no dangling references.
- Full gates: `fmt`, `clippy -D warnings`, `test --workspace`, OpenAPI snapshot.

## Out of scope

- The consumer-side change (calling the endpoint vs self-drawing) — its own repo/spec.
- Engine-side blob storage / re-serving drawn images on history reload (the
  consumer owns storage, as in E1).
- Re-adding an `original` prompt variant — the engine emits/draws one composed
  prompt.
- Any change to the composer, the PDE judge, or the decision audit.

## Open items for the implementation plan

1. Endpoint config-disabled status code: `409` vs `501` (and body shape).
2. Whether the endpoint emits `image_pending` itself (the consumer already knows
   it opened the stream) or begins at `image_attempt`.
3. The draw-request builder: adapt `build_image_gen_request` to accept a
   pre-composed prompt, or add a slimmer `build_image_gen_request_precomposed` —
   pick the lower-churn option and keep `select_image_ref` reuse.
4. Confirm `build_image_failed_meta` / any inline-only helper has no remaining
   caller after the arm removal; delete if orphaned.
5. Exact route registration + `utoipa::path` so the OpenAPI snapshot captures the
   new endpoint; confirm no unintended schema churn beyond the shrunk
   `ImageReplyParams` + the new `DrawImageRequest`.

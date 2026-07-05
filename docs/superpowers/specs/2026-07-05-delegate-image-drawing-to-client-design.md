# Delegate image drawing to the client (`image_request` frame)

**Date:** 2026-07-05
**Scope:** engine (this repo). Additive, flag-gated. A new `ProtocolFrame::ImageRequest`
variant + a delegated branch in the two image arms of `pipeline/stream.rs`, a
per-request `image.delegate` opt-in, and docs. No image is drawn on the engine
when delegated; the composed prompt is handed to the downstream consumer, which
draws it. The old in-engine drawing path is **retained unchanged** behind the
default-off flag and removed in a later cleanup phase (E2) once no consumer sends
`delegate:false`.

> This is a public repository. Keep the spec and all code/commits
> product-identity-free — refer to "the client" / "the downstream consumer",
> never a specific product, brand, or URL.

## Problem

Today the engine performs the entire image lifecycle inline on the chat SSE
stream. For a `reply_image` / `reply_text_image` turn the engine:

1. Runs an LLM composer (`run_image_prompt_compose`, task
   `chat_image_prompt_compose`) to expand the PDE seed into a detailed subject.
2. Calls the image provider itself (`OpenRouterClient::execute_image_inner`,
   `crates/eros-engine-llm/src/openrouter.rs:849-954`), walking the full
   model×variant retry chain (`plan_attempts`, `openrouter.rs:313-329`:
   `model1·composed → model1·original → model2·composed → model2·original`).
3. Streams the finished image back as base64 in the `image` frame, and
   persists `metadata.image{...}` / `metadata.image_failed{...}` onto the
   assistant row in `engine.chat_messages`.

The consequence is that **a single chat request stays open for the entire draw**.
One successful generation takes ≥15s; a chain that retries up to four times can
approach a minute, all inside the one stream. A client that cannot afford to
block that long gets a very poor turn, and a fully-failed chain degrades to a
plain text reply after ~60s of waiting. The engine also carries the whole image
lifecycle: provider retries, base64 transport, a write-back endpoint for the
durable URL, and result persistence — I/O and RTT that do not need to live here.

## Goal

Invert the responsibility: **the engine decides and composes; the consumer
draws.** For a delegated image turn the engine emits a single new frame carrying
the composed prompt and the reference choice, then ends the stream. It performs
no provider call, streams no image bytes, and persists no draw result. This:

- Removes the 15–60s draw (and its retries) from the chat stream — the turn
  returns as soon as the composer finishes (one LLM call, fail-open).
- Stops the engine from parsing/forwarding reference-image URLs and base64
  images — less RTT and I/O.
- Moves the image lifecycle (retries, upload, success/failure record) to the
  consumer, which already owns its own storage.

The composer stays in the engine (it needs persona `art_metadata`, the scene
transcript, and the deployment's tuned `filter_prompt`, all engine-side).

## Background — the current image path

- **Action enum** `ActionType` — `crates/eros-engine-core/src/types.rs:112-119`
  (`ReplyText`, `Ghost`, `ReplyImage`, `ReplyTextImage`, `Proactive`). The PDE
  bundle `ActionPlan` (`types.rs:143-160`) carries `image_prompt`,
  `image_ref: ImageRef` (`Face`/`Previous`, `types.rs:101-107`), `aspect_ratio`.
- **Decision** — the LLM judge (task `pde_decision`) yields a `PdeVerdict`
  (`stream.rs:1227-1239`); `guard_action` (`stream.rs:1463-1488`) maps it to an
  `ActionType`, gating image actions on `image_executor_available`
  (`stream.rs:2376-2384`), which is true iff the request carries an `image`
  block. `pde::plan_for` (`pde.rs:125-158`) builds the plan.
- **Composer** — `run_image_prompt_compose` (`stream.rs:1977-2040`) feeds the
  model `[人物外观]{appearance}` (from persona `art_metadata`),
  `[最近场景]{recent_scene}`, `[画面主题种子]{seed}`, `[风格]{style}`,
  `[画幅]{aspect_ratio}` (`compose_user_payload`, `stream.rs:1961-1971`). Its
  output is the enriched **subject**, not yet the wire prompt. It fails open to
  the seed subject.
- **Wire prompt** — `build_image_gen_request` (`stream.rs:166-224`) calls
  `compose_image_prompt(style, persona, subject)`
  (`crates/eros-engine-server/src/pipeline/handlers.rs:265-282`), which layers
  `style_preset(style)` + persona appearance + subject into the final text sent
  to the provider. The reference URL (`face`/`previous`) is chosen by
  `select_image_ref` (`stream.rs:141-158`) and attached to the provider body
  (`build_image_body`, `openrouter.rs:269-307`), **not** to the composer.
- **SSE frames** — `ProtocolFrame` (`stream.rs:52-120`, `#[serde(tag="type",
  rename_all="snake_case")]`): image-related variants `ImagePending`,
  `ImageAttempt`, `ImageFailed`, `Image` (`stream.rs:95-119`), plus
  `ImageFailReason` (`stream.rs:40-49`). Serialized as unnamed `data:` lines in
  `routes/companion_stream.rs:586-589`.
- **Emission** — the two arms: `reply_image` (`stream.rs:2563-2793`) and
  `reply_text_image` (`stream.rs:3077-3280`). Both call `drive_image_gen`
  (`stream.rs:244-269`) around `execute_image_inner`.
- **Persistence** — assistant row in `engine.chat_messages` (`metadata JSONB`
  added in migration `0019`); image data merged via `merge_assistant_image_meta`
  / `set_assistant_image_url` / `merge_assistant_metadata_key`
  (`crates/eros-engine-store/src/chat.rs:714-787`). The **PDE image-awareness
  transcript** annotates prior assistant turns that carry `metadata.image` (the
  `assistant_transcript_line` helper feeding `build_input_filter_transcript`) —
  see Design §5, this is the one non-obvious dependency.
- **Decision audit** — a separate table `engine.companion_decision_events`
  (migration `0028`) records the judge verdict incl. the seed `image_prompt`
  (`VerdictAudit`, `stream.rs:1620-1647`). Unaffected by this change.
- **Request DTO** — `StreamSendRequest` (`companion_stream.rs:109-135`) with
  optional `image: ImageReplyParams` (`companion_stream.rs:81-107`).
- **Version** — workspace `Cargo.toml:8` (`0.7.1-dev`). Config path via
  `MODEL_CONFIG_PATH` (`main.rs:275-281`).

## Design

### 1. Per-request opt-in — `image.delegate`

Add `delegate: bool` (`#[serde(default)]`) to `ImageReplyParams`
(`companion_stream.rs:81-107`). Default `false` → today's in-engine drawing path,
byte-for-byte. `true` → the delegated path below. The `image` block's *presence*
still drives `image_executor_available` (so the judge may pick image actions);
`delegate` only routes **how** an image action is fulfilled. No change to
`guard_action` or the availability gate.

When `delegate` is `true` the consumer no longer needs to send `model`,
`face_ref_url`, `prev_image_url`, or `resolution` (the engine neither draws nor
resolves the reference URL). The only field the composer still needs is `style`.
These fields remain accepted (ignored when delegated) for compatibility.

### 2. New frame — `ProtocolFrame::ImageRequest`

Added to `ProtocolFrame` (`stream.rs:52-120`), snake_case:

```rust
ImageRequest {
    message_id: String,
    composed_prompt: String,   // base64(STANDARD) of the UTF-8 final wire prompt
    image_ref: ImageRef,       // "face" | "previous"
    aspect_ratio: Option<String>,
},
```

- **`composed_prompt` is the *final wire prompt*** — exactly the text the engine
  would have sent to the provider today, i.e. the output of
  `compose_image_prompt(style, persona, composer_subject)` (style preset +
  appearance + enriched subject). The consumer decodes it and uses it verbatim as
  the provider text prompt; it reconstructs no prompt logic. Style presets and
  persona appearance stay engine-owned.
- **Base64.** The composed prompt is often explicit and frequently CJK. Encoding
  it (STANDARD, unwrapped, of the UTF-8 bytes) keeps the plaintext out of SSE
  transport, intermediary logs, and consumer network inspectors. The plaintext
  exists only inside the engine (before encoding) and inside the consumer's
  server (after decoding). Requires `ImageRef` to derive `Serialize`
  (snake_case) if it does not already.
- **`image_ref`** names the reference *image* choice (`face` vs `previous`) the
  plan made. The consumer holds both candidate images and resolves the actual
  URL at draw time. The engine emits only the choice — no URL.
- **`aspect_ratio`** is the PDE's semantic choice (`{1:1,3:4,4:3,9:16,16:9}` or
  absent). The consumer owns the aspect→resolution mapping (it is now a drawing
  concern). The engine does **not** emit width/height.

The engine emits `image_request`, no image bytes, and none of `image_pending`,
`image_attempt`, `image`, or `image_failed` in the delegated path — those become
the consumer's own draw-stream frames.

### 3. Delegated branch in the two image arms

A shared helper produces the frame from the already-computed composer output:
run the same compose path (`run_image_prompt_compose` → `compose_image_prompt`)
to get the final wire prompt, base64-encode it, and yield `ImageRequest` with the
plan's `image_ref` and `aspect_ratio`. Skip `drive_image_gen` /
`execute_image_inner` entirely.

**Uniform sequence — `image_request` sits between `done` and `final` in both
actions:**

- `reply_image` (image-only) delegated — persist the assistant row with empty
  `content` (as today), then:

  ```
  meta(reply_image, X) → done(X) → image_request(X) → final
  ```

- `reply_text_image` delegated — the text burst streams first (unchanged), then:

  ```
  meta(reply_text_image, X) → delta* → done(X) → image_request(X) → final
  ```

`X` is the real, persisted assistant `message_id`; the consumer keys its draw and
its own storage to `X`. (Contrast the non-delegated `reply_image` failure flow,
where the intended-image id `X` differs from a degraded text id `Y`
(`2026-06-30-image-lifecycle-frames-design.md` §3). Delegated `reply_image` has
**no** engine-side failure degrade — the engine cannot know the consumer's draw
outcome — so there is a single id and no `X`≠`Y` split. Total-failure handling is
the consumer's, by design.)

### 4. No draw-result persistence

In the delegated path the engine writes **no** `metadata.image` /
`metadata.image_failed` draw result, does not call the write-back endpoint path
(`set_generated_image_url`, `companion_stream.rs:605-671`), and does not persist
the composed wire prompt. The composed prompt, model, generation id, and
success/failure record are the consumer's to store. `companion_decision_events`
is unchanged (it already holds the seed `image_prompt` for engine-side
diagnostics; the composed wire prompt was never in it).

### 5. Preserve PDE image-awareness (the one subtle dependency)

The PDE's image-awareness transcript (`build_input_filter_transcript` via
`assistant_transcript_line`) annotates prior assistant turns **by the presence of
`metadata.image` on the chat row**. If the delegated path writes nothing, the PDE
can no longer tell it previously sent an image, breaking the iterate-vs-fresh
`image_ref` choice and the "don't repeat / stop drawing" guidance — and for
image-only turns the transcript line goes blank again (empty `content`).

Therefore the delegated path MUST still write a **minimal marker** on the
assistant row, sufficient for `assistant_transcript_line` to keep working, but
**not** the draw result. Recommended: store the short PDE **seed subject** (the
value already persisted plaintext in `companion_decision_events`) under a minimal
`metadata.image` marker — e.g. `{"image": {"subject": "<seed>"}}` — deliberately
omitting the composed wire prompt, model, generation id, attempts, url, and
success/failure. This keeps the PDE fully aware while honoring "the draw metadata
lives with the consumer" and keeping the explicit composed prompt out of engine
storage.

> Implementation note: confirm exactly what `assistant_transcript_line` reads
> (boolean presence vs. a subject string) and write the smallest marker that
> reproduces today's annotation. Do not regress the "avoid repeating the same
> composition" behavior.

### 6. What the engine keeps doing

- Judge (`pde_decision`) and composer (`chat_image_prompt_compose`) run exactly
  as today. The composer is the source of the composed prompt.
- The assistant row is still inserted (text for `reply_text_image`, empty for
  `reply_image`) so `message_id` `X` is stable and returned in `meta`.
- `companion_decision_events` audit unchanged.

## Backward compatibility

Purely additive and flag-gated:

- `image.delegate` defaults `false` → the entire existing draw path
  (`execute_image_inner`, `plan_attempts`, `drive_image_gen`, `image_pending` /
  `image_attempt` / `image` / `image_failed`, `metadata.image` persistence, the
  write-back endpoint) is unchanged. Existing consumers see today's streams.
- `image_request` is a new `type` discriminant; consumers already ignore unknown
  frames.
- No existing frame's shape changes.

**This matters because the engine deployment is shared across environments.** A
single engine build serves consumers that have and have not migrated. Shipping E1
with the flag off means unmigrated consumers keep drawing via the engine while a
migrated consumer opts in per-request with `delegate:true`. Both paths coexist
for the whole migration window.

## Error handling

- Composer fail-open is unchanged: on compose failure/timeout/empty the engine
  falls back to the seed subject, then wraps it through `compose_image_prompt` and
  emits `image_request` with that. A delegated turn never fails to emit a frame
  for a compose problem.
- The engine has no draw outcome to report in the delegated path; there is no
  `image_failed` from the chat stream. Draw failure is entirely downstream.
- `image_ref` / `aspect_ratio` follow the existing defaults when the PDE omits
  them (`face`, config default) before the frame is built.

## Documentation

- `docs/api-reference.md`: add the `image_request` frame table (fields +
  base64 note + `image_ref` values); document the delegated sequences for both
  actions; note `image.delegate` on the request DTO; state that the delegated
  path emits none of the `image_pending`/`image_attempt`/`image`/`image_failed`
  frames and persists no draw result. Keep product-identity-free.
- `docs/api-reference.zh.md`: mirror in simplified Chinese (repo convention).
- No `openapi.json` change (SSE frames are not modeled there).

## Testing

- **Frame serialization** (`stream.rs` `#[cfg(test)]`): serialize `ImageRequest`
  and assert `type:"image_request"` + field shape, incl. `image_ref` snake_case
  and that `composed_prompt` round-trips base64→UTF-8.
- **Delegated arm sequences**: with a fake/stubbed compose (or a wiremock compose
  endpoint), drive both arms with `delegate:true` and assert the frame order —
  `meta → done → image_request → final` (image-only) and
  `meta → delta* → done → image_request → final` (text+image) — and assert **no**
  `image_pending`/`image_attempt`/`image`/`image_failed` and **no** provider call.
- **Marker persistence**: assert the delegated assistant row carries the minimal
  `metadata.image` marker (subject only) and that a subsequent turn's PDE
  transcript annotates it as a prior image (the §5 regression guard).
- **Non-delegated regression**: `delegate:false` (and absent) still produces
  today's streams and persistence unchanged.

## Rollout / sequencing

- **E1 (this spec) — additive, flag-gated. Implemented and shipped first.** Add
  `image.delegate`, `ImageRequest`, the delegated branch + minimal marker, docs,
  tests. Deploy the engine; with the flag off, all environments behave exactly as
  today. This is the "engine first" work.
- Then the consumer migrates (its own repo/spec): opts in with `delegate:true`,
  draws the composed prompt itself, owns storage + settlement. Verified in a test
  environment, then promoted.
- **E2 — cleanup (later, after the consumer is fully migrated and verified in
  production).** Remove the dead in-engine drawing path: `execute_image_inner`,
  `plan_attempts`, `drive_image_gen`, the `image_pending`/`image_attempt`/`image`/
  `image_failed` emission, the write-back endpoint, and the `chat_image_generation`
  model-config task. Do **not** do this in E1 — it would break unmigrated
  consumers on the shared deployment.

## Out of scope

- Removing the old drawing path (deferred to E2).
- Moving the composer out of the engine — it stays (persona/scene/config live
  here); the stream still waits on the single composer LLM call, which is the
  small part of the latency.
- Re-adding an `original` prompt variant — the consumer's retry uses the composed
  prompt only (its decision); the engine emits one composed prompt.
- Modeling the SSE protocol in OpenAPI.

## Open items for the implementation plan

1. Confirm `ImageRef` derives `Serialize` (snake_case); add if missing.
2. Confirm the exact read shape of `assistant_transcript_line` and design the
   smallest `metadata.image` marker that preserves its annotation (§5).
3. Pick the base64 helper already in the workspace (the image-data path uses one)
   and apply STANDARD/unwrapped to the UTF-8 wire prompt.
4. Decide whether the delegated branch shares one helper across both arms (it
   should) and where it slots relative to the existing `drive_image_gen` call.

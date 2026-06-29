# Image-gen failure diagnostics + composed→original retry

**Date:** 2026-06-29
**Issues:** #122 (failure observability) + composed→original fallback retry
**Scope:** one combined PR — both tasks rewrite the same `execute_image` loop.

## Problem

Two gaps in the image-generation path, both centered on `execute_image`
(`crates/eros-engine-llm/src/openrouter.rs:716`):

1. **#122 — failures persist nothing.** When the whole image model chain fails
   (every candidate returns an HTTP error or a zero-image response), the turn
   falls through to text and *nothing about the attempt is written to the DB* —
   only `tracing::warn!` lines that scroll away. The composed prompt that
   triggered a content-policy refusal (HTTP 400) is unrecoverable, and per-model
   detail is lost because `execute_image` returns only the **last** `LlmError`.
   This makes a common, real failure mode (all providers refuse a prompt)
   undebuggable from persisted data.

2. **Retry only walks models, never prompts.** When `chat_image_prompt_compose`
   is enabled, the LLM-rewritten "composed" prompt is the only thing tried — if
   the composer over-edits or trips a content filter, the original PDE prompt is
   never attempted, even though it might have drawn fine.

Both converge on `execute_image`'s contract, so they ship together: doing them
separately would refactor the same function twice, and #122's attempts list
should naturally capture the composed→original tries.

## Background — the two prompt layers

- `subject` = the PDE/request image prompt (`plan.image_prompt`, falling back to
  `req_image.image_prompt`). The **original** subject.
- `final_subject` = when `chat_image_prompt_compose` is configured,
  `run_image_prompt_compose(...)` rewrites `subject` into the **composed**
  subject; otherwise `final_subject == subject`. On compose failure/empty it
  degrades to `subject.clone()` (`stream.rs:2462-2482`, `:2881-2901`).
- `build_image_gen_request` (`stream.rs:140`) wraps the chosen subject via
  `compose_image_prompt(style, persona, subject)` (style preset + appearance +
  subject) into the wire `prompt`. This wrapping applies to **both** variants
  identically; the composed/original distinction lives purely at the subject
  layer.

Two call sites drive image-gen:

- **image-only** (`ActionType::ReplyImage`, `stream.rs:2496`): on success emits
  Meta→Image→Done and returns; on failure logs and **falls through to the text
  path** (`image_only_done` stays false → stream continues to vision/filter/text
  burst).
- **text+image** (`ActionType::ReplyTextImage`, `stream.rs:2915`): text has
  already streamed; the image is appended after. Target row is
  `produced.last()` (`:2859`). On failure, no Image frame.

## Design

### 1. Shared core — `execute_image`'s new contract (`eros-engine-llm`)

New types in `openrouter.rs`, all `#[derive(Serialize)]` so the server layer can
serialize them straight into turn metadata:

```rust
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptVariant { Composed, Original, Single }

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AttemptOutcome {
    Status { status: u16, message: String },  // HTTP non-2xx (400 content-policy, 404, …)
    ZeroImages,
    Transport { message: String },
    Decode { message: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageAttempt {
    pub model: String,
    pub variant: PromptVariant,
    #[serde(flatten)]
    pub outcome: AttemptOutcome,
}

#[derive(Debug)]
pub enum ImageGenError {
    Config(String),                              // pre-flight: no api key / no models — no attempts
    ChainExhausted { attempts: Vec<ImageAttempt> },
}
```

Serialized `ImageAttempt` shape (matches the approved schema):

```jsonc
{ "model": "A", "variant": "composed", "outcome": "status", "status": 400, "message": "…" }
{ "model": "A", "variant": "original", "outcome": "zero_images" }
```

`execute_image` returns `Result<ImageGenResponse, ImageGenError>`:

- `ImageGenResponse` gains `pub attempts: Vec<ImageAttempt>` — the failures that
  **preceded** the win (empty when the first try succeeded).
- Total failure → `Err(ImageGenError::ChainExhausted { attempts })` carrying
  every failed try, in attempt order.
- Pre-flight config failures stay `Err(ImageGenError::Config(_))` (no attempts to
  persist).

The current callers' `Ok(_)` "zero images" arm is effectively dead —
`execute_image` only ever returns `Ok` with ≥1 image — so it folds into the
`Err` arm at both sites.

### 2. Composed→original interleave (Task 2)

`ImageGenRequest` gains:

```rust
/// The original PDE prompt, retried after `prompt` per model. Set only when
/// `chat_image_prompt_compose` actually changed the subject; `None` otherwise.
pub prompt_original: Option<String>,
```

The loop becomes **model-outer / variant-inner**:

```text
for model in candidates:                       # [A, B, C]
    for (variant, prompt) in variants(req):    # [(Composed, prompt), (Original, prompt_original)]
        send → record ImageAttempt → return ImageGenResponse on first ≥1-image success
```

`variants(req)` = `[(Composed, &prompt), (Original, original)]` when
`prompt_original` is `Some`, else `[(Single, &prompt)]`.

For `[A,B,C]` with a distinct original, the order is exactly:

```
A·composed → A·original → B·composed → B·original → C·composed → C·original
```

**Guard — when is `prompt_original` set?** Only when the composer ran *and changed
the subject*: `compose enabled && final_subject != subject && !subject.is_empty()`.
When compose is off, failed, returned empty, or returned text identical to the
subject, `final_subject == subject` → `prompt_original = None`, variant `Single`,
behavior **identical to today** (no duplicate attempt). When the original subject
is empty (no PDE/request prompt at all), there is no original to fall back to →
`None`.

`build_image_gen_request` gains `original_subject: Option<&str>`; when `Some`, it
wraps it via the existing `compose_image_prompt(style, persona, …)` into
`prompt_original` (same style+appearance wrapping as the primary). The caller at
each image site passes `Some(subject)` only when the guard holds.

> Naming note: `ImageGenRequest` already has `fallback_model` (the model chain)
> and `build_image_gen_request` already has a `fallback_subject` param (a *default*
> subject when plan/request prompts are blank — unrelated to retry). The new field
> is named `prompt_original` / `original_subject` to keep it distinct from both.

**Testability:** extract the ordering into a pure helper

```rust
fn plan_attempts<'a>(
    candidates: &'a [&'a str],
    prompt: &'a str,
    prompt_original: Option<&'a str>,
) -> Vec<(&'a str /*model*/, PromptVariant, &'a str /*prompt*/)>
```

so the 6-step interleave and the `Single` collapse are unit-tested without HTTP.
`execute_image` iterates the returned plan, performing the network call per step.

### 3. Persist diagnostics on total failure (#122)

A **separate `metadata.image_failed` key** (chosen over reusing `metadata.image`
with `failed:true`, so a consumer can never mistake a failure for a success and
`where metadata ? 'image_failed'` cleanly selects refusals):

```jsonc
metadata.image_failed = {
  "prompt": "<composed final_subject>",   // the single most useful debug field
  "style": "realistic",
  "aspect_ratio": "3:4",
  "resolution": null,
  "image_ref": null,
  "face_ref_used": false,
  "attempts": [
    { "model": "A", "variant": "composed", "outcome": "status", "status": 400, "message": "…" },
    { "model": "A", "variant": "original", "outcome": "zero_images" },
    { "model": "B", "variant": "composed", "outcome": "transport", "message": "…" }
  ]
}
```

The builder (a small `fn` in `stream.rs` taking the context fields + `attempts`)
is shared by both failure sites. Attachment:

- **text+image failure** (`stream.rs:2961-2972`): merge `image_failed` onto
  `produced.last()` — the already-streamed text row already in hand (`:2859`).
- **image-only failure** (`stream.rs:2582-2593`): the diagnostic is built at the
  failure site but the fallen-through text row does not exist yet. Stash it in a
  `let mut image_failure_meta: Option<serde_json::Value> = None;` declared before
  the image block; after the text burst captures `produced` (`:2831`), if `Some`,
  merge it onto `produced.last()`. Text fallthrough is otherwise unchanged — this
  is purely additive. (If the burst produced no row — e.g. a pseudo-ghost edge —
  the merge is a no-op `UPDATE … WHERE id = …`; the diagnostic is dropped, same
  as today. Acceptable: the common case has a text row.)

**Store helper:** add a generalized sibling of `merge_assistant_image_meta`
(`chat.rs:714`, which hardcodes the `"image"` wrapper):

```rust
pub async fn merge_assistant_metadata_key(
    &self, session_id: Uuid, message_id: Uuid, key: &str, value: &serde_json::Value,
) -> Result<(), sqlx::Error>   // metadata = COALESCE(metadata,'{}') || {key: value}, role='assistant', scoped by session
```

`merge_assistant_image_meta` can be reimplemented in terms of it (or left as-is);
the new key path uses `merge_assistant_metadata_key(.., "image_failed", ..)`.

### 4. Attempts on the success path (nice-to-have, included)

When earlier models failed before one drew the image, surface those skipped tries
on the **success** record too ("provider A refused, provider B drew it"). Since
`execute_image` now returns `resp.attempts` (the pre-win failures), thread it into
the existing `image_meta` JSON at both success sites — include `"attempts"` only
when non-empty:

- image-only success (`stream.rs:2509`): add to the `image_meta` json before the
  `AssistantInsert`.
- text+image success (`stream.rs:2927`): add to the `image_meta` json before
  `merge_assistant_image_meta`.

`attempts` carries the same `ImageAttempt` shape (failures only — the winner is
already named by `metadata.image.model` / `generation_id`), so the field is
consistent between the success and failure records.

## Testing

- **Pure / no network:**
  - `plan_attempts` — `[A,B,C]` + distinct original → the exact 6-step interleave;
    `prompt_original = None` → 3-step `Single` plan.
  - `ImageAttempt` / `AttemptOutcome` serialization — `Status` →
    `{outcome,status,message}`, `ZeroImages` → `{outcome:"zero_images"}`, etc.
  - `build_image_gen_request` — sets `prompt_original` when composed≠original,
    `None` when equal / compose off / empty subject. Extend the existing
    `build_image_gen_request_*` tests (`stream.rs:3277+`).
- **Existing tests:** update call sites/signatures (`build_image_gen_request`
  gains a param; `execute_image` callers match the new `Result` shape).
- Network-driven all-fail persistence is covered by the pure builder + the shared
  attachment path; full live mocking of OpenRouter is out of scope (consistent
  with the existing image tests, which don't mock the provider).

## Non-goals

- No change to text-fallthrough behavior (purely additive observability + an extra
  retry variant).
- No retry/backoff/timeout semantics change beyond inserting the original-prompt
  variant into the existing walk.
- `prompt_original` is engine-internal — **no new request field** exposed to web
  clients.
- No DB migration — `metadata` is `jsonb`; both `image_failed` and the new
  `attempts` are additive keys.

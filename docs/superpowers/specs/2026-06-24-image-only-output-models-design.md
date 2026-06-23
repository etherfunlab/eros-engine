# Support image-only OpenRouter output models (issue #101)

- **Date:** 2026-06-24
- **Issue:** [#101](https://github.com/etherfunlab/eros-engine/issues/101)
- **Scope:** one crate (`eros-engine-llm`); no DB migration, no SSE/route change, no version bump (rides `dev` at `0.6.3-dev`).

## Problem

`build_image_body()` (`crates/eros-engine-llm/src/openrouter.rs`) unconditionally
requests `"modalities": ["image", "text"]`. Many routable OpenRouter image models
output **image only** (e.g. `bytedance-seed/seedream-4.5`,
`x-ai/grok-imagine-image-quality`) and reject a request that also asks for text
with `404 (no endpoints support image,text)`. Only models that *also* emit text
(e.g. `google/gemini-3-pro-image`) work today, so a web client letting users pick
the image model per request (`image.model`) is limited to text-emitting models.

## Key finding — text/image generation is already decoupled

The issue's suggested fix has two parts; **only the first is an actual change.**
Part 2 ("`reply_text_image` → compose, don't conflate") is already how the engine
works:

- **`reply_image`** (`pipeline/stream.rs`, generate-first arm): calls
  `execute_image`, persists the assistant row with `content: ""`, and emits an
  image-only turn. The image model's text output is **never read**.
- **`reply_text_image`** (`pipeline/stream.rs`, text-then-image arm): the caption
  text comes from the **normal chat path** (`drive_chat_burst` → `chat_companion`),
  streamed as `meta → delta* → done`; the image is then produced by a **separate**
  `execute_image` call and appended as one `Image` frame before `final`.

So text (via `chat_companion`) and image (via the `chat_*` image task) are already
two independent OpenRouter calls; the image model is never asked for the caption.
`ImageGenResponse.text` is confirmed dead — no reader anywhere in the workspace.
`ImageGenResponse` is constructed in exactly one place (`execute_image`).

The fix therefore collapses to: **stop asking the image model for text.**

## Decision

1. **`build_image_body()` → `"modalities": ["image"]`.** Per the issue and
   OpenRouter's documented behavior, a text-capable model asked for `["image"]`
   still returns the image, so this is backward-compatible for the existing
   text-emitting models.
2. **Remove the now-provably-dead text capture** (chosen over keeping it as audit
   data): drop `pub text: Option<String>` from `ImageGenResponse` and the
   `let text = … message.content` extraction + `text,` initializer in
   `execute_image`. Keep the `first` binding and `finish_reason` (still used for
   content-filter detection). `WireMessage.content` stays — the chat / vision /
   filter paths still read it.
3. **Docs:** one additive line in `docs/model-config.md`'s chat-image section
   noting that **any** OpenRouter image model works, including image-only ones;
   mirror to `docs/model-config.zh.md` per the doc-language convention.

## Changes (file by file)

- `crates/eros-engine-llm/src/openrouter.rs`
  - `build_image_body`: `"modalities": ["image"]`.
  - `ImageGenResponse`: remove `text` field + its doc comment.
  - `execute_image`: remove the `text` extraction line and the `text,` field in
    the returned literal.
  - Test `image_body_has_modalities_and_optional_face_ref`: assert `["image"]`,
    with a comment anchoring it to #101 as the regression guard.
- `docs/model-config.md` + `docs/model-config.zh.md`: one-line image-only note.

## Compatibility / non-impact

- No SSE protocol change, no route change, no DB migration.
- `reply_image` / `reply_text_image` behavior unchanged for text-capable models;
  image-only models now work for both.
- Removing the `pub text` field is a minor pre-1.0 API trim to `eros-engine-llm`
  (the field had zero readers).

## Out of scope (YAGNI)

- No per-model `modalities` configuration.
- No change to `reply_text_image` composition (already composed) or `chat_vision`.

## Testing / gate

- Updated unit test asserts `["image"]`; existing
  `image_response_parses_data_url_from_images_array` stays green (does not touch
  `text`).
- Pre-PR gate: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D
  warnings`, `cargo test --workspace`, openapi snapshot (expected no drift —
  purely internal).

# Image generation: prior-image awareness, ref iteration, real aspect ratio, prompt composer

**Status:** design (ready for implementation)
**Area:** chat stream pipeline — interaction director (PDE) and image generation.

The per-turn interaction director (PDE) can emit `reply_image` / `reply_text_image`
and the engine generates an image via OpenRouter. This design closes four gaps in
that path. It covers **engine code support only**; the PDE prompt text and the
prompt-composer task definition live in the deployment's `model_config.toml`
(downstream) and are out of scope here.

## Problems (verified against source)

1. **PDE is blind to prior images.** `build_input_filter_transcript`
   (`crates/eros-engine-server/src/pipeline/stream.rs`) emits only `m.content` for
   assistant rows. Image turns persist empty `content`, so the transcript shows a
   blank `AI:` line. The image facts are persisted at `chat_messages.metadata.image`
   (written around `stream.rs:2304-2326`) but never surfaced to the judge.
2. **Only one reference image.** `ImageGenRequest.face_ref_url` is a single URL the
   client supplies; the engine forwards it into one `image_url` block
   (`crates/eros-engine-llm/src/openrouter.rs:164-168`). No way to continue from the
   previous generated image.
3. **Aspect ratio is ignored.** `build_image_body` folds aspect ratio into the
   **text prompt** as `"(aspect ratio: …)"` (`openrouter.rs:152-181`), which image
   models do not honor — output stays at the model default.
4. **Thin prompts.** The PDE writes `image_prompt` while also choosing the action
   and `inner_state`, on a tight token budget.

## Changes

### 1. Surface prior images into the judge transcript
- `build_input_filter_transcript`: for assistant rows that carry `metadata.image`,
  render a marker line instead of the empty `content`, e.g.
  `AI:（image sent: <subject summary>, ratio <ar>）` (wording set by implementation;
  keep it terse). This transcript is also consumed by the input filter, where the
  marker is harmless context.
- **Dependency (already satisfied):** the transcript builder reads
  `chat_repo.history(...)` (`stream.rs:1630`), which returns `ChatMessage` rows that
  already carry `metadata` (`crates/eros-engine-store/src/chat.rs:69`, `#[serde(default)]`;
  covered by the `history_row_exposes_metadata_column` test at `chat.rs:2068`). No
  struct/query change needed — the impl just reads `m.metadata.image` and renders the
  marker. (The narrower `history_slim`/`ChatMessageSlim` path drops `metadata`, so do
  **not** switch the transcript builder to it.)

### 2. PDE output schema: `image_ref` + `aspect_ratio`
- Extend `PdeVerdict` (`stream.rs`) and the resulting `ActionPlan`
  (`crates/eros-engine-core/src/pde.rs`, `types.rs`) with:
  - `image_ref`: enum `{ face, previous }`, default `face`.
  - `aspect_ratio`: optional, validated against `{1:1, 3:4, 4:3, 9:16, 16:9}`;
    unknown/missing → `None` (caller falls back, see §4). This is the **same set**
    already enforced on the request input at `companion_stream.rs:283`; factor it into
    one shared constant/helper so the PDE-output and request-input checks can't drift.
- Update `parse_pde_verdict` and `VerdictAudit` (`stream.rs:1440`) so the new fields
  are parsed and recorded into the decision-event payload.
- All new fields `#[serde(default)]` so existing prompts that omit them still parse.

### 3. Reference iteration: accept and select a previous-image ref
- `ImageReplyParams` (`crates/eros-engine-server/src/routes/companion_stream.rs:81-100`):
  add `prev_image_url: Option<String>`, validated as an absolute `http(s)` URL with
  the same rule as `face_ref_url` (`companion_stream.rs:270`). It points at the
  previous generated image; clients backed by a private object store should pass a
  short-lived signed URL (the engine does not fetch it — the URL is embedded in the
  OpenRouter request body and fetched by the image provider at generation time).
- At the image-draw sites (`stream.rs` `ReplyImage` path ~2256 and `ReplyTextImage`
  path ~2648), select the reference:
  `ref = (plan.image_ref == previous && prev_image_url present) ? prev_image_url : face_ref_url`.
  Thread the chosen URL through `build_image_gen_request` into
  `ImageGenRequest.face_ref_url`. Record which kind of ref was used in
  `metadata.image` (extend the existing `face_ref_used` field).

### 4. Real aspect-ratio control
- Aspect-ratio resolution priority becomes
  `plan.aspect_ratio > req_image.aspect_ratio > config default`
  (extends `stream.rs:141`; the image-draw paths must pass `plan.aspect_ratio`).
- `build_image_body` (`openrouter.rs:152-181`): stop appending the
  `(aspect ratio: …)` / `(resolution: …)` text hints. Send the aspect ratio as a
  **real generation parameter**, with an explicit `width×height` resolution as the
  fallback for models that do not accept an aspect parameter.
- **Define the aspect → `width×height` mapping.** The resolution fallback needs a
  concrete pixel size when only an aspect ratio is known. Add a small table mapping
  each of the five ratios to a `width×height` (e.g. anchor on a fixed long edge per
  orientation) so the fallback is deterministic. The plumbing already exists
  (`ImageGenRequest.resolution`, `req_image.resolution`, config `default_resolution`);
  this only defines how a chosen aspect becomes a resolution when no explicit one is
  supplied.
- **Feasibility risk — resolve first:** the accepted parameter differs per image
  model. Verify it for each configured image model (probe the model's
  `/models/{slug}/endpoints` and/or a live test) before relying on it; keep the
  resolution fallback for models without aspect support.

### 5. Prompt composer task (`chat_image_prompt_compose`)
- New optional config task parsed alongside the others in
  `crates/eros-engine-llm/src/model_config.rs` (e.g. `ResolvedImagePromptCompose`
  with `model`, `fallback`, `temperature`, `max_tokens`, `filter_prompt`). Feature is
  **off** when the task is absent (mirrors the `chat_vision` gating pattern).
- In the pipeline, after an image action is decided and before image generation,
  call the composer with: persona genome (`art_metadata` + `system_prompt`), the PDE
  seed subject (`plan.image_prompt`), recent scene context (transcript), the chosen
  aspect ratio, and the style. Use the enriched result as the image subject.
- **Fallback:** on composer failure / timeout / empty, use `plan.image_prompt`
  unchanged. The composer must never block or fail the image turn.

## Backward compatibility
- New PDE output fields and `ImageReplyParams.prev_image_url` are optional.
- The `chat_image_prompt_compose` task is optional; absent ⇒ current behavior.
- The aspect-parameter change is internal to image-body construction.

## Out of scope
- Multiple reference images in one request (single ref covers iteration).
- Persisting the real action type for image rows (today's hardcoded `"reply"`);
  `metadata.image` remains the source of truth for "was an image sent."

## Open items for implementation
1. Verify the real aspect-ratio parameter accepted by each configured image
   model (§4) — the one feasibility risk.
2. Settle the aspect → `width×height` mapping for the resolution fallback (§4).
3. Decide whether the PDE task's `max_tokens` needs raising for richer seed
   subjects (config-side; coordinate with the downstream `model_config.toml`).

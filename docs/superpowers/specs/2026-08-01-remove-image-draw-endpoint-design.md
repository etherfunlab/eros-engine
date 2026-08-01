# eros-engine — remove `[tasks.chat_image_generation]` and the image draw endpoint

The follow-up promised by §11 of
`2026-07-31-multi-llm-providers-design.md`. After this PR the engine *composes
image prompts but never calls a generation API*: the chat stream emits an
`image_request` frame carrying the composed prompt, and the consumer calls
whichever image vendor it likes.

Rationale (maintainer decision, recorded in the parent spec): driving image
generation was never a primary engine capability and costs more maintenance
than it returns; image API shapes churn per vendor and are not a surface worth
generalizing behind `[providers]`.

---

## 0. Decisions (settled during brainstorm)

- **The compose-path config defaults are dropped entirely, not relocated.**
  `default_style` / `default_aspect_ratio` were documented as draw-only but in
  fact also fed the delegated-image compose path
  (`resolve_image_turn_inputs`, `stream.rs:2553`). They are removed with the
  block rather than moved to `chat_image_prompt_compose` or a new `[image]`
  table. Style and aspect are now per-turn only; every delegated image turn
  already carries a request `image` block (availability keys on its presence),
  so the consumer is in the loop each turn. `default_resolution` was already
  consumed by no production code.
- **A leftover `[tasks.chat_image_generation]` block refuses to boot.**
  `tasks` is a `HashMap<String, TaskConfig>`, so after removal a leftover block
  would otherwise parse silently as an inert never-queried task and the
  operator could believe the engine still draws. Loud-fail instead, matching
  the `openrouter` reserved-name and `VOYAGE_API_KEY` precedents. The message
  states the engine no longer draws, emits only `image_request`, and asks the
  operator to delete the block.
- **Single PR, full removal.** No deprecation release, no server/llm split.
  Everything is subtraction and the pieces interlock (frames ↔ endpoint,
  chain ↔ client methods); staging would only create dead-code intermediate
  states.
- **Version bumps are out of scope.** Removing `pub` items from
  `eros-engine-llm` is a breaking change under 0.x; the version and release
  timing are decided by the maintainer at release time, not in this PR.

---

## 1. Removed

**`eros-engine-llm/src/openrouter.rs`**

| Item | Site (pre-PR) |
|---|---|
| `ImageAttempt`, `ImageAttemptProgress` | 195, 207 |
| `ImageGenError` (+ `Display`/`Error` impls) | 214–237 |
| `ImageGenRequest`, `ImageGenResponse` | 338, 361 |
| `build_image_body`, `plan_attempts` | 411, 455 |
| `execute_image`, `execute_image_inner` (incl. flagged-input scrub) | 1174, 1184 |
| all image unit tests + the image wiremock test | ~3685–3900, 4359 |

The PR #202 regression test "draw endpoint stays on OpenRouter for
`req_model = \"x@venice\"`" is deleted too — the path it locks no longer
exists.

**`eros-engine-llm/src/model_config.rs`**

| Item | Site (pre-PR) |
|---|---|
| `TaskConfig.default_style` / `default_aspect_ratio` / `default_resolution` | 685–693 |
| `ResolvedImageGen` | 1078 |
| `resolve_image_gen()` | 1691 |
| `effective_image_chain()` | 1853 |
| the `IMG_TASK` `@`-rejection special case in `validate_providers_with` (the `draw` flag in the scan tuple disappears) | 2123 |
| tests for all of the above, incl. `image_generation_rejects_any_at` and friends | 4447–4550, 5747–5770 |

Within the 4447–4550 range, the `style_preset` content assertions
(`style_preset_maps_keys`, ~4477) are **kept** — `StyleKey` /
`style_preset` survive per §2; only the `resolve_image_gen` /
`effective_image_chain` cases go.

**`eros-engine-server/src/pipeline/stream.rs`**

| Item | Site (pre-PR) |
|---|---|
| `ImageFailReason` | 52 |
| `ProtocolFrame::{ImagePending, ImageAttempt, Image, ImageFailed}` | 124–137 |
| `select_image_ref` | 182 |
| `ImageGenEvent`, `drive_image_gen`, `draw_image_frames` | 200, 216, 249 |
| `data_url_mime` | 373 |
| the `resolved_image_gen` parameter of `resolve_image_turn_inputs` and the `resolve_image_gen()` read + comment | 2553, 2987 |
| tests: frame serialization, draw frame-flow, `select_image_ref`, `data_url_mime`, `mk_resolved_image_gen` helper | 4084–4260, 11969–12260 |

The `log_openrouter_usage("chat_image_generation", …)` call site dies with
`draw_image_frames`; no other audit path references that task name.

**`eros-engine-server/src/routes/companion_stream.rs`**

`DrawImageRequest` (184), `validate_draw_request`, `draw_image_stream` (679),
its `routes!` registration (800), and the draw-route integration tests.
`openapi.json` is regenerated; the path and the `DrawImageRequest` schema
disappear from it.

**The one addition.** Boot validation rejects a leftover
`[tasks.chat_image_generation]` block, following the established
`Result<(), String>` validator shape with `anyhow::bail!` at the `main.rs`
call site. Error message: the engine no longer draws, the chat stream emits
`image_request` and the consumer draws, delete this block.

---

## 2. Kept

- `[tasks.chat_image_prompt_compose]` — the engine's only image-related task.
  It remains an ordinary chat-shaped task and supports `@provider`.
- The `image_request` frame and `build_image_request_frame`.
- `ImageReplyParams` (incl. `style`, `prompt_variant` from PR #201) and the
  PDE actions `reply_image` / `reply_text_image`.
- `StyleKey` / `style_preset` — still consumed by `compose_image_prompt`.
- `eros_engine_core::types::ImageRef` — still carried by `image_request`.

---

## 3. Behavior changes

1. `POST /comp/chat/{session_id}/image/stream` → 404 (route gone); OpenAPI
   entry removed.
2. The SSE protocol loses `image_pending` / `image_attempt` / `image` /
   `image_failed`. `image_request` is unchanged. None of the four was ever
   emitted by the chat stream, only by the draw endpoint.
3. Delegated-image style precedence becomes **request `image.style` →
   `Realistic`**; aspect becomes **PDE plan → request → none**. The
   deployment-level defaults are gone — flag as breaking in release notes.
4. A deployment upgrading with the block still in its TOML refuses to boot
   (decision §0).
5. Usage-log lines tagged `chat_image_generation` stop appearing (the calls
   themselves no longer exist). No schema change anywhere.

---

## 4. Testing

- **New:** boot rejection of the leftover block — one red case (block present
  ⇒ error mentioning `image_request`) and one green case (no block ⇒ ok).
- **New:** `resolve_image_turn_inputs` with its new signature — request
  `style` wins; absent ⇒ `Realistic`; aspect resolves plan → request →
  `None`.
- **Kept as regression lock:** the PR #202 green case asserting
  `chat_image_prompt_compose` accepts `@provider` — now guarding that the
  removal does not clip the compose task.
- Deleted tests are exactly those listed in §1. Call-form changes forced by
  the signature change are allowed; no assertion is weakened.
- Gates before PR: `cargo fmt`, `clippy`, full `cargo test`, openapi
  regeneration with a clean diff.

---

## 5. Documentation

- `docs/model-config.md` / `.zh`: delete the `chat_image_generation` section;
  rewrite the note (currently near `model-config.md:352`) that image-action
  availability no longer depends on the block — the engine now only composes
  and emits the frame.
- `docs/api-reference.md` / `.zh`: delete the draw endpoint and the four
  frame entries; the `image_request` entry gains one sentence: the engine
  does not draw, the consumer calls its own image vendor.
- `README.md` / `README.zh.md` / `README.ja.md`: remove every
  `chat_image_generation` / draw-endpoint mention (all three files currently
  match a `chat_image_generation` grep).
- `examples/model_config.toml`: delete the sample block.

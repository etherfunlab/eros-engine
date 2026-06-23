# eros-engine — Companion image replies (`reply_image` / `reply_text_image` executor) (Spec)

**Status**: design, pending implementation plan
**Target release**: `0.6.x` dev track. **No migration** — the generated-image
reference and its prompt/params ride on the existing `engine.chat_messages.metadata`
JSONB column. No new columns.
**Audience**: anyone implementing companion-sent images on
`POST /comp/chat/{session_id}/message/stream`.
This is the **output-direction mirror** of `chat_vision`
(`2026-06-02-chat-vision-image-input-design.md`): vision *describes* an image the
user sent; this executor *generates* an image the companion sends.

---

## 0. Background

### What already exists

The LLM-based PDE (`2026-06-04-llm-based-pde-design.md`, shipped in #83/#90)
already decides each turn's action via its judge:
`reply_text` | `ghost` | `reply_image` | `reply_text_image`, and already emits an
`image_prompt: Option<String>` ("what photo to send"). **Today both image actions
are degraded to `ReplyText`** in `guard_action()` (`stream.rs:1160`,
`// image executor not shipped → always degrade to text`). The `image_prompt`
is logged to the `companion_decision_events` audit table but **dropped from the
`ActionPlan`** (`ActionPlan` has no `image_prompt` field — `types.rs:112`).

`ModelSpec` (the `""` fixed / `[]` round-robin / `{}` weighted forms,
`2026-05-23-model-spec-rr-weighted-design.md`) already exists in
`model_config.rs`.

So this spec is the **executor**: it flips the reserved actions into real ones,
builds the OpenRouter image-generation call, and wires delivery + persistence.

### The feature

When the turn's action is `reply_image` or `reply_text_image`, the engine calls
an image-generation model (via OpenRouter), and delivers the generated image to
the client.

- `reply_text_image` — a normal streamed text reply **plus** a generated image.
- `reply_image` — a generated image **only** (no text reply).

Three capabilities, all driven by per-turn frontend params:
1. **Model selection** — config `ModelSpec` (`""`/`[]`/`{}`) **or** a per-turn
   single-id override from the frontend.
2. **Style** — one of three engine-owned presets (`realistic` /
   `semi_realistic` / `anime`), selected per turn by key.
3. **Resolution + aspect ratio** — per-turn params, with config defaults.
4. **image2image** — an optional per-turn `face_ref_url` (e.g. the character's
   avatar) passed to the model as a face/appearance reference, so a "selfie"
   looks like the persona.

### Why it mirrors `chat_vision`

`chat_vision` is the direct precedent and constrains the shape:

| | `chat_vision` (input) | this executor (output) |
|---|---|---|
| Trigger | `image_url` present on the user turn | action is `reply_image`/`reply_text_image` (PDE-decided **or** frontend-forced) |
| OpenRouter call | `execute_vision` (multimodal describe) | `execute_image` (image generation) |
| Engine handles bytes? | **No** — forwards a client URL | **No** — relays base64 to the client; persists only a URL the client writes back |
| Persisted | `metadata.vision` (description) | `metadata.image` (prompt/params + written-back URL) |
| Failure | fail-open: neutral placeholder | fail-open: deliver text / fall through to a text reply |
| Migration | none (rides `metadata`) | none (rides `metadata`) |

### eros-engine scope boundary

eros-engine is OSS and owns neither storage nor deploy. OpenRouter image-gen
models return the generated image as **base64 data-URLs** in the response body,
so the engine *transiently* holds the bytes — but it **never persists bytes**.
It relays the data-URL to the client in an SSE frame; the client uploads it to
**its own** storage and writes the resulting `https` URL back (§9). The engine's
durable record is a URL + text, symmetric to `chat_vision`.

---

## 1. Goals / non-goals

**Goals**
- Optional, off-by-default image replies, gated by the presence of a
  `[tasks.chat_image_generation]` task — absent task ⇒ feature off ⇒ both image
  actions degrade to `ReplyText` exactly as today (backward compatible,
  byte-for-byte).
- Both decision authorities: the PDE judge decides autonomously (today's path),
  **and** the frontend can force an image this turn.
- Per-turn frontend control of style / model / resolution / aspect_ratio /
  face reference.
- Config `model`/`fallback` are **optional**: a task block with no `model`
  enables the executor but defers the model entirely to the per-turn frontend
  param. No model resolvable for a turn (config has none, frontend passed none)
  ⇒ that turn degrades to `reply_text`.
- image2image via an optional per-turn face reference URL.
- Generated image delivered as a data-URL SSE frame; client stores it and writes
  the resulting `https` URL back to the engine.
- Fail-open: any image-gen failure still produces a reply (text), never a hard
  stream failure.
- Future turns "remember" the companion sent a photo (textual history fold).
- Affinity is evaluated on image turns (see §11) — image turns are no longer
  skipped.

**Non-goals (YAGNI)**
- Multiple images per turn (single image only).
- Engine-side image byte storage or upload.
- Config-overridable style preset *strings* (the three presets are engine-owned
  constants in v1; the frontend selects which by key).
- A dedicated prompt-builder LLM step — the gen prompt is composed
  deterministically (no extra LLM call beyond the image call itself).
- Per-turn `model` as a full `ModelSpec` (per-turn override is a single id
  string; the `ModelSpec` forms live only in config).
- Safety/NSFW classification of generated images.
- Native video / animation.
- Insight & memory extraction on `reply_image` turns that carry no text (they
  have nothing to extract); these stay covered by the existing
  section-presence/empty-assistant gating.

---

## 2. API surface — `StreamSendRequest.image`

Add one optional field
(`crates/eros-engine-server/src/routes/companion_stream.rs`, `StreamSendRequest`):

```rust
#[serde(default)]
pub image: Option<ImageReplyParams>,
```

```rust
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ImageReplyParams {
    /// Force an image this turn, overriding the PDE for this turn only.
    #[serde(default)]
    pub force: bool,
    /// TextImage (text reply + image) | ImageOnly (image, no text).
    #[serde(default)]
    pub mode: ImageMode,                  // default: TextImage
    /// One of the three engine-owned presets; None ⇒ task default_style.
    #[serde(default)]
    pub style: Option<StyleKey>,          // Realistic | SemiRealistic | Anime
    /// Per-turn single-id override of the config ModelSpec. None ⇒ config.
    #[serde(default)]
    pub model: Option<String>,
    /// Subject for the forced path (PDE path uses the judge's image_prompt).
    #[serde(default)]
    pub image_prompt: Option<String>,
    /// None ⇒ task default_aspect_ratio.
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    /// Model-specific (e.g. "1024x1024"). None ⇒ task default_resolution.
    #[serde(default)]
    pub resolution: Option<String>,
    /// image2image face/appearance reference. http/https.
    #[serde(default)]
    pub face_ref_url: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImageMode { #[default] TextImage, ImageOnly }

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum StyleKey { Realistic, SemiRealistic, Anime }
```

**Validation** (extend `validate_payload`):
- `face_ref_url` (when `Some`): reuse `image_url_is_valid` (absolute http/https,
  no whitespace, ≤ `MAX_IMAGE_URL_LEN` = 2048), else `422`
  (`user_message: "脸部参考图链接无效"`).
- `aspect_ratio` (when `Some`): must be one of an allowed set
  `{"1:1","3:4","4:3","9:16","16:9"}`, else `422` (`"不支持的画幅比例"`).
- `resolution` (when `Some`): bounded length + `^[0-9]{2,5}x[0-9]{2,5}$` shape;
  else `422`. (Opaque beyond the shape check — forwarded to the model.)
- `force` **with** `tips_amount_usd`: `422` (`"打赏消息暂不支持图片回复"`) —
  mirrors `chat_vision`'s image+tip rejection (tip turns persist as `gift_user`
  and don't drive the main reply path).
- The empty-content rule is unchanged: a forced image turn still requires the
  usual non-empty `content`/tip/image_url unless `mode = ImageOnly` (an
  image-only request may carry empty `content`):
  ```text
  if content.is_empty() && tips_amount_usd.is_none() && image_url.is_none()
     && !(image.is_some() && image.force && image.mode == ImageOnly)
  → 422 "请输入一条消息"
  ```

OpenAPI: regenerate `openapi.json` after the DTO change (PR gate).

---

## 3. Decision layer

### 3.1 `image_prompt` propagates into `ActionPlan`

Add to `ActionPlan` (`crates/eros-engine-core/src/types.rs:112`):

```rust
pub image_prompt: Option<String>,   // subject for the image executor; None for text/ghost
```

`pde::plan_for` / the PDE decision wiring sets it from the verdict's
`image_prompt` (today it is read only for the audit row). All existing
constructors of `ActionPlan` set `image_prompt: None` (text/ghost paths).

### 3.2 Effective model resolution (per turn) — unified candidate list

The image model chain is resolved **per turn** from one ordered candidate list,
and is the *real* gate on whether an image can be produced. The head is the
primary; the tail is the fallback chain:

```rust
/// Returns None ⇒ no model anywhere ⇒ the turn cannot generate (→ degrade).
/// Some((primary, fallback_chain)) otherwise.
fn effective_image_chain(
    req_model: Option<&str>,             // req.image.model (per-turn single-id override)
    resolved: Option<&ResolvedImageGen>, // resolve_image_gen() — Some iff task block present
) -> Option<(String, Vec<String>)> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(m) = req_model { candidates.push(m.to_owned()); }        // 1. per-turn override
    if let Some(r) = resolved {
        if let Some(m) = r.model.as_ref().and_then(ModelSpec::select) {  // 2. config ModelSpec
            candidates.push(m);
        }
        candidates.extend(r.fallback_model.iter().cloned());            // 3. config `fallback` (resolved)
    }
    dedup_keep_first(&mut candidates);     // drop later duplicates (model-spec Feature B dedup)
    let mut it = candidates.into_iter();
    it.next().map(|primary| (primary, it.collect()))                   // head = primary, tail = chain
}
```

- **Per-turn override wins** as the primary; the config `ModelSpec` (when set,
  `select()`-ed to one id) and then the config `fallback` entries follow, in that
  order. (TOML key `fallback` = `FallbackSpec`, string-or-array, sequential;
  resolves to `fallback_model: Vec<String>`.)
- **`fallback` alone is sufficient.** With no per-turn model and no config
  `model`, the head of `fallback` becomes the primary (the candidate list is
  non-empty). A deployment can leave `model` unset and configure only `fallback`.
- **Dedup (Feature B of `2026-05-23-model-spec-rr-weighted-design.md`):**
  `dedup_keep_first` drops later duplicates, so a just-tried model is never
  retried in the same chain. Example: frontend `model="X"`, config
  `fallback=["X","Y"]` ⇒ candidates `[X, X, Y]` ⇒ `[X, Y]` ⇒ primary `X`,
  fallback `[Y]`.
- **Empty list** (no per-turn model, no config `model`, empty `fallback`)
  ⇒ `None` ⇒ the turn degrades to `reply_text` (§3.4).

### 3.3 Forced path

In `run_stream`, before the decision is finalized: when
`req.image.is_some() && req.image.force` and `effective_image_chain(..).is_some()`,
build the plan directly as `ReplyImage` (when `mode = ImageOnly`) or
`ReplyTextImage` (when `TextImage`), with `image_prompt` =
`req.image.image_prompt` (subject resolved in §6). This **overrides the PDE for
this turn** (the judge may be skipped — it has nothing to decide). When the
effective model is `None`, the force is dropped and the turn proceeds as a
normal text/PDE turn.

### 3.4 Conditional degradation in `guard_action`

`guard_action` (`stream.rs:1142`) gains an `image_executor_available: bool` arg —
where availability means **the task block is present AND an effective model
resolved for this turn**:

```rust
PdeAction::ReplyImage     if image_executor_available => ActionType::ReplyImage,
PdeAction::ReplyTextImage if image_executor_available => ActionType::ReplyTextImage,
PdeAction::ReplyImage | PdeAction::ReplyTextImage      => ActionType::ReplyText, // can't generate
```

```rust
let image_executor_available =
    effective_image_chain(req.image.as_ref().and_then(|i| i.model.as_deref()),
                          resolved_image_gen.as_ref()).is_some();
```

So an image action degrades to `ReplyText` when **either** the task block is
absent (identical to today's behavior) **or** no model is resolvable this turn
(config has none and the frontend passed none). Both the PDE path and the forced
path share this gate.

---

## 4. Config — `[tasks.chat_image_generation]`

New task block (`crates/eros-engine-llm/src/model_config.rs`), same machinery as
`chat_vision`:

```toml
[tasks.chat_image_generation]
# `model` and `fallback` are BOTH optional. Omit `model` to enable the executor
# but defer the model choice entirely to the per-turn frontend param
# (req.image.model). When `model` IS set it reuses ModelSpec: "" fixed /
# [] round-robin / {} weighted (point 1's "explicit model").
model = "google/gemini-2.5-flash-image"   # OPTIONAL — ModelSpec ("" / [] RR / {} weighted)
# `fallback` is a FallbackSpec: a single id ("x") OR an ordered array, tried
# SEQUENTIALLY, first success wins — NOT round-robin, NOT weighted. (Note: under
# `model`, [..] means round-robin; under `fallback`, [..] means ordered retry.)
fallback = ["..."]                         # OPTIONAL — resolves to fallback_model: Vec<String>
default_style = "realistic"                # one of realistic | semi_realistic | anime
default_aspect_ratio = "3:4"
default_resolution = "1024x1365"
max_tokens = 4096
```

```rust
pub struct ResolvedImageGen {
    pub model: Option<ModelSpec>,         // optional — None ⇒ frontend must supply req.image.model
    pub fallback_model: Vec<String>,      // optional; defaults empty
    pub default_style: StyleKey,
    pub default_aspect_ratio: String,
    pub default_resolution: Option<String>,
    pub max_tokens: u32,
}

/// `None` (feature off) when `[tasks.chat_image_generation]` is absent. Presence
/// of the block is the on/off switch — no probability, no chat_companion toggle.
/// Note: `Some(_)` means the feature is ENABLED, not that a model exists — the
/// model chain is resolved per-turn by `effective_image_chain` (§3.2), which may
/// still yield `None` (→ degrade to text).
pub fn resolve_image_gen(&self) -> Option<ResolvedImageGen>;
```

Mirror `resolve_vision`. Per-turn model resolution and the optional-`model`/
`fallback` semantics live in `effective_image_chain` (§3.2): per-turn override →
config `ModelSpec` → config `fallback`, deduped, head = primary; empty ⇒ `None`
→ degrade.

**Style presets** — engine-owned constants (verbatim from the request):

```rust
pub const STYLE_REALISTIC: &str = "Photorealistic candid lifestyle photography, natural skin texture, believable anatomy, soft natural lighting, authentic smartphone photo aesthetic.";
pub const STYLE_SEMI_REALISTIC: &str = "Semi-realistic digital character illustration, believable anatomy, softly painted skin, subtly stylized facial features, detailed cinematic lighting.";
pub const STYLE_ANIME: &str = "High-quality Japanese anime illustration, clean expressive line art, detailed eyes, polished cel shading, coherent anatomy and detailed background.";
```

Add a `chat_image_generation` example block to
`examples/model_config.toml(.example)`.

---

## 5. OpenRouter `execute_image` (new, mirrors `execute_vision`)

New in `crates/eros-engine-llm/src/openrouter.rs`:

```rust
pub async fn execute_image(&self, req: ImageGenRequest) -> Result<ImageGenResponse, LlmError>;

#[derive(Debug, Clone, Default)]
pub struct ImageGenRequest {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub prompt: String,                  // composed in §6
    pub face_ref_url: Option<String>,    // Some ⇒ image2image
    pub aspect_ratio: Option<String>,
    pub resolution: Option<String>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageGenResponse {
    pub images: Vec<String>,             // base64 data-URLs (choices[].message.images[])
    pub text: Option<String>,            // any accompanying text the model returns
    pub generation_id: Option<String>,
    pub model: Option<String>,
    pub usage: Option<serde_json::Value>,
    pub finish_reason: Option<String>,
}
```

Wire body (per chain entry) — `modalities` requests image output; image2image
adds the reference as an `image_url` block in the user content (same shape as
`build_vision_body`):

```json
{
  "model": "<model>",
  "modalities": ["image", "text"],
  "messages": [
    { "role": "user", "content": [
        { "type": "text", "text": "<composed prompt> (aspect ratio: <ar>)" },
        { "type": "image_url", "image_url": { "url": "<face_ref_url>" } }  // only when image2image
    ]}
  ],
  "max_tokens": <n>
}
```

- The generated image is read from `choices[0].message.images[]`, each entry
  `{ "type": "image_url", "image_url": { "url": "data:image/png;base64,..." } }`.
  Take the first; ignore the rest (single image, §1).
- `aspect_ratio` / `resolution` are folded in a model-appropriate way (Gemini
  image models honor an aspect-ratio hint in the prompt/`image_config`; the body
  builder is a pure, unit-tested function so the exact placement is swappable per
  model). Treated as best-effort hints, not hard guarantees.
- Fallback chain walked on transport failure (error/timeout/no image), identical
  to `execute()`. A success with zero images ⇒ treated as a transport-level miss
  → next chain entry; chain exhausted ⇒ `Err`/empty handled by §7 fail-open.
- **Model ids are opaque — no pre-validation.** The engine never checks a model
  id (per-turn or config) against an OpenRouter catalog or an image-capability
  list (consistent with the chat path and OSS scope: deployments/frontends own
  model governance). An **unavailable** per-turn model — a typo (OpenRouter
  `404 model not found`), a model that is down/rate-limited (transport error), or
  a non-image-capable model (returns text, **zero images**) — simply fails its
  attempt and the walk moves to the next chain entry. With no fallback configured
  and a single bad model, the chain is immediately exhausted → §7 fail-open
  (text). A config `fallback` is the safety net here.
- Reuse app-attribution headers. Log usage via
  `log_openrouter_usage("chat_image_generation", None, …)` so the call shows up
  in the OpenRouter audit alongside the other non-chat tasks.

> Keep `ChatMessage` text-only. The block-array + `modalities` body lives in a
> private wire struct local to `execute_image` (as `execute_vision` does).

---

## 6. Prompt composition (deterministic — no extra LLM)

A pure helper next to the prompt builders in `pipeline/handlers.rs`:

```rust
/// Compose the final image-gen prompt. Pure, unit-testable.
pub(crate) fn compose_image_prompt(
    style: StyleKey,
    persona: &CompanionPersona,
    subject: &str,
) -> String;
```

Composition (blank parts omitted):

```text
<STYLE preset for `style`>
<persona appearance>          ← meta_str(persona, "appearance"), omitted when absent
<subject>
```

**Subject resolution** (the `??` chain):

```text
subject = pde_or_plan.image_prompt        // PDE path (or forced w/ explicit prompt)
        ?? req.image.image_prompt          // forced path explicit prompt
        ?? effective_user_text(user_msg)   // last resort: the user's own message
```

`persona appearance` is an **optional** new `art_metadata` key
(`meta_str(persona, "appearance")`) — additive, non-breaking, read only here;
absent on existing personas ⇒ omitted. (Optionally seed it in
`examples/personas/aria.toml`.)

`face_ref_url` is *not* part of the text prompt — it rides as the image2image
input image (§5).

---

## 7. Pipeline execution (fail-open)

Insertion point: `pipeline/stream.rs`, the
`ActionType::ReplyText | ReplyImage | ReplyTextImage` arm (currently @ ~L2007),
after the existing `chat_vision` / input-filter pre-stages.

- **`ReplyTextImage`**:
  1. Run the existing streamed chat-companion call → `meta(reply_text_image)` +
     `delta*` (text), exactly as `ReplyText` today.
  2. Then one `execute_image` call (compose prompt §6; per-turn or config model;
     style/ar/resolution; optional `face_ref_url`).
  3. Emit the `Image` frame (§8) with the first data-URL, then `done` + `final`.
- **`ReplyImage`**:
  1. Skip the chat-companion call entirely.
  2. `meta(reply_image)` → one `execute_image` call → `Image` frame → `done` +
     `final`. No `delta` frames.
- **Latency/UX**: `reply_text_image` shows text immediately; the image (one
  synchronous round-trip, several seconds) lands after. Acceptable — not a hot
  loop. `reply_image` is a single image round-trip.

**Fail-open policy**
- `reply_text_image`, image-gen fails (chain exhausted / no image) → deliver the
  already-streamed text, emit **no** `Image` frame, log a warning. The turn
  succeeds as a text reply.
- `reply_image`, image-gen fails → **fall through to a normal `ReplyText`
  generation** (run the chat-companion path now) so the turn is never empty.
  (Costs one extra round-trip on the rare failure — accepted.)

---

## 8. SSE protocol

`crates/eros-engine-server/src/pipeline/stream.rs`:

- Extend `FrameActionType` (`stream.rs:32`, currently `Reply | Ghost`) with
  `ReplyImage`, `ReplyTextImage`, so the client learns the turn shape from the
  `meta` frame.
- New `ProtocolFrame` variant:

```rust
Image {
    message_id: String,
    data_url: String,                 // "data:image/png;base64,..."
    mime: String,                     // "image/png"
    image_prompt: Option<String>,     // the subject used (also persisted)
    model: Option<String>,            // image model actually served
    generation_id: Option<String>,
},
```

- `reply_text_image`: `meta → delta* → image → done → final`.
- `reply_image`: `meta → image → done → final`.

**Client contract for a failed image** (no new error frame): the `meta` frame's
`action_type` declares the intended shape. If `action_type` is
`reply_text_image`/`reply_image` but **no `Image` frame arrives before `done`**,
the image failed (§7 fail-open) — the client renders whatever text it received.
On the `reply_image` fall-through to text, `meta.action_type` is `reply_text`
(the degrade is visible up front), so the client never expects an image.

---

## 9. Persistence + write-back (no migration)

Everything rides on `engine.chat_messages.metadata` (JSONB).

**At generation** (assistant row): `content` = the text reply
(`reply_text_image`) or empty (`reply_image`); seed:

```json
{
  "image": {
    "prompt": "<subject>",
    "style": "realistic",
    "model": "<image model served>",
    "aspect_ratio": "3:4",
    "resolution": "1024x1365",
    "generation_id": "<gen id>",
    "face_ref_used": true
  }
}
```

**The bytes are never persisted.** They go out on the `Image` frame only.

**Write-back endpoint** — the client uploads the data-URL to its own storage and
returns the URL:

```text
POST /comp/chat/{session_id}/message/{message_id}/image
body: { "url": "https://cdn.example/gen/abc.png" }
```

- `set_assistant_image_url(message_id, &url)` (new store method,
  `eros-engine-store/src/chat.rs`) merges `metadata.image.url = $url` on the
  assistant row (top-level JSONB merge, mirrors `set_user_image_vision`).
- `url` validated like `image_url` (absolute http/https, ≤2048). Idempotent
  (re-POST overwrites the same key). Returns `404` if the message_id isn't an
  assistant row in this session, `422` on a bad URL.

---

## 10. Future-turn memory

Assistant image rows fold a text marker into the model-facing history
(symmetric to `model_facing_user_text`), via an assistant-side helper in
`pipeline/handlers.rs`:

```text
reply_text_image:  <reply text>\n\n[你给对方发送了一张照片：<image prompt>]
reply_image:       [你给对方发送了一张照片：<image prompt>]
```

So the main chat model "remembers" it sent a photo on later turns. The
written-back `metadata.image.url` enables later image2image continuity (re-using
a past photo as a reference) — not required for v1 memory.

---

## 11. Affinity evaluation on image turns (revised — no longer skipped)

Today `eval_skip_reason` returns `Some("image_reply")` for both image actions
(`post_process.rs:498`), skipping affinity eval. **This is removed** — image
turns are evaluated:

- **`ReplyTextImage`**: routed through the same gate as `ReplyText`
  (`user_msg_chars >= AFFINITY_EVAL_MIN_CHARS` and `!assistant_empty`),
  evaluating on the **assistant text** as normal.
- **`ReplyImage`** (no text): the caller supplies the **`image_prompt` as the
  assistant-content proxy** for the evaluator, and computes `assistant_empty`
  from it. So "user said X, companion responded by sending a photo of Y" still
  moves affinity. A blank `image_prompt` ⇒ `empty_assistant` skip (as for an
  empty text reply).

Implementation: `eval_skip_reason` drops the `ReplyImage | ReplyTextImage` arm
and lets both fall through the `ReplyText` arm; the **call site** passes the
right "assistant content" (text for text-bearing actions, `image_prompt` for
`reply_image`) and the matching `assistant_empty`.

Consequences:
- The OpenRouter **audit trio** (`model`/`usage`/`generation_id`) on the
  affinity event is now populated from the `affinity_evaluation` call on image
  turns (no `image_reply` NULL-trio marker — cross-ref
  `project_affinity_audit_trio_nulls`).
- One extra `affinity_evaluation` LLM call per evaluated image turn (the
  explicit cost trade chosen for this feature).

---

## 12. Failure & idempotency (fail-open)

- **Image-gen failure** → §7 fail-open (text delivered / fall through to text).
  The stream never hard-fails on image-gen.
- **Run-once**: keyed on the upsert idempotency gate exactly like the rest of
  `run_stream` — `run_stream` only runs on a fresh Insert; a `Replay`
  short-circuits before this arm, so the image call is paid once per turn. A
  resumed/retried in-progress turn that already has `metadata.image` skips the
  re-call.
- **Write-back** is a separate, idempotent endpoint; losing it only loses the
  durable URL (the client already has the image), not the reply.

---

## 13. Testing

- `model_config`: `resolve_image_gen` ⇒ `None` when task absent, `Some` when
  present (even with no `model`); honors `ModelSpec` forms when `model` is set.
- `effective_image_chain`: candidate order per-turn → config `ModelSpec` →
  config `fallback`; head = primary, tail = chain; `fallback`-only (no `model`,
  no per-turn) ⇒ its head is the primary; `"X"` per-turn + `["X","Y"]` fallback
  ⇒ `(X, [Y])` (dedup keep-first); all empty ⇒ `None`.
- `compose_image_prompt`: style preset + appearance(present/absent) + subject;
  subject `??` chain (PDE / forced / user-msg fallback).
- `execute_image` body builder (pure): `modalities` present; image2image block
  added iff `face_ref_url`; aspect/resolution folded; image parsed from
  `message.images[]`; zero-image success treated as a chain miss.
- `execute_image` chain walk: unavailable primary (error / zero-image) walks to
  the fallback; chain exhausted with no image → caller fail-open (§7). No
  model-id pre-validation (a bogus id is just an attempt that fails).
- `validate_payload`: `face_ref_url` shape; `aspect_ratio`/`resolution` allowed
  sets; `force`+tip → 422; ImageOnly allows empty content.
- `guard_action`: image actions kept iff `image_executor_available` (task block
  present AND effective model resolved), else degrade to text. Cases: task
  absent → degrade; task present, config model + no per-turn → keep; task
  present, no config model + per-turn model → keep; task present, no model
  anywhere → degrade; forced + no model → force dropped, degrade.
- Decision: `ActionPlan.image_prompt` propagates from the PDE verdict; forced
  path builds the right action + subject.
- SSE: `reply_text_image` = meta+delta+image+done+final; `reply_image` =
  meta+image+done+final; fail-open paths (text only / fall-through).
- Persistence: `metadata.image` seeded at gen; `set_assistant_image_url` merges
  `url`, leaves other keys intact; 404/422 cases.
- History fold: assistant image rows render the `[发送了一张照片：…]` marker.
- Affinity: `reply_text_image` evals on text; `reply_image` evals on
  `image_prompt`; blank prompt ⇒ `empty_assistant`; audit trio populated.

---

## 14. Files touched / PR checklist

- `crates/eros-engine-server/src/routes/companion_stream.rs` — `image` field,
  `ImageReplyParams`/`ImageMode`/`StyleKey`, validation, write-back route +
  handler.
- `crates/eros-engine-core/src/types.rs` — `ActionPlan.image_prompt`.
- `crates/eros-engine-llm/src/model_config.rs` — `ResolvedImageGen`,
  `resolve_image_gen`, style preset constants.
- `crates/eros-engine-llm/src/openrouter.rs` — `ImageGenRequest`/
  `ImageGenResponse`, `execute_image`, body builder, usage logging.
- `crates/eros-engine-server/src/pipeline/stream.rs` — conditional degradation
  in `guard_action`, forced-path plan, execution arm, `FrameActionType` +
  `Image` frame.
- `crates/eros-engine-server/src/pipeline/handlers.rs` — `compose_image_prompt`,
  subject resolution, assistant-side history fold.
- `crates/eros-engine-server/src/pipeline/post_process.rs` — drop the
  `image_reply` skip; image-turn assistant-content proxy.
- `crates/eros-engine-store/src/chat.rs` — `set_assistant_image_url`; seed
  `metadata.image` at assistant upsert.
- `examples/model_config.toml(.example)` — `[tasks.chat_image_generation]`.
- `examples/personas/aria.toml` — optional `appearance` key.
- `docs/api-reference.md(.zh)`, `docs/model-config.md(.zh)` — document the
  `image` param, the new task, the write-back endpoint; update the
  "image variants degrade to reply_text" note now that the executor can ship.
- Regenerate `openapi.json`.
- **No SQL migration.**
- PR gate: `cargo fmt` / `clippy` / `test` / openapi check (per repo release flow).

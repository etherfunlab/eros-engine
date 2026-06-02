# eros-engine — Chat image input via vision describe (`chat_vision`) (Spec)

**Status**: design, pending implementation plan
**Target release**: `0.5.x` dev track (`0.5.3-dev`). **No migration** — image URL and
the extracted description ride on the existing `engine.chat_messages.metadata`
JSONB column (added in migration `0019`). No new columns.
**Audience**: anyone implementing image input on `POST /comp/chat/{session_id}/message/stream`.
Mirrors the `chat_input_filter` pre-stage (`2026-06-02-chat-input-filter-design.md`)
on the image side.

---

## 0. Background

### The feature

`chat/stream` today accepts a single `content: String` user turn and is fully
text-only end to end — `openrouter::ChatMessage { role, content: String }` has no
content blocks, no `image_url`. We want the user to be able to send an **image**
to their companion.

We do **not** send the image to the main chat model directly. Instead, a
**two-step** flow:

1. The image is analysed by a **vision model** (its own OpenRouter call), which
   returns a **fixed-schema JSON** description of the image.
2. That description is **folded into the user message text** the main chat model
   (`tasks.chat_companion`) sees. The main chat model stays **text-only** — it
   never receives image bytes or `image_url`.

This keeps the main-chat invariant ("the companion model is driven by plain
text") intact, gives downstream a structured, auditable description of every
image, and lets the vision model be swapped/configured independently of the chat
model.

### Why this mirrors `chat_input_filter`

`chat_input_filter` (#70) is the direct precedent. It is an **input-augmentation
pre-stage** that runs inside `run_stream`, after the idempotency gate and before
`build_reply_request`: it issues a secondary OpenRouter call, parses a JSON
verdict, and changes the text the main model sees — while the **persisted,
client-visible `content` stays the user's original**.

The vision stage is the same shape, with these deliberate differences:

| | `chat_input_filter` | `chat_vision` |
|---|---|---|
| Trigger | per-turn **probability** coin flip | **deterministic** — fires iff `image_url` present |
| Input | typed caption + recent transcript | the image URL (+ optional caption) |
| Secondary call | text-only `execute()` | **multimodal** `execute_vision()` |
| Output | `{rewrite, content, reason}` verdict | fixed `{description, ocr_text, people, scene}` |
| Where stored | `pre_filter_content` (replaces text) | `metadata.vision` (augments, never touches `pre_filter_content`) |
| On failure | fail-open: keep original | fail-open: inject a neutral placeholder |

The two stages are **orthogonal and both may run on the same turn**: vision
describes the image, the input filter still only rewrites the typed caption. They
do not write the same column.

### eros-engine scope boundary

eros-engine is OSS and does not own storage or deploy. The client uploads the
image to **its own** storage and sends eros-engine an **`https` URL**. The engine
never receives or stores image bytes — it forwards the URL to the vision model
and persists the URL + extracted description only.

---

## 1. Goals / non-goals

**Goals**
- Optional, off-by-default image input on `chat/stream`, configured via a new
  `[tasks.chat_vision]` task — absent task ⇒ feature off ⇒ existing text-only
  behaviour byte-for-byte unchanged (backward compatible).
- A single optional `image_url: Option<String>` field on `StreamSendRequest`
  (one image per turn).
- The image is described by a vision model into a fixed JSON schema
  `{description, ocr_text, people, scene}`, which is folded into the
  model-facing user text for both the current turn and history.
- The persisted, client-visible `content` always stays the user's original text
  (empty when the user sent an image with no caption).
- Fail-open: any vision failure degrades gracefully; the turn always produces a
  reply.
- The image description survives into later turns' history, so the companion
  "remembers" the photo.

**Non-goals (explicitly out of scope — YAGNI)**
- Multiple images per turn (single image only).
- A `safety`/NSFW field in the schema.
- Native multimodal pass-through to the **main** chat model.
- Engine-side image upload / storage / byte handling.
- A dedicated client-visible "analysing image" protocol frame.
- Feeding the image description into the `chat_input_filter` transcript (the
  input filter keeps seeing only typed captions for v1).

---

## 2. API surface — `StreamSendRequest`

Add one optional field (`crates/eros-engine-server/src/routes/companion_stream.rs`,
`StreamSendRequest` @ ~L72):

```rust
#[serde(default)]
pub image_url: Option<String>,
```

**Validation** (extend `validate_payload` @ ~L101):

- When `image_url` is `Some`:
  - must parse as an absolute `http`/`https` URL,
  - length ≤ `MAX_IMAGE_URL_LEN` (2048), else `422 unprocessable`
    (`user_message: "图片链接无效"`).
- **Empty-content rule** (existing): `content` may be empty only when a tip is
  attached. Extend so `content` may also be empty when `image_url` is present:
  ```
  if req.content.is_empty() && req.tips_amount_usd.is_none() && req.image_url.is_none() {
      → 422 "请输入一条消息"
  }
  ```
- `MAX_CONTENT_CHARS` (4096) still applies to the caption.

The `image_url` is stored on the user row at upsert time (see §7); it is **not**
otherwise echoed in protocol frames.

OpenAPI: regenerate `openapi.json` after the DTO change (PR gate).

---

## 3. Vision sub-call — multimodal path in `openrouter.rs`

The two-step flow still requires the **vision call itself** to be multimodal. The
main `ChatMessage { content: String }` stays unchanged; add a **dedicated
vision-only** entry point used by nobody but the `chat_vision` stage.

New in `crates/eros-engine-llm/src/openrouter.rs`:

```rust
/// One-shot multimodal describe call. Builds an OpenRouter user message whose
/// `content` is an array of blocks: a text instruction + one image_url block.
/// Returns the model's text reply (expected to be JSON; parsing is the caller's
/// job). Non-streaming.
pub async fn execute_vision(&self, req: VisionRequest) -> Result<ChatResponse, LlmError>;

pub struct VisionRequest {
    pub model: String,
    pub fallback_model: Vec<String>,   // sequential chain, like ChatRequest
    pub system_prompt: String,         // the describe instruction
    pub image_url: String,
    pub caption: Option<String>,       // user's own caption, if any (helps grounding)
    pub temperature: f32,
    pub max_tokens: u32,
    pub reasoning: Option<ReasoningConfig>,
}
```

Wire body for the single call (per chain entry):

```json
{
  "model": "<model>",
  "messages": [
    { "role": "system", "content": "<system_prompt>" },
    { "role": "user", "content": [
        { "type": "text", "text": "<caption or a default 描述这张图片 instruction>" },
        { "type": "image_url", "image_url": { "url": "<image_url>" } }
    ]}
  ],
  "temperature": <t>, "max_tokens": <n>, "reasoning": <opt>
}
```

- The fallback chain is walked on **transport-level** failures (error / timeout /
  empty reply), identical to `execute()`’s chain walk. A content-level
  non-success (unparseable JSON) is handled by the caller in §6.
- Reuse the existing app-attribution headers and `ChatResponse`
  (`reply`, `generation_id`, `model`, `finish_reason`).
- Log usage via `super::log_openrouter_usage("chat_vision", None, &resp)` so the
  vision call shows up in OpenRouter audit alongside the other non-chat tasks.

> Implementation note: keep `ChatMessage` text-only. The block-array body lives
> in a private wire struct local to `execute_vision`, so the public text-only
> contract of `ChatMessage`/`ChatRequest` is untouched.

---

## 4. Fixed schema (engine-defined, parsed & validated)

In the pipeline (`pipeline/stream.rs`, next to the input-filter helpers):

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
struct ImageVision {
    description: String,            // required: 画面 / 主体 / 在发生什么
    #[serde(default)]
    ocr_text: Option<String>,      // 图中文字; None/blank ⇒ omitted
    #[serde(default)]
    people: Option<String>,        // 是否有人物 + 表情 / 情绪
    #[serde(default)]
    scene: Option<String>,         // 场景 / 地点 / 氛围
}
```

**Parsing** reuses the input-filter two-pass approach:
`serde_json::from_str` first, then `post_process::find_json_block` on a JSON
block embedded in prose.

**Validity gate**: a parse is *valid* only when `description` is non-blank
(after trim). `ocr_text` / `people` / `scene` are optional; blank ⇒ treated as
absent and dropped from the injected preamble. A `content_filter` finish reason
or a refusal-shaped reply ⇒ invalid (mirror `rewrite_content_invalidity`).

---

## 5. Config — `[tasks.chat_vision]`

New task block, same machinery as `chat_input_filter` / `chat_output_filter`
(`crates/eros-engine-llm/src/model_config.rs`):

```toml
[tasks.chat_vision]
model = "..."                 # a vision-capable model id on OpenRouter
fallback_model = ["..."]      # optional; truncated to retry_depth
temperature = 0.2
max_tokens = 400
retry_depth = 1
filter_prompt = """
你是图像识别助手。只输出 JSON，字段：
{ "description": "...", "ocr_text": "...", "people": "...", "scene": "..." }
没有文字/人物/场景信息时对应字段留空字符串。不要输出 JSON 以外的任何内容。
"""
```

Add:

```rust
pub struct ResolvedVision {
    pub model: String,
    pub fallback_model: Vec<String>,   // truncated to retry_depth
    pub temperature: f64,
    pub max_tokens: u32,
    pub describe_prompt: String,       // from filter_prompt
    pub retry_depth: u32,
    pub reasoning: Option<ReasoningConfig>,
}

/// `None` (feature off) when `[tasks.chat_vision]` is absent OR its resolved
/// `filter_prompt` is blank. No `chat_companion` toggle and no probability —
/// presence of the task block is the switch; the per-turn trigger is "image_url
/// present" (§6), decided in the wiring, not here.
pub fn resolve_vision(&self) -> Option<ResolvedVision>;
```

Mirror `resolve_input_filter` (read task, blank-prompt guard, truncate fallback
to `retry_depth`, default `retry_depth = 1`). Unlike the input filter there is
**no** `[tasks.chat_companion]` probability gate — image presence is the gate.

`describe_prompt` reads from the **existing `TaskConfig.filter_prompt` field**
(`model_config.rs:390`) — the same generic key `chat_output_filter` /
`chat_input_filter` already use. **No `TaskConfig` struct change**: do not add a
new prompt field for `chat_vision`.

Also add a `chat_vision` example block to `examples/model_config.toml`.

---

## 6. Pipeline pre-stage (`run_stream`, Reply branch)

Insertion point: `pipeline/stream.rs`, in the `ActionType::Reply | GiftReaction`
arm, **after** the idempotency gate, **adjacent to** the input-filter block
(@ ~L1195–1241). Run vision **before** the input filter.

```text
if Reply turn AND user_msg has image_url AND not already described:
    f = state.model_config.resolve_vision()?            // None ⇒ skip (feature off)
    resp = run_vision(state, &f, &image_url, caption)   // walks fallback chain
    if Some(ImageVision) parsed & valid:
        chat_repo.set_user_image_vision(user_message_id, &vision_json,
                                        &vision_model, generation_id)   // §7
    // else: persist nothing → §9 placeholder path covers it
```

- **Trigger**: fires iff the user row carries `image_url` (not gift, not tip-only).
  No probability.
- **`run_vision`** mirrors `run_input_filter`: walk `[primary, ...fallback]`,
  `tokio::time::timeout(FILTER_TIMEOUT, execute_vision(...))`, `continue` on
  transport failure, parse+validate per §4. Returns `Some(ImageVision + audit)`
  only on a valid parse; otherwise `None`.
- **Idempotency / run-once**: skip the call when the user row already has
  `metadata.vision` (a resumed/retried in-progress turn) — the vision call is
  paid once per image. Replay outcomes never reach this branch at all.

---

## 7. Persistence / data model (no migration)

Everything rides on `engine.chat_messages.metadata` (JSONB).

**At upsert** (`upsert_user_message_idempotent`, `chat.rs` @ ~L451 — already
takes `metadata: Option<&serde_json::Value>`): when the request has `image_url`,
seed the user row’s metadata with it:

```json
{ "image_url": "https://..." }   // merged with any existing tip metadata
```

**After a successful describe**, a new store method merges the result in
(top-level JSONB merge via `metadata || $2`, leaving `image_url` and any tip keys
intact):

```rust
/// chat.rs — merge the vision result into the user row's metadata. Does NOT
/// touch content or pre_filter_content.
pub async fn set_user_image_vision(
    &self,
    user_message_id: Uuid,
    vision: &serde_json::Value,     // {description, ocr_text, people, scene}
    vision_model: &str,
    generation_id: Option<&str>,
) -> Result<(), StoreError>;
// UPDATE engine.chat_messages
//   SET metadata = metadata || jsonb_build_object(
//       'vision', $2, 'vision_model', $3, 'vision_generation_id', $4)
// WHERE id = $1 AND role = 'user';
```

Resulting user-row metadata after a described image turn:

```json
{
  "image_url": "https://...",
  "vision": { "description": "...", "ocr_text": "...", "people": "...", "scene": "..." },
  "vision_model": "...",
  "vision_generation_id": "..."
}
```

`content` and `pre_filter_content` are untouched by this stage.

---

## 8. Injection fold — model-facing text

Introduce a shared helper next to `effective_user_text`
(`pipeline/handlers.rs` @ ~L82) that layers the image preamble **on top of** the
input-filter rewrite:

```rust
/// What the MAIN chat model should see for a user row: optional image preamble
/// (built from metadata.vision, or a placeholder when an image was sent but not
/// described) followed by effective_user_text(msg).
pub(crate) fn model_facing_user_text(msg: &ChatMessage) -> String;
```

Logic:
1. `base = effective_user_text(msg)` (handles input-filter `pre_filter_content`).
2. If `metadata.vision` present → build the preamble from its non-blank fields.
3. Else if `metadata.image_url` present but no `vision` → placeholder preamble
   `[用户发送了一张图片，但内容无法识别]`.
4. Else → return `base` unchanged.
5. Return `preamble + "\n\n" + (base if non-empty else "[用户未附文字]")`.

Preamble template (engine-owned constant; blank lines omitted):

```
[用户发送了一张图片]
画面：{description}
文字：{ocr_text}     ← omit line if blank
人物：{people}       ← omit line if blank
场景：{scene}        ← omit line if blank
```

**Call sites to switch to `model_facing_user_text`** (so past image turns are
remembered by the main model):
- `build_reply_request` — current user turn + its history rendering of `user`
  rows.
- Any other history/transcript builder that feeds the **main** chat model and
  currently calls `effective_user_text` on `user` rows.

**Left unchanged** (keep plain `effective_user_text`):
- `build_input_filter_transcript` — the input filter sees typed captions only
  for v1 (non-goal §1).

Edge case: a non-image text turn has neither `vision` nor `image_url` in
metadata → `model_facing_user_text` returns `base`, i.e. identical to today.

---

## 9. Failure & idempotency (fail-open)

- **Vision chain exhausted / unparseable / invalid** → nothing written to
  `metadata.vision`. At assembly, `model_facing_user_text` sees `image_url`
  without `vision` and injects the neutral placeholder (§8 step 3). The main
  model still replies; a present caption carries the turn. The stream **never
  hard-fails** because of vision.
- **Run-once**: keyed on `metadata.vision` already present → skip re-running on a
  retried/resumed turn. Replay (`UpsertUserOutcome::Replay`) short-circuits
  before the Reply branch, so a completed turn never re-pays for vision.
- **Latency**: an image turn adds one synchronous vision round-trip before the
  main burst starts (same cost profile as an input-filter turn). Acceptable; not
  a hot loop.

---

## 10. Testing

- `model_config`: `resolve_vision` returns `None` when task absent / prompt
  blank; returns truncated fallback chain at `retry_depth`.
- Schema parse: direct JSON, embedded JSON block, blank `description` rejected,
  optional fields dropped when blank, refusal/`content_filter` rejected.
- `validate_payload`: `image_url` required-shape (http/https, length), empty
  `content` allowed with image, rejected without image/tip.
- `set_user_image_vision`: merges `vision`/`vision_model`/`vision_generation_id`
  into metadata, leaves `content` / `pre_filter_content` / `image_url` intact.
- `model_facing_user_text`: described image → full preamble + caption; image,
  no caption → preamble + `[用户未附文字]`; image, no vision → placeholder;
  plain text turn → unchanged.
- Wiring: vision skipped when no `image_url`; skipped when `metadata.vision`
  already present; orthogonal to input filter (both may fire).

---

## 11. Files touched / PR checklist

- `crates/eros-engine-server/src/routes/companion_stream.rs` — `image_url` field
  + validation, store on upsert.
- `crates/eros-engine-llm/src/openrouter.rs` — `VisionRequest` + `execute_vision`.
- `crates/eros-engine-llm/src/model_config.rs` — `ResolvedVision` + `resolve_vision`.
- `crates/eros-engine-server/src/pipeline/stream.rs` — `ImageVision`, `run_vision`,
  pre-stage wiring.
- `crates/eros-engine-server/src/pipeline/handlers.rs` — `model_facing_user_text`;
  swap main-model call sites.
- `crates/eros-engine-store/src/chat.rs` — `set_user_image_vision`; seed
  `image_url` on upsert metadata.
- `examples/model_config.toml` — `[tasks.chat_vision]` example.
- Regenerate `openapi.json`.
- **No SQL migration.**
- PR gate: `cargo fmt` / `clippy` / `test` / openapi check (per repo release flow).
```

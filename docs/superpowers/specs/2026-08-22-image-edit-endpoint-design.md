# `POST /v2/comp/session/{session_id}/message/{message_id}/image/edit` — Design

- **Date:** 2026-08-22
- **Status:** Approved, not yet implemented
- **Type:** One new v2 action endpoint, one new config task, one CHECK widening
  on an audit table
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` 1.6.x, one PR to `dev`
- **Convention:** [2026-08-22-user-insights-and-api-v2-design.md](2026-08-22-user-insights-and-api-v2-design.md) §4
- **Independent of:** [2026-08-22-affinity-event-user-message-id-design.md](2026-08-22-affinity-event-user-message-id-design.md);
  the two take migration numbers in merge order.

## 1. What this is

A consumer holds a picture the character sent (an assistant message with
`metadata.image`) and wants a variation of it: "换套衣服", "换个角度",
"晚上的版本". Today there is no way to ask the engine for that — the standalone
composer (`POST /persona/{instance_id}/image/compose`) knows nothing about the
source picture, and the chat turn's `image.force` flag composes from the
conversation, not from an instruction.

This endpoint takes an edit instruction against one source image turn, runs the
composer with the source's subject as context, and **forces a `reply_image`
turn**: a new image-only assistant message is persisted and its `image_request`
payload is returned synchronously. The engine composes; the consumer draws —
the delegation contract is unchanged.

It is the thing the removed engine draw endpoint (`POST
/comp/chat/{session_id}/image/stream`, gone in #203) was reaching for, brought
back in the delegate-only shape.

## 2. Contract

### 2.1 Path

```
POST /v2/comp/session/{session_id}/message/{message_id}/image/edit
```

Per the v2 grammar: `session/{session_id}` then `message/{message_id}` are
entity segments sharing a stem with their parameter; `image` is the resource;
`edit` is the verb leaf. `chat` does not appear. Both ids are `chat_*` UUIDs,
the same type the v1 recovery endpoint takes — `{message_id}` is the value
`ChatHistoryEntry.id` / `BffHistoryEntry.id` carries.

`session_id` stays in the path even though a message id is globally unique:
ownership is settled on the session (`require_session_for_user`), the message
is then loaded *inside* that session (`message_by_id_in_session`), and that is
the same two-step every session-keyed route performs. A message-only path would
need a new ownership resolver for one endpoint.

### 2.2 Request

```rust
#[derive(Deserialize, ToSchema)]
pub struct ImageEditRequest {
    /// The edit, in the user's words. Required, non-empty after trim.
    pub instruction: String,
    /// Same presets as the chat path. Default `realistic`. Pass the style the
    /// source was drawn with — the engine does not record it on the message.
    #[serde(default)] pub style: Option<StyleKey>,
    /// Same allow-list as the chat path. Default: the source marker's
    /// `aspect_ratio`; absent there too ⇒ unspecified, as on the chat path.
    #[serde(default)] pub aspect_ratio: Option<String>,
    /// `[tasks.chat_image_edit_compose].filter_prompt` variant, same selection
    /// rules as `image.prompt_variant` on the chat path.
    #[serde(default)] pub prompt_variant: Option<String>,
}
```

`instruction`, not `prompt`: in this codebase `prompt` is the composer's output
subject (`metadata.image.prompt`) and the wire string is `composed_prompt`. A
third meaning on the request body would make the docs unreadable.

### 2.3 Status ladder, in evaluation order

| Status | Condition | Body |
|---|---|---|
| 401 | missing / invalid bearer | — |
| 404 | session unknown or archived | `session not found` |
| 403 | session not owned by the JWT user | `not your session` |
| 404 | no such message in this session | `no such message` |
| **409** | message exists but has no `metadata.image` | `not an image turn` |
| 422 | `instruction` blank, `aspect_ratio` off the allow-list | validation message |
| 501 | no composer configured (§3.1) | `image prompt composer not configured` |
| 502 | composer chain exhausted | `image prompt composer failed` |
| 200 | — | `ImageEditResponse` |

**409, not 404, for "not an image turn".** The message *was* found; what is
wrong is its state relative to the action, which is what Conflict means. The
v1 recovery endpoint answers 404 for the same condition, and that stays frozen
— v1 is a read, where "the thing you asked for does not exist" is defensible;
this is an action on a resource that does exist.

Nothing is persisted on any non-200 path except the `exhausted` audit row on
502 (§3.4). A 502 leaves no assistant message: there is **no portrait
fallback** here. The chat path falls back to a plain portrait because a turn
that promised a picture must ship one; an edit that ignores its instruction is
worse than an error, and the consumer can simply retry.

### 2.4 Response

```rust
#[derive(Serialize, ToSchema)]
pub struct ImageEditResponse {
    /// The new assistant message (`chat_messages.id`).
    pub message_id: Uuid,
    /// The source image turn this is an edit of.
    pub edit_of: Uuid,
    /// base64(STANDARD) of the UTF-8 wire prompt — the same encoding as the
    /// `image_request` frame and the recovery endpoint, so a consumer feeds it
    /// to the draw path it already has.
    pub composed_prompt: String,
    /// Always `"previous"` on an edit turn; see §3.3.
    pub image_ref: String,
    pub aspect_ratio: Option<String>,
    /// The composer's caption for the new picture; `null` when it gave none.
    pub caption: Option<String>,
}
```

A new struct. `ImageRequestPayloadResponse` is the v1 recovery endpoint's type
and stays exactly as shipped; `ComposeResponse` is the standalone composer's
and carries no message id.

## 3. Behaviour

### 3.1 Config — `[tasks.chat_image_edit_compose]`

A new task block, the same shape as `chat_image_prompt_compose`: `model`,
`fallback`, `retry_depth`, `temperature`, `max_tokens`, `reasoning`, sampling
knobs, and a `filter_prompt` accepting the same three shapes (plain / indexed /
keyed) selected by `prompt_variant`. The built-in default is a new
`DEFAULT_EDIT_PROMPT` constant next to `DEFAULT_COMPOSE_PROMPT`.

Resolution, `resolve_image_edit_compose(variant) -> Option<ResolvedImagePromptCompose>`
(the existing resolved type; nothing in it is compose-specific):

| `chat_image_edit_compose` | `chat_image_prompt_compose` | Result |
|---|---|---|
| present | any | the edit block's chain and params; its `filter_prompt` if set, else `DEFAULT_EDIT_PROMPT` |
| absent | present | the **compose** block's chain and params, `compose_prompt = DEFAULT_EDIT_PROMPT`, `variant_key = None` |
| absent | absent | `None` → 501 |

The compose block is the gate — an operator who enabled image turns gets edits
with no further config, on the same models, with the engine prompt. The edit
block exists so the prompt and the model can be tuned separately once there is
a reading to tune against (the `image_edit` audit rows, §3.4).

Housekeeping that comes with a new task: `KNOWN_CHAT_TASKS` gains
`chat_image_edit_compose`; the wire `task` on the composer call is
`chat_image_edit_compose`, so `[[providers.*.body]]` rules and
`log_openrouter_usage` distinguish it from chat composes; the
model-config validator applies the same `filter_prompt` variant checks as the
compose block. `examples/model_config.toml` ships the block commented out,
directly under the compose block, with the fallback rule in its comment.

`DEFAULT_EDIT_PROMPT` says: you are given the character's appearance, the
picture they previously sent (its subject and caption), and the partner's
requested change; return the same two-field JSON — `prompt` describing the
**new** picture, preserving everything about the original the change does not
touch, applying the change fully and without softening; `caption` one short
in-language line. The no-sanitising clause is copied from the compose prompt
verbatim — content policy is the provider's and the consumer's, not this step's.

### 3.2 The composer call

The chain walk in `run_image_prompt_compose` is reused; the function is
refactored to take the rendered user payload and the wire task name instead of
rendering the five chat slots itself (the chat path passes what it renders
today — no behaviour change there). The edit payload is a new pure renderer:

```
[人物外观]
{persona meta "appearance"}

[原图]
{source metadata.image.prompt}
{source metadata.image.caption, when present}

[修改要求]
{instruction}

[风格]
{style}

[画幅]
{aspect_ratio or （未指定）}
```

The source's **subject** feeds the composer, not its stored wire prompt:
`chat_images_events.composed_prompt` already contains the style preset and the
appearance, both of which the payload carries in their own slots. An empty
subject (a source drawn through the portrait fallback) renders `（无）` and the
composer works from appearance plus instruction — still a valid edit.

Output parsing (`parse_compose_reply`) and assembly
(`compose_image_prompt(style, persona, subject)`) are the existing functions.

### 3.3 Persistence

In this order, on the success path:

1. **Audit row** in `engine.chat_images_events`, `source = 'image_edit'`,
   `session_id` / `instance_id` / `user_id` set, `inputs =
   {appearance, source_subject, source_caption, instruction, style,
   aspect_ratio, source_message_id}`. The row is written the moment the
   composer returns, via the existing `record_compose_event`, and its id is
   stamped on the marker below — the same direction the chat path uses.
2. **Assistant row** via `ChatRepo::insert_assistant_batch` (which also bumps
   the session's `last_active_at`):
   - `id` — a fresh ULID cast to UUID, like every streamed assistant row.
   - `content = ""`, `assistant_action_type = 'reply'` — identical to the chat
     path's image-only row.
   - **`user_message_id` = the source message's `user_message_id`.** The edit
     belongs to the turn the source picture answered; there is no new user
     message. `NULL` if the source has none.
   - `metadata.image` — the usual marker from `build_delegated_image_marker`
     (subject, caption, aspect, compose trio, `compose_event_id`,
     `image_ref`), plus one new key **`edit_of`** = the source message id.
     The builder gains an `edit_of: Option<Uuid>` parameter, written only when
     present; chat-path markers are byte-identical to today.
   - `image_ref = "previous"`. On an edit turn "previous" means the `edit_of`
     image, not "whatever the consumer drew last" — the consumer holds both
     ids in the response. No new `ImageRef` variant: that enum is on the wire
     in `image_request` frames and a new value would break every consumer's
     match.

Nothing else runs. No PDE verdict, no affinity event, no insight or memory
extraction, no ghost-streak change, no queue claim. The new row is not a
`ProducedMessage`; it is a picture, not a reply to anything new.

Because it is an ordinary assistant row with `metadata.image`, everything that
reads such rows works on it for free: history shows `image: true`, the v1
recovery endpoint returns its `composed_prompt`, and its caption enters the
transcript so the character later "remembers" having sent the edited picture.

### 3.4 Audit

Migration `0057_chat_images_events_image_edit.sql` widens the `source` CHECK
to add `'image_edit'`. `inputs` for that source carries the seven keys in
§3.3, not the chat composer's five — `inputs` is free-form JSONB and the
`source` column says which shape to expect. `docs/llm-audit.md` documents both.

A 502 writes one `exhausted` row with `attempts` / `last_failure` / the failure
list, `composed_prompt = NULL` — the same as the standalone composer's 502.

`chat_images_events WHERE source = 'image_edit'` grouped by `status` and by day
is the reading for this feature: how often edits are requested, how often the
composer refuses or fails, and on which models.

### 3.5 Concurrency and cost

- A chat turn may be in flight on the session while an edit lands. The edit
  row interleaves wherever its `created_at` falls. Accepted — the voice and
  chat pipelines already write to one session concurrently, and the edit row
  carries no state the in-flight turn reads.
- One composer call per request, no per-user cap. Every call is audited
  (§3.4). Default-open; the gate gets ratcheted if the reading says so.

## 4. Implementation shape

- `crates/eros-engine-server/src/routes/image_edit.rs` — DTOs, handler,
  `router()`; merged (not nested) into both `router()` and
  `router_for_openapi()` in `routes/mod.rs`, like `insight.rs`.
- `pipeline/stream.rs` — `run_image_prompt_compose` takes `(payload, task)`;
  new `compose_edit_payload(...)` and `compose_edit_inputs_json(...)` pure
  functions; `build_delegated_image_marker` gains `edit_of`.
- `crates/eros-engine-llm/src/model_config.rs` — `DEFAULT_EDIT_PROMPT`,
  `resolve_image_edit_compose`, `KNOWN_CHAT_TASKS` entry, validator case.
- `crates/eros-engine-store/migrations/0057_…` — CHECK widening.
- `examples/model_config.toml` — the commented block.
- `docs/api-reference.md` + `.zh.md` — new v2 section after the async-turn
  entry, with the 409 rule and the `image_ref = previous` meaning;
  `docs/llm-audit.md` + `.zh.md` — the new `source` and `inputs` shape.
- `openapi.json` regenerated.

## 5. Not in scope

- Image-to-image on the engine side, reference-image URLs, or any drawing. The
  consumer draws; it already holds the source image it chose to edit.
- An SSE mode. One composer call, one JSON response; the standalone composer's
  streaming mode exists for long-form prompts on a path with no message to
  persist, which is not this.
- Editing a message that is not an image turn (409), or "edit the latest
  picture" without a message id.
- Recording the edit instruction as a user message. The instruction is an
  input to a picture, not a thing the user said to the character; it is kept
  on the audit row's `inputs`.
- A new `ImageRef` variant (§3.3).
- Any change to `/persona/{instance_id}/image/compose` or to the v1 recovery
  endpoint.

## 6. Testing

**Pure:**

- `compose_edit_payload` renders all five slots, the caption line only when
  present, `（无）` for an empty subject.
- `resolve_image_edit_compose`: the edit block's model when present; the
  **compose block's model specifically** with `DEFAULT_EDIT_PROMPT` when
  absent; `None` when both are absent. An unknown variant key falls back to
  the built-in prompt.
- `DEFAULT_EDIT_PROMPT` is non-empty and names both output fields (guards a
  broken-constant edit).
- The shipped example config parses with the new task present.

**Server (`sqlx::test`, mocked OpenRouter, mirroring the recovery-endpoint
tests):**

- The full ladder: 403 foreign session, 404 unknown session, 404 unknown
  message, **409** on a text message, 422 blank instruction, 501 with no
  composer task, 502 on chain exhaustion.
- 502 writes exactly one `image_edit` / `exhausted` audit row and **no**
  assistant row.
- Success: response fields; a new assistant row exists with `content = ""`,
  `user_message_id` equal to the source's, `metadata.image.edit_of` equal to
  the source id, `image_ref = "previous"`, and a `compose_event_id` that
  resolves to an `image_edit` / `ok` audit row whose `inputs.instruction` is
  the request's; the wire `task` on the mocked call is
  `chat_image_edit_compose`.
- Follow-through: `GET /comp/chat/{sid}/history` flags the new row
  `image: true`, and the v1 recovery endpoint returns the same
  `composed_prompt` for the new `message_id`.
- Editing an edit: a second call with the first result's `message_id` as the
  source succeeds and points `edit_of` at it.

**Pre-PR gate:** `cargo fmt --check`, `cargo clippy`, `cargo test`, OpenAPI
regeneration check.

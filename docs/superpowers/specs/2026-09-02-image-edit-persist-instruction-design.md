# Image edit `persist_instruction` — Design

**Date:** 2026-09-02
**Status:** approved
**Extends:** `2026-08-22-image-edit-endpoint-design.md` (v1)

## 1. What this is

v1 of the image-edit endpoint treats the instruction as an input to a picture:
it is recorded on the audit row only, and the new image-only assistant row
inherits the source turn's `user_message_id`. The consequence is that the
user's own words leave no trace in the conversation — after a refresh the
thread shows a new picture with no message that asked for it.

v2 adds an opt-in switch, `persist_instruction`. When set, the instruction is
persisted as an ordinary `role='user'` chat message quoting the source image
turn, and the new image row hangs off that message. History then replays the
whole exchange naturally: user instruction → assistant picture.

The design intent behind the switch: although the endpoint is named "edit",
the interaction is the user *asking* the character for a revision. The
persisted row is a real user message — it is not filtered from companion
context, memory extraction, or later affinity evaluation. No new metadata
keys are invented; the row uses the existing quote mechanism.

## 2. Contract

### 2.1 Request

`ImageEditRequest` gains one field:

```rust
/// Persist the instruction as a `role='user'` chat message quoting the
/// source image turn, and hang the new image row off it. Default false —
/// the v1 audit-only behaviour.
#[serde(default)]
pub persist_instruction: bool,
```

`false` (or absent) is byte-for-byte the v1 contract. Every existing caller
is unaffected.

### 2.2 Response

`ImageEditResponse` gains one field, present only when a user row was
persisted:

```rust
/// The persisted instruction message, when `persist_instruction` was set.
#[serde(skip_serializing_if = "Option::is_none")]
#[schema(value_type = Option<String>)]
pub instruction_message_id: Option<Uuid>,
```

The consumer can render the user bubble without refetching history.

### 2.3 Status ladder

Unchanged. One 409 arm narrows: "source image turn has no originating user
message" applies only when `persist_instruction` is false — with the switch
on, the new user row is the anchor and the source's `user_message_id` is not
read. (No engine path produces such a source; the arm remains for defense.)

## 3. Behaviour (`persist_instruction: true`)

### 3.1 Persistence

Both rows are written in **one transaction**, only after the composer
succeeds (a failed compose persists nothing — the v1 semantics; retry is
safe):

1. A `role='user'` row: `content` = trimmed instruction,
   `metadata` = `{"reply_to_message_id": "<source message_id>"}` — the same
   key the chat stream path writes for a quoted turn, so the history
   endpoints hand it back unchanged and a client renders it as a quote of
   the source picture. This is also how "this row is an edit instruction"
   is recognized: a user row quoting an image turn, with an image-only
   assistant row hanging off it.
2. The assistant image row, exactly as v1 builds it (same `image` marker,
   `edit_of`, `image_ref: "previous"`, compose-event link), except
   `user_message_id` = the id of row 1 instead of the source's.

`last_active_at` bumps once, as `insert_assistant_batch` does today.

The assistant-side marker is unchanged — no new keys on the image row.

### 3.2 Downstream visibility

The persisted user row is an ordinary user message. Deliberately:

- it enters the history window, so subsequent turns' companion context,
  memory extraction, and affinity evaluation see it as something the user
  said — no filtering marker of any kind;
- the edit call itself still runs nothing else: no PDE verdict, no affinity
  movement, no insight or memory extraction, no queue. The row is visible
  to later pipelines, not a trigger of any.

After this change a persisted edit turn is shape-identical to a chat-path
image turn (real user row → empty-content assistant row with an `image`
marker), so history rendering, turn pairing, and the recovery endpoint need
no special casing.

### 3.3 Audit

Unchanged. The instruction is still recorded in the compose-event `inputs`;
the audit row does not reference the new user row.

## 4. Implementation shape

- `eros-engine-store/src/chat.rs`: one new `ChatRepo` method,
  `insert_instruction_turn(session_id, instruction, source_message_id,
  AssistantInsert) -> Result<Uuid, _>` — inserts the user row and the
  assistant row in one transaction, bumps `last_active_at`, returns the user
  row's id. The assistant insert reuses the same column set as
  `insert_assistant_batch`.
- `eros-engine-server/src/routes/image_edit.rs`: branch at persistence time —
  `false` → `insert_assistant_batch` keyed to the source's `user_message_id`
  (v1 path, untouched); `true` → `insert_instruction_turn`, and
  `instruction_message_id` in the response.
- OpenAPI regenerated; `docs/api-reference.md` / `.zh.md` image-edit sections
  updated (the "never persisted as conversation" claim becomes conditional).

## 5. Not in scope

- No idempotency key for the persisted user row (the endpoint itself has
  none; a double-click makes two edits in v1 already).
- No change to which pipelines the edit call triggers.
- Web-side composer UX (chip copy, default for the switch) — the consumer
  decides policy; the engine default preserves v1.

## 6. Testing

Extend `image_edit.rs` tests:

1. `persist_instruction: true` → user row exists with trimmed content and
   `metadata.reply_to_message_id` = source id; assistant row's
   `user_message_id` = the new user row; response carries
   `instruction_message_id`; history replays both rows in order, with the
   quote handed back on the user entry.
2. `persist_instruction` absent/false → v1 behaviour byte-for-byte (existing
   tests already lock this; add an explicit `false` probe on the response:
   no `instruction_message_id` key).
3. Composer exhausted with `persist_instruction: true` → no rows persisted
   (extends the existing 502 test).
4. An edit of an edit with the switch on anchors to its own new user row.

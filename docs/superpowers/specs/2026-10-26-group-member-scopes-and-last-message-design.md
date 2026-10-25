# Group chat: per-member scopes on send, and the last message on the list — Design

- **Date:** 2026-10-26
- **Status:** Approved
- **Type:** Engine change. No migration. Additive API changes: one optional
  field on `GroupSendRequest`; one field on the group DTO (`GET
  /v2/comp/groups`, `GET /v2/comp/group/{group_id}`, and every route that
  returns the DTO). Ships in v2.3.1.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-12-group-chat-design.md` (*the group spec*). The
  1:1 send's `memory_scope` / `affinity_scope` and their resolution stand.

## 1. Why

A client that filters memory and affinity by the user's orientation and each
persona's gender does it per turn on the 1:1 send, through `memory_scope` and
`affinity_scope`. The group send has no such fields: every speaker runs with
the engine defaults (`neutral_and_relationship`, `bond`), so in a group a
persona the user is not into still recalls relationship memories, and one
the user is into runs on the bond axes only. And a client listing groups
has to fetch each group's history to show what was said last.

## 2. Per-member scopes on send

`GroupSendRequest` gains `member_scopes`: an object keyed by
`member_session_id`, each value `{memory_scope?, affinity_scope?}` with the
1:1 send's types (`MemoryScope` string; `AffinityScopeDto`, a named scope or
an axes array). All optional; absent, `null` or `{}` means every speaker
runs on the engine defaults, as today.

```json
{"content": "…", "client_msg_id": "…",
 "member_scopes": {
   "<member_session_id>": {"memory_scope": "neutral_only", "affinity_scope": "bond"},
   "<member_session_id>": {"affinity_scope": ["warmth", "trust"]}
 }}
```

- Each speaker's turn uses its own entry; a missing entry, or a missing key
  inside one, is the engine default for that field. The same resolution as
  the 1:1 send (`AffinityScopeDto::resolve`, `MemoryScope` default).
- A key that is not a seated member is refused the way a stray mention is:
  `422 unprocessable`, before anything is enqueued. The check runs on the
  active members loaded for the turn, so a kicked member's key is refused.
- What the 1:1 send persists for a scope, the group turn persists for the
  speaker it applies to (the resolved scopes on that speaker's assistant row
  metadata, as the pipeline already does with the `Event`); `member_scopes`
  itself is not stored.
- Replays of a settled `client_msg_id` are unaffected: they re-emit rows.

## 3. The last message on the group DTO

The group DTO gains `last_message`: the newest visible row of the group
session, or `null` for a group with none.

```json
{"group_id": "…", "title": "周末爬山", "members": [...],
 "created_at": "…", "last_active_at": "…",
 "last_message": {"message_id": "…", "speaker_session_id": "…", "role": "assistant",
                  "preview": "山顶的风好大", "is_image": false, "sent_at": "…"}}
```

| field | value |
|---|---|
| `message_id` | the row |
| `role` | `user` or `assistant` |
| `speaker_session_id` | the speaker's member session on an assistant row; `null` on a user row |
| `preview` | the row's content, trimmed, cut to its first 120 characters |
| `is_image` | the row carries an image (`metadata.image` present) |
| `sent_at` | the row's `sent_at` |

A row is **visible** when it is a `user` or `assistant` row, its channel is
not `product_qa`, and it has text or an image (`btrim(content) <> ''` or
`metadata ? 'image'`). This is the same set a history page shows a reader
and skips the rows that would make an empty preview: a failed attempt with
no text, a silent fallback, an out-of-character product answer. A ghosted
user row is visible: the user wrote it and sees it.

One query serves the list: a lateral join picks each group's newest visible
row. The detail route reuses it for one group.

## 4. Out of scope

- Unread counts, or anything per reader.
- Storing `member_scopes`, or a per-group default scope.
- Changing what a scope means, or the group's memory writes (one relationship
  row per seated member keeps the group spec's rule).

## 5. Testing

- Route: a `member_scopes` key that is not a member ⇒ `422 unprocessable`,
  nothing enqueued; a valid map reaches the speakers (the resolved scopes
  on each speaker's row metadata match the entry; a member without an entry
  gets the defaults); absent map ⇒ byte-identical turn to today's.
- Store: the last visible row per group across a list of groups; `null` for
  an empty group; a product_qa row, an empty assistant row and an image-only
  row (visible) each handled; the 120-character cut; `speaker_session_id`
  on an assistant row and `null` on a user row.
- Route: the list and the detail carry `last_message`.

## 6. Documentation

`docs/group-chat.md` and `.zh.md`: the Send request body gains
`member_scopes` with the 422 rule; the group DTO example and the management
table gain `last_message` with the visibility rule. The v2.3.1 release notes.

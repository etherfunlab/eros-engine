# `companion_affinity_events.user_message_id` — Design

- **Date:** 2026-08-22
- **Status:** Approved, not yet implemented
- **Type:** Additive column on a live audit table + one new field on a BFF DTO
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` 1.6.x, one PR to `dev`
- **Depends on:** nothing. Independent of
  [2026-08-22-image-edit-endpoint-design.md](2026-08-22-image-edit-endpoint-design.md);
  the two take migration numbers in merge order.

## 1. Problem

`engine.companion_affinity_events` records one row per turn but nothing on the
row says *which* turn. The only link is `affinity_id → companion_affinity →
session_id`, so a consumer wanting "the delta this message caused" has to match
`created_at` against `chat_messages.created_at` — a guess, not a fact. Every
sibling audit table (`companion_decision_events`, `*_insights_events`,
`chat_vision_events`) already carries a message reference; this one is the
outlier.

## 2. Which message

**The user message that drove the turn.** Not the assistant message(s):

- A turn has exactly one user message. It may have zero assistant messages
  (`ghost`) or several (a burst), so an assistant-side key is either absent or
  a lossy "last one".
- `message` / `ghost` / `gift` turns all have a user message.
- Assistant rows already point back at it via
  `chat_messages.user_message_id`, so "all replies for this event" is
  `WHERE user_message_id = event.user_message_id` — a join, not a second column.

`proactive` and `time_decay` rows have no user message and store `NULL`.

The column is named **`user_message_id`**, not `message_id`: it holds the same
fact as `chat_messages.user_message_id` and `chat_turn_queue.user_message_id`
and takes the same name. The generic `message_id` on the insight tables names a
different fact (the assistant message that was mined).

## 3. Schema — migration `0056_affinity_event_user_message.sql`

```sql
ALTER TABLE engine.companion_affinity_events
    ADD COLUMN user_message_id UUID NULL
        REFERENCES engine.chat_messages(id) ON DELETE SET NULL;

CREATE INDEX idx_affinity_events_user_message
    ON engine.companion_affinity_events (user_message_id)
    WHERE user_message_id IS NOT NULL;
```

**A real foreign key, deliberately.** The audit-table convention in this
codebase is "no FK — the trail must outlive the row it describes". That
argument never held for this table: it already cascades away with its session
through `affinity_id → companion_affinity(session_id) ON DELETE CASCADE`, which
`session_archive.rs` documents as accepted. Adding an FK therefore removes
nothing that exists today. `ON DELETE SET NULL` rather than `CASCADE` so that a
message deleted on its own (migration 0027 did that once for legacy gift rows)
blanks the pointer instead of erasing the event.

The write order makes the FK safe: the user row is inserted before the turn
starts, and both affinity writers run strictly after it (`post_process::run` is
spawned after the `final` frame; the ghost path writes after
`mark_user_message_ghosted`).

**No backfill.** Rows written before the migration stay `NULL`. A backfill by
timestamp proximity would write a guess into a column whose whole point is to
replace one.

No lockdown block: the table's `REVOKE` / RLS from migration 0013 covers new
columns.

## 4. Writers — `crates/eros-engine-store/src/affinity.rs`

Both event writers gain one parameter, `user_message_id: Option<Uuid>`, bound
as a new column in their INSERTs:

- `AffinityRepo::persist_with_event(...)` — already carries
  `#[allow(clippy::too_many_arguments)]`; one more positional argument, no
  insert struct introduced for it.
- `AffinityRepo::record_ghost(&mut Affinity, user_message_id: Option<Uuid>)`.

Call sites:

| Site | Value |
|---|---|
| `pipeline/post_process.rs::persist_affinity` (→ `persist_with_event`, and the `Ghost` arm's `record_ghost`) | `Some(message_id)` from `Event::UserMessage { message_id, .. }`; `None` for any other `Event` variant |
| `pipeline/stream.rs` ghost arm (`record_ghost`, next to `mark_user_message_ghosted`) | `Some(user_msg.user_message_id)` |
| store tests that call either writer | `None` unless the test is about this column |

`persist_affinity` gains a `user_message_id: Option<Uuid>` parameter threaded
from `run()`, which destructures `message_id` out of the event alongside
`content`. `proactive` reaches `persist_with_event` through the same function
with whatever `run()` was given — in practice `None`, since proactive turns do
not arrive as `Event::UserMessage`.

Nothing moves into `context`. The column is the authority; the JSON blob does
not get a copy.

## 5. Reader — the BFF delta endpoint

The column is useless to a downstream that may not read `engine.*` tables
unless an endpoint carries it, so:

- `AffinityEventRow` gains `pub user_message_id: Option<Uuid>`; the two SELECTs
  that build it (`list_events`, `latest_turn_event`) add the column.
- `BffAffinityDelta` (`GET /bff/v1/comp/affinity/{session_id}/event`) gains
  `user_message_id: Option<Uuid>`, `skip_serializing_if = "Option::is_none"` —
  the same key-presence contract as its other nullable fields. Absent on
  pre-migration rows.

Additive only. No new endpoint, no v2 twin: the v2 convention says a v2 form
appears only when behaviour changes, and this is a new key on a v1 response.

## 6. Not in scope

- Backfill (§3).
- An assistant-side column. Derivable by join (§2).
- Exposing `user_message_id` on `list_events` consumers other than the BFF
  delta (there are none in the server today).
- Any change to `context`, `event_type`, or the CHECK constraint.

## 7. Testing

**Store (`sqlx::test`):**

- `persist_with_event` with `Some(id)` round-trips through `latest_turn_event`
  and `list_events`; with `None` reads back `NULL`.
- `record_ghost` with `Some(id)` stores it.
- An id not present in `chat_messages` fails the INSERT (proves the FK exists,
  not just the column).
- Deleting the referenced message leaves the event row with `NULL` (proves
  `SET NULL`, not `CASCADE`).

**Pipeline (`sqlx::test`, mocked OpenRouter, mirroring the existing affinity
post-process tests):**

- A `message` turn's event row carries the driving user message id.
- A `ghost` turn's row carries it (stream arm).

**Server:**

- The BFF delta endpoint echoes `user_message_id` on a seeded row that has one
  and omits the key on a seeded row that does not.

**Docs:** `docs/affinity-model.md` + `.zh.md` "Event rows" section lists the
column; `docs/api-reference.md` + `.zh.md` BFF delta section documents the
key. OpenAPI regenerated.

**Pre-PR gate:** `cargo fmt --check`, `cargo clippy`, `cargo test`, OpenAPI
regeneration check.

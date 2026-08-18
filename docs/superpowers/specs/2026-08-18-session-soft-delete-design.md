# Soft-deleting a relationship's sessions — Design

- **Date:** 2026-08-18
- **Status:** Approved
- **Type:** Engine change — one schema migration (one boolean column), one new
  endpoint, an `archived` filter on every session read.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` 1.4.0 proposed — additive endpoint plus a read-visibility
  change on existing routes. The release number and its timing are the owner's
  call, not this document's.

## 1. Motivation

The engine has never offered a way to delete a conversation, so downstream
clients delete rows themselves. The verb available to them is `DELETE`, and
`engine.chat_sessions` has three `ON DELETE CASCADE` children — `chat_messages`,
`companion_affinity` (which cascades on to `companion_affinity_events`), and
`companion_memories`. A client that wants "start over with this companion" gets
the whole cascade, because that is the only thing the schema will do for it.

Three consequences, all bad:

**Chat history is destroyed permanently.** The user asked to clear a
conversation, not to erase the record. There is no recovery path — the rows are
gone.

**Audit rows survive their subject.** `companion_decision_events`,
`chat_vision_events` and `chat_images_events` all carry `session_id` as a plain
UUID with no foreign key, so they outlive the delete. An operator inspecting an
image-generation record can find the session id and nothing it points at.

**The engine never learns it happened.** The semantics of "delete a
conversation" are currently decided by which foreign keys happen to exist, in a
client the engine cannot see. That is a rule with no owner.

What the client actually wants is narrower than what it gets: the conversation
should stop being visible and stop being resumable, and the relationship state
should reset so a fresh start is genuinely fresh. Neither of those requires
destroying the transcript.

## 2. Design principles applied

1. The transcript is evidence and is never deleted by an API call. Visibility is
   a flag; the rows stay.
2. Relationship state — affinity, relationship-layer memory, character insights —
   *is* deleted, because leaving it would contaminate the next conversation.
   Requirement, not oversight.
3. `archived` is a boolean, not a status enum. A session is visible or it is
   not; there is no third state to name.
4. No restore endpoint. Reviving a session is an operator action, and `UPDATE`
   is the whole of it.
5. The engine does not touch ownership. `persona_instances.status` and the
   client's own ownership tables are the client's business.

## 3. The endpoint

```
DELETE /comp/instance/{instance_id}/sessions
→ 200 { "instance_id": "…", "archived_sessions": 3 }
```

Keyed by instance, matching `GET /comp/instance/{instance_id}/profile`. The
instance is the unit of a relationship: one user, one persona, however many
sessions across however many channels. Archiving one session and leaving its
siblings resumable would not give the caller the fresh start it asked for.

- Auth: the existing bearer-JWT layer. The instance's `owner_uid` must equal the
  JWT `sub` — mismatch is `403`, unknown instance is `404`. Ownership is read
  through the instance rather than compared against a path parameter, the same
  way `get_character_profile` does it.
- Idempotent. A second call finds nothing left to archive and returns
  `archived_sessions: 0` with `200`.
- `archived_sessions` counts sessions flipped by *this* call.

### Reviving a session

Deliberately not an endpoint. An operator runs:

```sql
UPDATE engine.chat_sessions SET archived = false WHERE id = '…';
```

That restores the session and its transcript. Affinity, relationship-layer
memories and character insights are gone and do not come back — the revived
session resumes with a cold relationship. This is the documented and intended
outcome, not a limitation: those rows were deleted precisely so the next
conversation would not inherit them.

## 4. Schema

Migration `0052_chat_sessions_archived.sql`:

```sql
ALTER TABLE engine.chat_sessions
    ADD COLUMN archived BOOLEAN NOT NULL DEFAULT false;
```

**No index.** The only hot path is resume, which reaches its rows through
`idx_chat_sessions_user_instance_channel` and lands on a single-digit number of
candidates; filtering `NOT archived` on the heap from there costs nothing worth
an index. Archived rows are read only by hand-written SQL.

**No backfill.** Sessions destroyed by earlier hard deletes are gone. Nothing to
mark.

## 5. The archive action

One transaction, four statements:

```sql
UPDATE engine.chat_sessions SET archived = true
 WHERE user_id = $1 AND instance_id = $2 AND NOT archived;

DELETE FROM engine.companion_memories
 WHERE user_id = $1 AND instance_id = $2;

DELETE FROM engine.companion_affinity
 WHERE user_id = $1 AND instance_id = $2;

DELETE FROM engine.character_insights WHERE instance_id = $2;
```

**Sessions on every channel are archived**, voice included. The unit is the
relationship, not one client's view of it.

**Memories are deleted by `(user_id, instance_id)`, not by session id.**
`MemoryRepo::search` retrieves on exactly that key, so the delete key has to
match the read key — deleting by session id would leave a row retrievable that
the caller believed gone. The same predicate is what protects the profile layer:
profile memories carry `instance_id IS NULL`, and `instance_id = $2` never
matches NULL. Those rows are cross-persona facts about the user, shared with
every companion; ending one relationship must not erase them. Today's hard
delete does erase them, via the `session_id` cascade. That is a data-loss bug
this design fixes by construction.

**`companion_affinity_events` goes with its parent** through the existing
`ON DELETE CASCADE`. Accepted, not worked around: those events are the ledger of
a relationship that no longer exists. The audit surface an operator needs —
`companion_decision_events`, `llm_attempt_audit`, `chat_vision_events`,
`chat_images_events` — holds `session_id` without a foreign key and is untouched
by any of this. Combined with the transcript now surviving, an operator
inspecting an image-generation record can finally read the conversation around
it. That is the point of the change.

**`character_insights` is deleted.** It is instance-keyed, so today it survives
the hard delete and carries the persona's memory of the relationship — where the
user lives, what they do, the current situation between them — straight into the
next conversation. Leaving it contradicts the reason affinity and memories are
deleted at all.

**`persona_instances.status` is not touched.** The user still owns the persona
and can start a new conversation immediately; `chat/start` will create a fresh
session because no unarchived one remains.

## 6. Read visibility

An archived session does not exist as far as the API is concerned.

**One choke point covers most of it.** `ChatRepo::get_session` gains
`AND NOT archived`. Every entry-point guard resolves the session through it, so
a single predicate turns all of these into `404`:

| Route | Path to `get_session` |
|---|---|
| `GET /comp/chat/{sid}/history` | `require_session_for_user` |
| `GET /bff/v1/comp/chat/{sid}/history` | `require_session_for_user` |
| `GET /bff/v1/comp/affinity/{sid}` | `require_session_for_user` |
| `GET /bff/v1/comp/affinity/{sid}/event` | `require_session_for_user` |
| `POST /comp/chat/{sid}/message/stream` | direct call |
| `POST /comp/voice/{sid}/turn/stream` | direct call |

**Five queries do their own session lookup and need the predicate added:**

1. `resume_latest_session` — the critical one. Without it the next
   `chat/start` reanimates the archived session and the user gets their deleted
   conversation back with a blank relationship.
2. `create_or_resume`
3. `list_sessions` (`GET /comp/chat/{user_id}/sessions`)
4. `latest_sibling_voice_session`
5. `story.rs` — the three joins onto `chat_sessions` that feed world stories,
   so archived material stops being harvested.

**`bff_list_affinities` needs no change.** It is driven by
`companion_affinity`, whose rows this endpoint deletes; an absent row is an
absent list entry.

## 7. Testing

- Archiving leaves `chat_messages` row count unchanged; `companion_affinity`,
  relationship-layer `companion_memories` and `character_insights` go to zero.
- Profile-layer memories (`instance_id IS NULL`) survive.
- `chat/start` after archiving returns a **new** session id, not the archived one.
- History, affinity and stream routes return `404` for an archived session.
- Voice-channel sessions are archived alongside text ones.
- Another user's instance → `403`; unknown instance → `404`; repeat call →
  `200` with `archived_sessions: 0`.
- `UPDATE … SET archived = false` makes history readable again.

## 8. Downstream

Not delivered by this spec, and the feature does not hold in production without
it. A client that reads `engine.chat_sessions` directly — through a privileged
SQL function, say, to build a chat list — must add `AND NOT archived` to those
reads. The engine cannot enforce visibility on a query it never sees. Any client
that currently hard-deletes sessions replaces that with a call to this endpoint.

## 9. Not doing

- **A restore endpoint.** §3 covers why.
- **Preserving `companion_affinity_events`.** Keeping them past their parent row
  means a nullable `affinity_id` or a redundant `session_id` column, and an
  orphan state for every reader to handle. §5 covers why the audit story does
  not need it.
- **Zeroing the affinity row instead of deleting it.** Reusing the row makes
  "delete and start over" into "same ledger, continued," which is the shape that
  caused the tier-oscillation problem this codebase already has a note about.
- **A `status` enum on `chat_sessions`.** There is one distinction to draw.
- **Touching `human_insights`.** User-level, cross-persona, same reasoning as the
  profile memory layer.

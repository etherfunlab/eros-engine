# Downstream-triggered openers on `/open` — Design

- **Date:** 2026-10-03
- **Status:** Draft — ready for review
- **Type:** Engine change. Two new optional request fields and one new
  response field on `POST /v2/comp/session/{session_id}/open`, one migration
  (one partial unique index on `chat_messages`), one changed predicate on the
  holiday path. `eros-engine-core` is untouched.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — additive; a body without the new fields behaves
  exactly as v1.9.4. Release number and timing are the owner's call, not this
  document's.
- **Builds on:** `2026-09-26-user-locale-and-holiday-greeting-design.md`
  (the `/open` endpoint, `pipeline/proactive.rs`, migration `0063`).

## 1. Motivation

`/open` lets the persona speak first only on a user-local holiday, and only
the engine decides when. A client also knows moments when the persona should
open the conversation — a brand-new chat, a user coming back after days away,
an ordinary visit it wants to warm up — but has no way to ask for it.

This design lets the caller ask the persona to speak first when the user
enters a session. The caller decides *whether*; the engine decides *what*:
the caller picks an occasion from a fixed engine-defined set, and the engine
owns the wording and the facts behind it.

## 2. Decisions

Settled during design review; not open for re-derivation during
implementation.

1. **The user is present.** The opener is produced when the user enters a
   session, synchronously, as `/open` already does. Messages to an absent user
   (scheduled or pushed outreach) are out of scope.
2. **Occasions are an engine-defined enum:** `first_meet`, `returning`,
   `just_opened`. The engine owns each occasion's wording. There is no
   caller-supplied free-text occasion; callers that want extra guidance already
   have `prompt_traits` on the same body.
3. **The holiday stays engine-decided.** It is not one of the caller's
   occasions. A body without `occasion` runs the v1.9.4 holiday path unchanged;
   a body with `occasion` skips the holiday decision entirely (the holiday
   still reaches the prompt through `[now]`).
4. **The caller supplies an idempotency key.** `open_key` is required with
   `occasion`. The same key returns the same message; a new key may produce a
   new one. How often the persona opens is the caller's policy — the engine
   sets no frequency cap.
5. **The engine verifies the occasion is true.** An occasion whose
   precondition fails yields `greeting: null` with no model call, not an error:
   the usual cause is a race (the user already spoke in another tab), not a
   caller bug.
6. **One proactive message per quiet stretch on the holiday path.** A
   caller-triggered opener today counts as "someone spoke today" for the
   holiday decision, so the two never stack. The reverse is not blocked: a
   holiday greeting does not stop a caller-triggered opener under a new key.

## 3. Wire contract

### 3.1 Request

Two fields added to `OpenSessionRequest`; all existing fields keep their
meaning.

| Field | Type | Rules |
|---|---|---|
| `occasion` | `"first_meet"` \| `"returning"` \| `"just_opened"` | absent → the holiday path (§4.1) |
| `open_key` | string | 1–128 chars, `^[A-Za-z0-9_.:-]+$`. Required when `occasion` is present; rejected when it is absent |

Validation failures — `occasion` without `open_key`, `open_key` without
`occasion`, an `open_key` outside the rules — return 400 `invalid_payload` in
the `StreamPreErrorBody` shape the route already uses. An unknown `occasion`
value fails body deserialization and returns 422, as an unknown
`memory_scope` does.

`open_key` is scoped to the session: the same key on two sessions names two
different openers.

### 3.2 Response

`GreetingDto` gains one field:

```json
{ "greeting": {
    "message_id": "…", "content": "…", "sent_at": "…",
    "occasion": "returning"
} }
```

`occasion` is `"holiday"`, `"first_meet"`, `"returning"` or `"just_opened"`,
read from the row's `metadata.proactive` (§6.1) — not stored a second time.
Holiday greetings written by v1.9.4 already carry `proactive: "holiday"`.

## 4. Behaviour

### 4.1 Without `occasion`

The v1.9.4 holiday path (holiday design §4.2), with one change: the "anyone
spoke since local midnight" check now counts caller-triggered openers (§6.4).

### 4.2 With `occasion`

After the existing tier / traits / audit validation and the
ownership / channel / persona checks (`resolve_text_turn`):

1. **Existing key.** A proactive row in this session whose
   `metadata.open_key` equals `open_key` → return it, whatever `occasion` this
   call names. No model call.
2. **Precondition.** No model call and `greeting: null` when it fails:

   | Occasion | Precondition |
   |---|---|
   | `first_meet` | the session has no rows at all |
   | `returning` | the session has at least one `role = 'user'` row |
   | `just_opened` | none |

3. **Generate** (§5) under the same per-user in-flight cap as every other LLM
   entry point (429 when reached).
4. **Persist** atomically (§6.2). The insert re-checks that no message other
   than a holiday greeting landed in the session since this request started;
   if one did, nothing is written and the response is `null` — the user spoke
   first.

Failure handling matches the holiday greeting: a failed generation returns the
upstream error (the provider's status passes through, 502 by default) and
persists nothing; a blank reply, or one `output_regex` strips to empty,
persists nothing and returns `null`.

## 5. Generation

### 5.1 Pipeline

Identical to the holiday greeting (holiday design §6.1–§6.2):
`build_reply_request` with a synthetic `Event::UserMessage` used only for
prompt assembly, the nil driving message id (history falls back to newest-N),
a hand-built `ReplyText` plan with `ReplyStyle::Neutral`, zero deltas and
default nudges, one non-streaming `openrouter.execute`, `record_generation`,
and Layer-0 `output_regex` keyed on the served hop. No PDE, no dice, no
post-process. `[now]` renders as on every turn, so a request carrying
`user_timezone` still gets the user-side holiday line.

### 5.2 Stage cues

Each occasion has one engine-owned cue, appended after the history as a
`user`-role message and never persisted. A cue states a fact; it carries no
instruction.

| Occasion | Cue |
|---|---|
| `holiday`, `just_opened` | `（对方刚打开和你的聊天，还没说话。）` — the existing `OPEN_CUE` |
| `first_meet` | `（你们还没聊过。对方刚打开和你的聊天，还没说话。）` |
| `returning`, `gap_days` ≥ 1 | `（对方隔了 {gap_days} 天又打开和你的聊天，还没说话。）` |
| `returning`, `gap_days` = 0 | `（对方隔了不到一天又打开和你的聊天，还没说话。）` |

### 5.3 The gap

`gap_days = floor((now − latest user row's sent_at) / 24h)`. Only the day count
reaches the model.

### 5.4 Memory recall query

The synthetic event's `content` is the recall query:

- `returning`, `just_opened`: `recall_query_text` of the latest user row, so
  an image-only message recalls on its vision description.
- `first_meet`: empty. `recall_memory` skips on an empty query; the insight
  bullets (`基础画像`) load regardless.

## 6. Persistence

### 6.1 The row

Same shape as the holiday greeting: `role = 'assistant'`,
`assistant_action_type = 'proactive'`, `user_message_id` NULL, `channel` NULL,
`read_at` NULL. Bumps `chat_sessions.last_active_at`.

`metadata`:

- `proactive`: the occasion name
- `open_key`
- `gap_days`: `returning` only
- `user_timezone`, `user_country`, `user_region`: those present, raw
- `tier`, `prompt_traits`, `memory_scope`, `affinity_scope`: the post-resolve
  keys a reply row carries

No `local_date` key: the row must never meet the holiday index
(`chat_messages_proactive_day_uidx`).

### 6.2 Insert

A new `ChatRepo::insert_open_message(session_id, since, row)`, separate from
`insert_proactive_message` because one `ON CONFLICT` names one arbiter index:

```sql
INSERT INTO engine.chat_messages (…)
SELECT …, 'proactive', …
WHERE NOT EXISTS (
    SELECT 1 FROM engine.chat_messages
    WHERE session_id = $session AND sent_at >= $since
      AND <not a holiday greeting>
)
ON CONFLICT (session_id, (metadata->>'open_key'))
    WHERE assistant_action_type = 'proactive' DO NOTHING
RETURNING *
```

`since` is the request's start time — the `now` the handler already takes
(and tests inject). When nothing is returned, the handler
reads the row for `open_key`: a concurrent call with the same key that won the
insert yields its row; a user message that landed first yields `None`, and the
response is `null`. Two concurrent calls with one key can both generate; one
insert wins. The cost is one extra model call in that race.

### 6.3 Migration `0064_chat_messages_open_key.sql`

```sql
CREATE UNIQUE INDEX chat_messages_proactive_open_key_uidx
    ON engine.chat_messages (session_id, (metadata->>'open_key'))
    WHERE assistant_action_type = 'proactive';
```

Holiday rows have no `open_key`; the expression is NULL and never conflicts.
The CHECK on `assistant_action_type` already admits `'proactive'`. `open_key`
is not a reference, so there is no FK work. Building the index scans
`chat_messages` and blocks writes for the duration, as `0063` did.

### 6.4 The holiday path's predicate

`ChatRepo::has_message_since` and the re-check inside
`insert_proactive_message` exclude every proactive row today. Both change to
exclude only holiday greetings:

```sql
AND NOT (assistant_action_type = 'proactive'
         AND metadata->>'proactive' = 'holiday')
```

A holiday greeting for another local date still never blocks today's; a
caller-triggered opener since local midnight now does. §6.2's re-check uses the
same predicate.

### 6.5 What does not run

As for the holiday greeting: no post-process (no memory write, no insight
extraction, no affinity event), the ghost streak is untouched, and
`compute_signals_for_session` counts user rows only, so the opener changes
neither `message_count` nor `hours_since_last_message`.

## 7. Testing

Route tests follow `routes/session_open.rs`: local Postgres, companion call
mocked with wiremock.

- **Each occasion's happy path:** row shape and every §6.1 metadata key; the
  response's `occasion`; the model request's last message is the occasion's
  cue.
- **Preconditions:** `first_meet` on a non-empty session and `returning` on a
  session with no user row → `null`, no model call.
- **Gap:** a user row 3 days old → the cue reads `隔了 3 天`; under 24 hours →
  `隔了不到一天`; `gap_days` recorded.
- **Idempotency:** the same key twice → the same row, one model call; a new key
  → a new row; two concurrent calls with one key → one row.
- **Validation:** 400 `invalid_payload` for `occasion` without `open_key`,
  `open_key` without `occasion`, and an `open_key` with a forbidden character;
  422 for `occasion: "holiday"`.
- **Interplay:** an opener today → a later body without `occasion` on a
  holiday returns `null`; a holiday greeting today → an opener under a new key
  still generates.
- **Regression:** every existing holiday test passes unchanged.

Store tests:

- The `open_key` index rejects a repeated key in one session and allows it
  across sessions.
- `insert_open_message` writes nothing when a user row landed at or after
  `since`.
- `has_message_since` counts a caller-triggered opener and still ignores a
  holiday greeting.

`openapi.json` regenerated for the CI drift check.

## 8. Documentation

- `docs/api-reference.md` and `docs/api-reference.zh.md`, the `/open` section:
  the two request fields, each occasion's precondition, the response's
  `occasion`, and the interplay with the holiday path.
- The usual concept sweep of `docs/`, `examples/` and README for anything
  describing `/open` or the persona speaking first.

## 9. Non-goals

- Messages to an absent user: scheduling, push, outreach.
- An engine-side cap on how often the persona opens.
- Caller-supplied free-text occasions, or occasions beyond the three.
- Persona-authored opening lines.
- Voice sessions (`/open` stays text-only).

# Life events — Design

- **Date:** 2026-10-13
- **Status:** Draft — ready for review
- **Type:** Engine change. One migration (a partial unique index). One new
  route, `POST /v2/comp/session/{session_id}/events`. Additive changes: one
  new field on `GET …/effects`, one new `occasion` value on the greeting DTO.
  `eros-engine-core` gains a module; nothing in it breaks.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — a caller-triggered event that the persona tells
  the user about, and that the judge and the reply see while it lasts.
- **Builds on:** `2026-10-03-open-occasions-design.md` (*the open spec*:
  `/open`, `pipeline/proactive.rs`) and `2026-10-13-persona-needs-design.md`
  (*the needs spec*).

## 1. Motivation

Things happen to the persona only when the conversation invents them. With
this design, a caller can ask for something to happen in her day: she gets
called in to work, her ex texts her, the weather turns, the police stop her, a
stray cat crosses her path. She tells the user in her own words, and for a
while afterwards the judge and the reply know she is in the middle of it.

## 2. Decisions

1. **The caller decides when, the engine decides whether and what.** The
   caller draws; the engine picks the kind, or none.
2. **A rule director, no extra model call.** The engine's rules choose the
   kind. The one model call is the persona telling the user about it, through
   the `/open` generation path.
3. **The event is the message.** The persona's proactive row carries the
   event in its `metadata`; there is no event table.
4. **One event at a time.** No draw while an event is under way. Each kind
   has its own cooldown. Among the kinds left, the persona's needs weight the
   pick.
5. **Events reach the needs only through the judge.** The judge sees the
   event under way and rates the needs itself; the needs keep one source.
6. **The caller can narrow the kinds.** An optional allowlist per draw lets
   the caller keep, say, the police out of a persona's world.

## 3. Kinds

| `kind` | weight | cooldown | lasts | weight from needs | gate | label |
|---|---|---|---|---|---|---|
| `temp_work` | 3 | 24 h | 4 h | ×0.5 when `stamina` is `low` | — | 临时接到工作 |
| `ex_message` | 1 | 72 h | 3 h | ×3 when `emotional` is `high` | intimacy rung ≥ 2 | 前任发来消息 |
| `weather_change` | 4 | 8 h | 4 h | — | — | 天气突然变了 |
| `police_encounter` | 1 | 48 h | 1 h | — | — | 被警察拦下问话 |
| `animal_encounter` | 3 | 12 h | 1 h | ×2 when `emotional` is `high` | — | 路上遇到小动物 |

- **Weights** are relative. With no needs rating, no factor applies.
- **The gate** reads the session's affinity row, as `/open` loads it. Below
  rung 2, `ex_message` is never drawn: a persona does not confide this to
  someone she barely knows.
- **The label** is what the judge and the reply read (§6).
- **The table** is a set of `const`s in `core/src/events.rs`, retuned in
  place.

Each kind has a stage cue, which states a fact and carries no instruction:

| `kind` | cue |
|---|---|
| `temp_work` | （你刚临时接到一份工作，得马上去处理。） |
| `ex_message` | （你的前任刚给你发来消息。） |
| `weather_change` | （你那边的天气刚刚突然变了。） |
| `police_encounter` | （你刚在外面被警察拦下来问话。） |
| `animal_encounter` | （你刚在路上遇到了一只小动物。） |

The persona fills in the rest — what work, what the ex wrote, which animal —
from her persona and her world.

## 4. The director

A new module `eros-engine-core/src/events.rs`:

```rust
#[serde(rename_all = "snake_case")]
pub enum EventKind { TempWork, ExMessage, WeatherChange, PoliceEncounter, AnimalEncounter }

pub struct PastEvent { pub kind: EventKind, pub started_at: DateTime<Utc>, pub ends_at: DateTime<Utc> }

pub struct DrawInput<'a> {
    pub now: DateTime<Utc>,
    pub needs: Option<Needs>,           // carried to now
    pub intimacy_rung: u8,
    pub history: &'a [PastEvent],       // the instance's recent events
    pub allowed: &'a [EventKind],
}

/// The kind drawn, or `None`. `roll` is uniform in [0, 1).
pub fn draw(input: &DrawInput, roll: f64) -> Option<EventKind>;

/// How long a kind lasts.
pub fn duration(kind: EventKind) -> chrono::Duration;
```

`draw`:

1. Any `history` entry with `ends_at > now` → `None` (an event is under way).
2. A kind is eligible when it is in `allowed`, passes its gate, and its newest
   `history` entry started at least one cooldown before `now`.
3. Each eligible kind gets its weight times its factors. `roll` picks one by
   cumulative weight.
4. No eligible kind → `None`.

The core takes `roll` rather than an RNG, so it stays free of `rand`; the
server rolls with `thread_rng`, as it rolls the turn nudges.

## 5. The route

### 5.1 Request

`POST /v2/comp/session/{session_id}/events`:

| Field | Type | Rules |
|---|---|---|
| `event_key` | string | required; 1–128 chars of `[A-Za-z0-9_.:-]`, as `open_key` |
| `kinds` | array of `kind` | optional; absent = all five; present = the allowlist (empty ⇒ nothing is drawn) |
| `user_timezone`, `user_country`, `user_region`, `tier`, `prompt_traits`, `memory_scope`, `affinity_scope`, `audit` | | as on `/open`, for generating the message |

A malformed `event_key` returns 400 `invalid_payload`. An unknown `kind`
fails deserialization with 422.

### 5.2 Behavior

After validation and `resolve_text_turn` (`404 session_not_found`,
`403 session_forbidden`, `409 wrong_channel`, and the rest of that gate):

1. **Existing key.** An event row in this session whose `metadata.event_key`
   equals `event_key` → return it. No draw, no model call.
2. **Draw.** Read the instance's needs carried to now (needs spec §5.2; none
   when the judge is not configured), the session's affinity, and the
   instance's events of the last 72 hours (the longest cooldown) across its
   sessions. `draw` with `thread_rng`. `None` → `{"event": null,
   "message": null}`, no model call.
3. **Generate** under the per-user in-flight cap every LLM entry point uses
   (429 when reached). `greet` with a new `Occasion::Event { event_key, kind
   }`: its cue is the kind's cue, and the recall query is the latest
   user row's `recall_query_text`, as for `just_opened`. The event's time is
   one instant taken after generation: it is the row's `sent_at`, and
   `ends_at` is that instant plus the kind's duration. A failed generation
   returns the upstream error and writes nothing. A blank reply, or one
   `output_regex` strips to empty, writes nothing and returns nulls.
4. **Persist** atomically (§5.4).

Unlike an opener, an event does not yield to a user message that lands while
it is generated: it happened whether or not the user spoke.

### 5.3 Response

```json
{"event": {"kind": "temp_work", "started_at": "…", "ends_at": "…",
           "message_id": "…"},
 "message": {"message_id": "…", "content": "…", "sent_at": "…",
             "occasion": "event"}}
```

- `message` is the `/open` `GreetingDto`. Its `occasion` gains the value
  `event`.
- `EventView` (`kind`, `started_at`, `ends_at`, `message_id`) is shared with
  `GET …/effects` (§7). `started_at` is the row's `sent_at`.
- Both fields are `null` when nothing was drawn, or when the insert lost to a
  concurrent draw under another key.

### 5.4 Persistence

The row has the shape of an opener: `role = 'assistant'`,
`assistant_action_type = 'proactive'`, `user_message_id` NULL, `channel` NULL,
`read_at` NULL; it bumps `chat_sessions.last_active_at`. Its `metadata` holds:

- `proactive`: `"event"`;
- `event`: `{"kind": "…", "ends_at": "…"}`. `ends_at` is stored because it is
  fixed when the event is written, and a later change to a kind's duration must
  not move past events;
- `event_key`;
- `user_timezone`, `user_country`, `user_region`, `tier`, `prompt_traits`,
  `memory_scope`, `affinity_scope`, as an opener's row carries them.

`ChatRepo::insert_event_message(instance_id, session_id, now, row)`:

```sql
INSERT INTO engine.chat_messages (…)
SELECT …
WHERE NOT EXISTS (
    SELECT 1 FROM engine.chat_messages m
      JOIN engine.chat_sessions s ON s.id = m.session_id
     WHERE s.instance_id = $instance
       AND m.metadata->>'proactive' = 'event'
       AND (m.metadata->'event'->>'ends_at')::timestamptz > $now
)
ON CONFLICT (session_id, (metadata->>'event_key'))
    WHERE metadata->>'proactive' = 'event' DO NOTHING
RETURNING *
```

When nothing is returned, the handler reads the row for `event_key`. A
concurrent call with the same key that won the insert yields its row.
Otherwise an event under way blocked it, and the response is null.

Two concurrent draws under different keys can both pass the `NOT EXISTS`
under READ COMMITTED and both land. This millisecond window is accepted, as
it is for `/open`.

### 5.5 Migration `0071_chat_messages_event_key.sql`

```sql
CREATE UNIQUE INDEX chat_messages_event_key_uidx
    ON engine.chat_messages (session_id, (metadata->>'event_key'))
    WHERE metadata->>'proactive' = 'event';
```

It also serves the event-history and event-under-way reads: event rows are
few, bounded by the cooldowns. `event_key` is not a reference, so there is no
FK work. Building it scans `chat_messages`, as `0064` did.

## 6. While it lasts

`ChatRepo::current_event(instance_id, at_message, now)` returns the instance's
event under way at `at_message`'s `sent_at`, or at `now` without one: the
event row with `sent_at ≤ at < ends_at`, if any. On a text turn, `at_message`
is the turn's user row, so a re-driven turn sees the same event.

- **Judge.** `build_pde_ctx` gains `event: Option<&str>` (the rendered text)
  and, when present, renders after `[需求]` and before `[身体状态]`:

  ```
  [近期事件] 临时接到工作（1 小时前开始，约 3 小时后结束）
  ```

  Hours are whole and rounded; under one hour reads `不到 1 小时前开始` /
  `不到 1 小时后结束`.
- **Reply.** `[character_state]` gains a line after `此刻的需要`:

  ```
  - 正在经历：临时接到工作（约 3 小时后结束）
  ```

  Passed by the stream path only. `/open`, proactive, image edit and tests
  pass `None`. The persona's own message about the event is in the history
  window besides.
- **Needs.** The event changes no stored level. The judge reads the
  `[近期事件]` line on the next turn and rates the needs with it in view.

## 7. `GET /v2/comp/session/{session_id}/effects`

The response gains `event`: the `EventView` under way now, or `null`. A page
that opens on a conversation can show it without having made the draw.

## 8. Interplay

- **Holiday and birthday.** An event row is not a dated greeting. It counts
  as a message for `has_message_since`, so no holiday or birthday greeting
  follows an event on the same local day.
- **Openers.** An event that lands while an opener is generating makes the
  opener yield (`greeting: null`), by the open spec's re-check.
- **Signals and post-process.** As for an opener: no post-process, no memory
  write, no affinity event; the ghost streak is untouched;
  `compute_signals_for_session` counts user rows only.
- **World Stories.** The story director reads the pair's recent chat, so an
  event message reaches the persona's life story with no extra write.

## 9. Where it lives

| change | file |
|---|---|
| `EventKind`, the kind table, `draw`, `duration` | `core/src/events.rs` (new), `core/src/lib.rs` |
| `insert_event_message`, `event_message`, `recent_events`, `current_event` | `store/src/chat.rs` |
| migration | `store/migrations/0071_chat_messages_event_key.sql` |
| `Occasion::Event`, its cue and metadata | `server/src/pipeline/proactive.rs` |
| the route, `EventView` | `server/src/routes/session_events.rs` (new), `routes/mod.rs` |
| `[近期事件]` line | `server/src/pipeline/stream.rs` |
| `正在经历` line | `server/src/prompt.rs`, `pipeline/handlers.rs` |
| `event` on the effects route | `server/src/routes/session_effects.rs` |
| `occasion: event` on `GreetingDto` | `server/src/routes/session_open.rs` |
| OpenAPI | `crates/eros-engine-server/openapi.json` (regenerated) |

## 10. Documentation

- `docs/api-reference.md` / `.zh.md`: the events route (request, the
  existing-key rule, the draw, the response, the nulls); the kind table
  without its weights; `event` on `GET …/effects`; `occasion: "event"` on the
  greeting.
- `docs/model-config.md` / `.zh.md`: the `[近期事件]` line in the judge
  context.
- The concept sweep (proactive messages, openers, `metadata.proactive`
  values) across `docs/`, `examples/` and the READMEs.

## 11. Rollback and deploy

- **Rolling back.** The route 404s. Event rows stay ordinary proactive rows
  in history; the previous image reads no event from them. The index is
  harmless.
- **Deploying.** The index builds at boot. Nothing happens until a caller
  draws.

## 12. Testing

- **core.** An event under way → `None`. A kind in cooldown is skipped, and
  is eligible again exactly one cooldown after it started. The allowlist,
  including an empty one. The `ex_message` gate at rungs 1 and 2. Each needs
  factor, and no factor without needs. `roll` at 0, just under 1, and on
  each cumulative boundary. `duration` for every kind.
- **store** (`#[sqlx::test]`). `insert_event_message` writes nothing while
  another session of the same instance has an event under way. A repeated key
  in one session conflicts; the same key in another session does not.
  `recent_events` reads across the instance's sessions only. `current_event`
  at, before and after `ends_at`.
- **route** (local Postgres, companion call mocked).
  - Happy path: the row's shape and every §5.4 metadata key; the response;
    the model request's last message is the kind's cue.
  - The same key returns the same event with one model call.
  - An event under way → nulls, no model call.
  - Every kind in cooldown → nulls, no model call.
  - `kinds: []` → nulls. 400 for a bad key; 422 for an unknown kind.
  - Blank reply → nulls, nothing written. Upstream failure → its status,
    nothing written.
  - 429 at the in-flight cap. 403, 404, `409 wrong_channel`.
- **stream.** A turn during an event renders `[近期事件]` and `正在经历`; a
  turn after `ends_at` renders neither; a re-driven turn renders the same
  line.
- **Interplay.** An event today → a later holiday `/open` returns null. An
  event landing mid-opener makes the opener yield.
- **effects route.** `event` under way, and `null` after.
- **Regression.** New parameters on `build_pde_ctx`, `build_prompt` and
  `build_reply_request` change call forms only. Every `/open` test passes
  unchanged.
- `openapi.json` regenerated.

## 13. Non-goals

- More kinds. The table admits them in place.
- A model call that directs the event, or writes it before the persona does.
- An event changing a need directly.
- A time-of-day factor, a mutual-exclusion matrix beyond "one at a time", or
  per-persona weight overrides.
- Echoing back what the client last showed.
- Writing events into `persona_story_events`.
- Events in voice or group sessions, and an event frame on the text stream.
- An engine-side cap on how often a caller draws.

## 14. Decisions taken

- **A rule director.** The kind is the engine's call; the model only tells
  it, so a draw costs one model call.
- **The event lives on its message**, over an event table: the message is the
  only thing an event produces, and the cooldowns and the event under way
  derive from those rows.
- **`ends_at` is stored**, `started_at` is the row's `sent_at`.
- **A draw always yields an event when a kind is eligible.** How eventful her
  day is follows from when the caller draws.
- **One event at a time** stands in for any mutual-exclusion table.
- **The allowlist is the caller's**, so a persona's world can rule kinds out
  without removing them from the engine.
- **Events reach the needs only through the judge.**
- **An event does not yield to a user message**, unlike an opener.

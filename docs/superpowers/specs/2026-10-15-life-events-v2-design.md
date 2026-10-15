# Life events v2 — Design

- **Date:** 2026-10-15
- **Status:** Draft — ready for review
- **Type:** Engine change. One migration: a new table `engine.persona_events`,
  backfilled from the event rows of `0071`. Additive API changes, plus
  `EventView.message_id` becoming nullable. `eros-engine-core::events`
  breaks: `draw` takes new inputs and returns the duration; `duration(kind)`
  is gone.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — events grouped into four categories, two of
  them days-long backgrounds that other events happen inside; twenty-three
  new kinds; one kind that is silence; three persona settings that shape the
  draw.
- **Builds on:** `2026-10-13-life-events-design.md` (*the v1 spec*). This
  spec replaces its kind table (§3), director (§4), persistence (§5.4–§5.5),
  rendering (§6) and the `event` field on effects (§7). The route, its
  request, the idempotency rule and the in-flight cap stand.

## 1. Motivation

V1 gives the persona five short things that can happen to her, one at a
time. A life also has states that last days — a trip, an illness, her period
— during which small things still happen. And sometimes she just isn't
looking at her phone, and explains later. V2 adds both, and lets each persona
lean towards the events that fit her.

## 2. Decisions

1. **Four categories.** `short` (within an hour) and `mid` (within four
   hours) are *foreground* events: incidents. `body` and `trip` are
   *background* events: states that last days.
2. **A two-stage draw.** The draw picks a category by category weight, then a
   kind within it by kind weight. A category's share does not move when kinds
   are added to another.
3. **One foreground and one background at a time.** A background does not
   block foreground events; a foreground event under way blocks every draw.
4. **Events get their own table.** `away_from_phone` produces no message, so
   an event can no longer live on its message.
5. **`away_from_phone` is silence.** No message when it starts. Every 1:1
   text turn of the instance ghosts while it lasts. Afterwards, the persona's
   next message in a session where the user was ghosted carries the reason.
6. **Persona settings live in `art_metadata`,** beside `gender`, `birthday`
   and `timezone`: `prefer_events`, `health`, `prefer_trip`.
7. **Category and background-ness derive from the kind.** Neither is stored.

## 3. Categories and kinds

### 3.1 Categories

| category | shape | lasts | weight | category cooldown |
|---|---|---|---|---|
| `short` | foreground | ≤ 1 h | 5 | — |
| `mid` | foreground | ≤ 4 h | 4 | — |
| `body` | background | days | 0.5 | 14 d |
| `trip` | background | days | 0.5 | 7 d |

A category cooldown runs from the start of the category's newest event, of
any kind.

### 3.2 Kinds

A range is drawn uniformly in whole hours, inclusive; a range in days,
`lo–hi d`, is `24·lo..=24·hi` hours. A cooldown of `—` is none.

| category | `kind` | label | lasts | weight | cooldown | gate | needs factor |
|---|---|---|---|---|---|---|---|
| short | `police_encounter` | 被警察拦下问话 | 30 min | 1 | 48 h | — | — |
| short | `animal_encounter` | 路上遇到小动物 | 1 h | 3 | 12 h | — | ×2 emotional `high` |
| short | `hit_someone` | 打人了 | 1 h | 0.5 | 14 d | — | — |
| short | `sudden_desire` | 突然很想要 | 1 h | 2 | 24 h | rung ≥ 3 | — |
| short | `grocery_shopping` | 买菜 | 1 h | 2 | 24 h | — | ×2 hunger `high` |
| short | `showering` | 洗澡中 | 30 min | 2 | 12 h | — | — |
| short | `bathing` | 泡澡中 | 1 h | 1 | 48 h | — | ×2 stamina `low` |
| short | `in_class` | 上课中 | 1 h | 1 | 24 h | — | — |
| short | `on_the_way` | 赶路中 | 30 min | 2 | 12 h | — | — |
| mid | `temp_work` | 临时接到工作 | 4 h | 3 | 24 h | — | ×0.5 stamina `low` |
| mid | `ex_message` | 前任发来消息 | 3 h | 1 | 72 h | rung ≥ 2 | ×3 emotional `high` |
| mid | `weather_change` | 天气突然变了 | 4 h | 4 | 8 h | — | — |
| mid | `got_hit` | 被打了 | 4 h | 0.5 | 14 d | — | — |
| mid | `breakup` | 刚分手 | 4 h | 0.5 | 30 d | rung ≥ 2 | — |
| mid | `working_out` | 在健身 | 1–2 h | 2 | 24 h | — | ×0.5 stamina `low` |
| mid | `reading` | 在看书 | 1–3 h | 2 | 12 h | — | — |
| mid | `gaming` | 在打游戏 | 2–4 h | 2 | 12 h | — | — |
| mid | `away_from_phone` | 没看手机 | 1–3 h | 1 | 48 h | — | — |
| mid | `unwell` | 不舒服 | 1–4 h | 1 | 24 h | — | ×2 stamina `low` |
| mid | `irritable` | 烦躁 | 2–4 h | 1 | 24 h | — | ×2 emotional `high` |
| body | `sick` | 生病中 | 1–3 d | 3 | 14 d | — | ×2 stamina `low` |
| body | `period` | 月经中 | 3–7 d | 3 | 28 d | `gender` is `female` | — |
| body | `hospitalized` | 住院 | 2–5 d | 0.5 | 60 d | — | — |
| body | `post_surgery` | 刚做完手术 | 3–7 d | 0.5 | 90 d | — | — |
| trip | `business_trip` | 出差 | 24 h | 3 | — | — | — |
| trip | `short_trip` | 短途旅游 | 48 h | 2 | — | — | — |
| trip | `long_trip` | 长途旅游 | 3–7 d | 1 | — | — | — |
| trip | `hometown_visit` | 回老家 | 72 h | 1 | — | — | — |

- The five v1 kinds keep their weights, cooldowns, gates and factors.
  `police_encounter` now lasts 30 minutes.
- **Gates** read the session's affinity row, as v1 does, and the persona's
  `art_metadata.gender`. A persona without `gender: "female"` never draws
  `period`.
- **Needs factors** scale a kind's weight within its category.
- **The label** is what the judge and the reply read (§8).
- The tables are `const`s in `core/src/events.rs`, retuned in place.

### 3.3 What a background rules out

While `hospitalized` or `post_surgery` is under way, these foreground kinds
are not drawn: `temp_work`, `police_encounter`, `animal_encounter`,
`hit_someone`, `got_hit`, `working_out`, `grocery_shopping`, `in_class`,
`on_the_way`. Every other background rules out nothing.

### 3.4 Cues

A cue states a fact and carries no instruction. `away_from_phone` has none:
nothing is generated when it starts.

| `kind` | cue |
|---|---|
| `police_encounter` | （你刚在外面被警察拦下来问话。） |
| `animal_encounter` | （你刚在路上遇到了一只小动物。） |
| `hit_someone` | （你刚动手打了人。） |
| `sudden_desire` | （你突然很想要。） |
| `grocery_shopping` | （你正在外面买菜。） |
| `showering` | （你准备去洗澡了。） |
| `bathing` | （你正在泡澡。） |
| `in_class` | （你正在上课。） |
| `on_the_way` | （你正在赶路。） |
| `temp_work` | （你刚临时接到一份工作，得马上去处理。） |
| `ex_message` | （你的前任刚给你发来消息。） |
| `weather_change` | （你那边的天气刚刚突然变了。） |
| `got_hit` | （你刚被人打了。） |
| `breakup` | （你刚和交往的人分手了。） |
| `working_out` | （你正在健身。） |
| `reading` | （你正在看书。） |
| `gaming` | （你正在打游戏。） |
| `unwell` | （你现在有点不舒服。） |
| `irritable` | （你现在心里很烦躁。） |
| `sick` | （你生病了，大概要 {span}才会好。） |
| `period` | （你的月经来了。） |
| `hospitalized` | （你刚住进医院，大概要住 {span}。） |
| `post_surgery` | （你刚做完一场手术，大概要恢复 {span}。） |
| `business_trip` | （你刚出发去外地出差，要待 {span}。） |
| `short_trip` | （你刚出发去短途旅行，要玩 {span}。） |
| `long_trip` | （你刚出发去长途旅行，要去 {span}。） |
| `hometown_visit` | （你刚动身回老家，要待 {span}。） |

`{span}` is the drawn duration in the §8.1 form (`24 小时`, `3 天`).

A foreground event drawn during a background opens its cue with the
background's state:

| background | state |
|---|---|
| `sick` | 你正在生病 |
| `period` | 你正在经期 |
| `hospitalized` | 你正在住院 |
| `post_surgery` | 你刚做完手术，还在恢复 |
| `business_trip` | 你正在外地出差 |
| `short_trip` | 你正在短途旅行 |
| `long_trip` | 你正在长途旅行 |
| `hometown_visit` | 你正在老家 |

The cue becomes `（{state}。{the kind's cue without its brackets}）`:
「（你正在外地出差。你刚在路上遇到了一只小动物。）」

The persona fills in the rest — which illness, who hit her, where she went —
from her persona and her world.

## 4. Persona settings

Three keys in `persona_genomes.art_metadata`, read by the engine and written
by whoever authors the persona:

| key | value | effect | absent |
|---|---|---|---|
| `prefer_events` | array of `mid` kinds | each listed kind's weight ×3 within `mid` | no preference |
| `health` | `"weak"` \| `"normal"` \| `"strong"` | `body` category weight ×2 / ×1 / ×0.5 | `normal` |
| `prefer_trip` | `true` \| `false` | `true`: `trip` category weight ×3 | `false` |

- A preference scales a weight; it does not lift a gate. A listed
  `ex_message` still needs rung 2.
- Reads are lenient: an unknown kind, a kind outside `mid`, or a value of the
  wrong type is ignored, and the key reads as absent.
- `health` scales the whole `body` category, `period` included. `period`'s
  28-day cooldown keeps it near monthly either way.

## 5. The director

`eros-engine-core/src/events.rs`:

```rust
pub enum EventCategory { Short, Mid, Body, Trip }
impl EventCategory { pub fn is_background(self) -> bool; }

pub enum EventKind { /* the 28 kinds of §3.2 */ }
impl EventKind { pub fn category(self) -> EventCategory; }

pub enum Health { Weak, Normal, Strong }

pub struct PersonaTraits {
    pub female: bool,
    pub health: Health,
    pub prefer_trip: bool,
    pub prefer_events: Vec<EventKind>,
}

pub struct DrawInput<'a> {
    pub now: DateTime<Utc>,
    pub needs: Option<Needs>,           // carried to now
    pub intimacy_rung: u8,
    pub persona: &'a PersonaTraits,
    pub history: &'a [PastEvent],       // the instance's events, `lookback()` back
    pub allowed: &'a [EventKind],
}

/// Three uniform draws in [0, 1).
pub struct Rolls { pub category: f64, pub kind: f64, pub duration: f64 }

pub struct Drawn { pub kind: EventKind, pub lasts: Duration }

pub fn draw(input: &DrawInput, rolls: Rolls) -> Option<Drawn>;

/// How far back a draw, and every under-way read, looks: the longest of all
/// cooldowns and all maximum durations.
pub fn lookback() -> Duration;

/// The kinds whose category is a background.
pub fn background_kinds() -> Vec<EventKind>;
```

`draw`:

1. A foreground event under way at `now` → `None`.
2. A kind is eligible when all hold:
   - it is in `allowed`;
   - it passes its gate;
   - its newest event started at least one kind cooldown before `now`;
   - its category's newest event started at least one category cooldown
     before `now`;
   - with a background under way: its category is not a background, and the
     background does not rule it out (§3.3).
3. **Category.** Each category with an eligible kind weighs its §3.1 weight
   times its persona factor (§4). `rolls.category` picks one by cumulative
   weight.
4. **Kind.** Each eligible kind in that category weighs its §3.2 weight times
   its needs factor and its `prefer_events` factor. `rolls.kind` picks one.
5. **Duration.** A fixed duration is taken as is. A range of `lo..=hi` whole
   hours yields `lo + ⌊rolls.duration × (hi − lo + 1)⌋` hours, clamped to
   `hi`.
6. No eligible kind → `None`.

A roll of exactly 1.0 lands on the last entry, as in v1. The server reads
`PersonaTraits` from `art_metadata` and rolls with `thread_rng`.

## 6. Storage

### 6.1 Migration `0072_persona_events.sql`

```sql
CREATE TABLE engine.persona_events (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id UUID NOT NULL
                REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    session_id  UUID
                REFERENCES engine.chat_sessions(id) ON DELETE SET NULL,
    event_key   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    started_at  TIMESTAMPTZ NOT NULL,
    ends_at     TIMESTAMPTZ NOT NULL CHECK (ends_at > started_at),
    message_id  UUID
                REFERENCES engine.chat_messages(id) ON DELETE SET NULL
);

CREATE INDEX persona_events_instance_idx
    ON engine.persona_events (instance_id, started_at DESC);
CREATE UNIQUE INDEX persona_events_session_key_uidx
    ON engine.persona_events (session_id, event_key);
CREATE UNIQUE INDEX persona_events_message_uidx
    ON engine.persona_events (message_id) WHERE message_id IS NOT NULL;
```

- **`instance_id`** cascades: the event is part of the instance's life and
  does not outlive it, as `persona_instance_items` does not.
- **`session_id`** is where the event was drawn and told. A deleted session
  does not undo the trip, and its cooldown still counts: `SET NULL`. Because
  it can be null, `instance_id` is stored.
- **`message_id`** is the message that told it; `away_from_phone` has none,
  and a deleted message does not undo the event: `SET NULL`.
- **`kind`** has no `CHECK`: core owns the list, and a read skips a kind it
  does not know.
- Each FK column leads an index: `instance_idx`, `session_key_uidx`,
  `message_uidx`.
- RLS and the `anon` / `authenticated` revokes follow `0067`.

The same migration backfills every v1 event row:

```sql
INSERT INTO engine.persona_events
    (instance_id, session_id, event_key, kind, started_at, ends_at, message_id)
SELECT s.instance_id, m.session_id, m.metadata->>'event_key',
       m.metadata->'event'->>'kind', m.sent_at,
       (m.metadata->'event'->>'ends_at')::timestamptz, m.id
  FROM engine.chat_messages m
  JOIN engine.chat_sessions s ON s.id = m.session_id
 WHERE m.metadata->>'proactive' = 'event'
   AND s.instance_id IS NOT NULL
   AND m.metadata->'event'->>'ends_at' IS NOT NULL;
```

### 6.2 The message row

An event's message keeps the v1 shape — `role = 'assistant'`,
`assistant_action_type = 'proactive'`, `metadata.proactive = 'event'`, the
locale and prompt fields — without `metadata.event` and
`metadata.event_key`.

### 6.3 Contract step, in the following release

V1 rows keep `metadata.event` and `metadata.event_key`, and `0071`'s index
stays, so a rollback image works on them unchanged (§12). Nothing in v2
reads them, and an event never changes after it is written. The release after
this one ships a migration that removes `event` and `event_key` from those
rows' `metadata` and drops `chat_messages_event_key_uidx`.

### 6.4 `EventRepo` (`store/src/events.rs`)

```rust
pub struct EventRecord {
    pub id: Uuid,
    pub kind: EventKind,
    pub session_id: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub message_id: Option<Uuid>,
}

pub struct CurrentEvents {
    pub foreground: Option<EventRecord>,
    pub background: Option<EventRecord>,
    pub at: DateTime<Utc>,
}
```

- **`insert(instance_id, session_id, event_key, kind, started_at, ends_at,
  message: Option<&ProactiveInsert>)`.** One transaction. With a message, the
  message row is written first. The event row is then written under:

  ```sql
  INSERT INTO engine.persona_events (…)
  SELECT …
  WHERE NOT EXISTS (
      SELECT 1 FROM engine.persona_events e
       WHERE e.instance_id = $instance
         AND e.started_at > $started_at - $lookback
         AND e.ends_at > $started_at
         AND ($is_background OR NOT (e.kind = ANY($background_kinds)))
  )
  ON CONFLICT (session_id, event_key) DO NOTHING
  RETURNING *
  ```

  A foreground event waits for no foreground under way; a background event
  waits for nothing under way. `$background_kinds` is
  `events::background_kinds()`, bound as `text[]`. When no event row is
  returned, the transaction rolls back, the message with it, and the route
  reads `event_key`'s row, as v1 does. A written message bumps
  `chat_sessions.last_active_at`.
- **`by_key(session_id, event_key)`.**
- **`recent(instance_id, since)`** — newest first; a row whose `kind` does
  not parse is skipped.
- **`current(instance_id, at_message, now)`** — the foreground and the
  background under way at `at_message`'s `sent_at`, or at `now` without one.
  A re-driven turn sees the same events.
- **`unexplained_away(session_id, instance_id, at)`** — §7.3.

The v1 functions in `store/src/chat.rs` (`insert_event_message`,
`event_message`, `recent_events`, `current_event`, `EventRecord`) are
removed.

Two concurrent draws under different keys can both pass the `NOT EXISTS`
under READ COMMITTED and both land. This millisecond window is accepted, as
in v1.

## 7. `away_from_phone`

### 7.1 Drawn

No model call. The event row is written with `message_id` NULL and
`started_at` = now. The route returns the event with `message: null`.

### 7.2 While it lasts

Every 1:1 text turn of the instance, in any session, whose user row's
`sent_at` falls inside the window ghosts without the judge:

- the user row is marked `ghost_decision = true`;
- `record_ghost` is not called: `ghost_streak` and `last_ghost_at` stay put,
  so the judge's own ghost vetoes and thresholds are untouched;
- the stream yields the ghost `meta`, `done` and `final` frames a judged
  ghost yields; nothing else runs.

An action turn (any user action: a give, a tip, a kiss…) is exempt and takes
the normal path, because an action is always answered; its `正在经历` line still
lists the away stretch.

Which ghosts came from an away window is derivable from the event row and the
ghosted rows' times, so it is not stored.

`POST …/open` returns `greeting: null` while an `away_from_phone` is under
way, before any generation.

### 7.3 Afterwards

In a session `S`, an away event `E` is **unexplained** at moment `at` when:

- `E` is the instance's newest `away_from_phone` with `ends_at ≤ at`;
- `S` holds a user row with `ghost_decision = true` and
  `E.started_at ≤ sent_at < E.ends_at`;
- `S` holds no assistant row with `E.ends_at < sent_at ≤ at`.

While `E` is unexplained, every persona message generated in `S` — a turn's
reply, an `/open` greeting, an event's message — renders the §8.2 `刚才`
line. The first assistant row written after `E.ends_at` ends it.

## 8. Rendering

### 8.1 Spans

| span | renders |
|---|---|
| < 1 h | `不到 1 小时` |
| 1 h – < 48 h | `{h} 小时`, whole hours rounded |
| ≥ 48 h | `{d} 天`, whole days rounded |

### 8.2 Lines

- **Judge.** `[近期事件]` lists the background, the foreground and an
  unexplained away event, in that order, joined by `；`:

  ```
  [近期事件] 长途旅游（2 天前开始，约 3 天后结束）；在看书（不到 1 小时前开始，约 2 小时后结束）
  ```

  Each event under way reads `{label}（{span}前开始，约 {span}后结束）`;
  under an hour reads `不到 1 小时前开始` / `不到 1 小时后结束`. An
  unexplained away event reads `没看手机（{span}前结束，期间对方的消息你没有回）`;
  under an hour reads `不到 1 小时前结束`. The judge addresses the persona as
  `你`, as its other lines do.
- **Reply.** `[character_state]` renders after `此刻的需要`:

  ```
  - 正在经历：长途旅游（约 3 天后结束）；在看书（约 2 小时后结束）
  - 刚才：有段时间没看手机，对方那时发来的消息你现在才看到
  ```

  `正在经历` lists the background and the foreground. `刚才` renders while an
  away event is unexplained (§7.3).
- **Where.** The stream reply, `/open` greetings and event messages all pass
  both lines. Voice and group sessions read no events, as in v1.

## 9. The route and effects

### 9.1 `POST /v2/comp/session/{session_id}/events`

- `kinds` accepts the 28 kinds; absent = all.
- The draw reads `PersonaTraits` from the persona's `art_metadata`, the
  history `lookback()` back, and the under-way events at now.
- Drawn with a message: the v1 generation path, with §3.4's cue and §8.2's
  lines. Drawn `away_from_phone`: §7.1.
- Response, `EventView`:

  ```json
  {"event": {"kind": "business_trip", "category": "trip",
             "started_at": "…", "ends_at": "…", "message_id": "…"},
   "message": {"message_id": "…", "content": "…", "sent_at": "…",
               "occasion": "event"}}
  ```

  `category` is new. `message_id` is nullable. `event` with `message: null`
  is an `away_from_phone`; both null means nothing was drawn, as in v1.

### 9.2 `GET /v2/comp/session/{session_id}/effects`

`event` is the foreground under way now, or `null`. A new field,
`background_event`, is the background under way now, or `null`. An
`away_from_phone` under way is reported like any other foreground event; how
a client shows it is the client's call.

## 10. Interplay

- **Holiday and birthday.** An away event writes no message, so it does not
  count for `has_message_since`; `/open` returns null during it regardless.
  An event with a message still counts, as in v1.
- **Openers.** An event message landing while an opener is generating still
  makes the opener yield. An away event starting mid-opener does not; the
  opener lands. This window is accepted.
- **Needs.** An away-ghosted turn runs no judge and writes no needs rating;
  the levels carry to the next judged turn.
- **Signals.** `compute_signals_for_session` counts user rows, ghosted ones
  included, as for any ghost.
- **World Stories.** Events with a message reach the life story through the
  chat, as in v1. `away_from_phone` does not.

## 11. Where it lives

| change | file |
|---|---|
| categories, kinds, `PersonaTraits`, `draw`, `lookback`, `background_kinds` | `core/src/events.rs` |
| migration | `store/migrations/0072_persona_events.sql` |
| `EventRepo`, `EventRecord`, `CurrentEvents` | `store/src/events.rs` (new), `store/src/lib.rs` |
| v1 event functions removed | `store/src/chat.rs` |
| `PersonaTraits` from `art_metadata` | `server/src/prompt.rs` (beside `meta_str`) |
| cues, background state prefix | `server/src/pipeline/proactive.rs` |
| `/open` returns null during away; lines on greetings | `server/src/routes/session_open.rs`, `pipeline/proactive.rs` |
| the route, `EventView.category`, nullable `message_id` | `server/src/routes/session_events.rs` |
| away ghost, `[近期事件]` | `server/src/pipeline/stream.rs` |
| `正在经历`, `刚才`, spans | `server/src/prompt.rs` |
| `background_event` | `server/src/routes/session_effects.rs` |
| OpenAPI | `crates/eros-engine-server/openapi.json` (regenerated) |

## 12. Rollback and deploy

- **Deploying.** The table builds and backfills at boot. With `kinds` absent,
  the new kinds are drawn from the first draw; a caller that wants to bring
  them in gradually sends an allowlist first.
- **Rolling back.** The previous binary's `migrate` refuses a database that
  holds `0072`, so a rollback deploys the previous image without running it.
  That image ignores the table and keeps reading and writing v1 events in
  `metadata`, which §6.3 leaves in place. Events written by v2 are invisible
  to it: no forced ghosts, no backgrounds. Events it writes during the
  rollback are in `metadata` only, and a later roll-forward does not see
  them.
- **The contract step** (§6.3) ships in the release after this one.

## 13. Testing

- **core.**
  - A foreground under way → `None`.
  - A background under way: `body` and `trip` are not drawn; the §3.3 kinds
    are not drawn under `hospitalized` or `post_surgery`; every other kind is.
  - Kind and category cooldowns, each eligible again exactly one cooldown
    after the start. `trip` once a week across its four kinds.
  - Gates: `ex_message` and `breakup` at rungs 1 and 2; `sudden_desire` at
    rungs 2 and 3; `period` for a female and a non-female persona.
  - A category with no eligible kind takes no part in stage 1.
  - Persona factors (`health` each level, `prefer_trip`, `prefer_events`
    inside and outside `mid`) and every needs factor; none without settings
    or needs.
  - Both stages on each cumulative boundary; a roll of 1.0.
  - Duration rolls at 0 and just under 1 for a range; fixed durations.
  - `lookback()` and `background_kinds()`.
  - Wire values round-trip for every kind.
- **store** (`#[sqlx::test]`).
  - Insert: a foreground waits for a foreground under way, not for a
    background; a background waits for either.
  - A repeated key in one session conflicts; the same key in another session
    does not.
  - A lost event insert leaves no message row.
  - `current` returns one foreground and one background.
  - Backfill: `0071`-shaped rows land with their kind, times and message.
  - A deleted session nulls `session_id`; a deleted message nulls
    `message_id`.
  - `unexplained_away` before and after the first assistant row, with and
    without ghosted rows in the window, and per session.
- **route** (local Postgres, companion call mocked).
  - `away_from_phone`: no model call, `message: null`, the row written.
  - A foreground drawn during a background: the cue opens with the state.
  - A background's cue carries its span.
  - `category` on `EventView`.
  - `/open` during an away event → null, no model call; after it, in a
    session with a ghosted row, the greeting's prompt carries `刚才`.
  - effects: `event` and `background_event`.
  - v1 route tests keep their assertions.
- **stream.**
  - During an away event: no judge call, the row is ghosted, `ghost_streak`
    unchanged.
  - An action turn during an away event is answered and its row is not
    ghosted.
  - After it: the first turn renders `刚才`, the next does not.
  - `[近期事件]` and `正在经历` with a background and a foreground; spans in
    hours and days; a re-driven turn renders the same lines.
- **Regression.** New parameters change call forms only. `openapi.json`
  regenerated.

## 14. Documentation

- `docs/api-reference.md` / `.zh.md`: the category and kind table without
  weights; `away_from_phone` (`message: null`, the ghosted window, `/open`
  null, the explanation); `EventView.category`; nullable `message_id`;
  `background_event`; the three `art_metadata` keys.
- `docs/model-config.md` / `.zh.md`: the `[近期事件]` format.
- The concept sweep (life events, ghosting, `/open` nulls, persona
  `art_metadata` keys) across `docs/`, `examples/` and the READMEs.

## 15. Non-goals

- Events in voice or group sessions.
- An engine-side scheduler: an explanation sent at `ends_at`, or draws the
  engine starts on its own.
- Overlapping backgrounds.
- Exclusions beyond §3.3.
- Per-persona weights, durations or cooldowns beyond the three §4 settings.
- An event changing a need directly.
- Writing events into `persona_story_events`.
- Typed `persona_genomes` columns for the settings.

## 16. Decisions taken

- **Categories by shape.** Short and mid incidents, body and trip states. The
  multi-day body states are backgrounds rather than long foreground events,
  so a week of illness does not freeze every other event.
- **A two-stage draw**, so each category's share is one number.
- **One foreground and one background at a time** stand in for any
  mutual-exclusion matrix, plus the one hospital and surgery list.
- **The weekly trip cap and the other caps are per instance**, like every
  cooldown: the persona has one life across sessions.
- **An event table**, because `away_from_phone` has no message to live on.
  Category and background-ness derive from the kind.
- **`away_from_phone` ghosts outside the judge** and leaves the ghost
  accounting alone: the silence is circumstance, not her reading of the user.
- **Action turns are exempt from the away ghost**: an action is always
  answered.
- **The explanation is derived**, not flagged: ghosted rows in the window and
  no assistant row since.
- **Persona settings in `art_metadata`**, read leniently, as the other
  persona attributes are.
- **The contract step waits a release**, so a rollback keeps v1 events.

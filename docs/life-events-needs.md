# Life events and needs

[English](life-events-needs.md) · [中文](life-events-needs.zh.md)

A persona has a day of her own, and two kinds of state follow it. **Needs** are four
basic levels (hunger, social, stamina, emotional) that the PDE judge rates and the engine
carries forward in time. **Life events** are things that happen to her, drawn when a
caller asks: a trip, an illness, a stray cat, an hour away from her phone. Both reach the
judge and the reply as plain facts in the present tense. Neither tells her how to behave;
the one exception is an event that is silence (`away_from_phone`).

Request and response shapes are in [api-reference.md](api-reference.md), the judge's
context and verdict in
[model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge). This page is
about how the pieces work and how they fit together.

## At a glance

| | Needs | Life events |
|---|---|---|
| What it is | four levels: `hunger`, `social`, `stamina`, `emotional`, each `low` / `mid` / `high` | things that happen in her day, in four categories |
| Written by | the PDE judge, on a judged text turn | a caller's `POST /v2/comp/session/{session_id}/events`; the engine picks the kind |
| Stored in | `metadata.needs` of the rated user row | `engine.persona_events`, plus the message that tells it (none for `away_from_phone`) |
| Scope | one state per persona instance, across all its sessions | the same |
| Time | drifts between ratings, computed on read | `started_at` and `ends_at`; cooldowns count from the start |
| Needs the LLM judge | yes | no |
| Judge sees | the `[需求]` line | the `[近期事件]` line |
| Reply sees | `- 此刻的需要：…` | `- 正在经历：…` and `- 刚才：…` |
| Caller gets | `needs` on the `body_state` frame and on `GET …/effects` | the events route response; `event` and `background_event` on `GET …/effects` |

Voice calls read needs and no events. Group turns read neither. Details follow.

## Needs

### The four needs

| need | `low` | `high` | reply wording, `low` / `high` |
|---|---|---|---|
| `hunger` | not hungry | very hungry | 不饿 / 饿了 |
| `social` | wants no company | wants someone to talk to | 不想跟人打交道 / 想找人说说话 |
| `stamina` | worn out | full of energy | 很累 / 精神很好 |
| `emotional` | content | badly wants to be cared about | 心里挺满足 / 很想被在乎 |

Levels are ordinals. The judge never reads or writes a number, and `mid` is the unremarkable
middle.

### Who rates them

Needs belong to the opt-in LLM judge (`[tasks.pde_decision].filter_prompt` set). Without it
there are no needs: no `[需求]` line, no reply line, `needs: null` on the frame and the
effects route, no needs factor in the event draw, nothing in a voice call.

- **The verdict.** The judge's JSON may carry `needs`: an object with all four keys, or `null`
  for "no change, keep the previous rating". A value that does not parse reads as `null` and
  never fails the verdict. The strict `response_format` requires all four keys inside a
  non-null object.
- **What it rates.** The persona alone, never the relationship; the affinity evaluator rates
  that.
- **Where it is stored.** On the user row of the turn the judge rated, as `metadata.needs`,
  whatever the turn's outcome: reply, image, ghost or product question. The write is awaited
  before the turn goes on. A failed write is logged and the turn carries on with the verdict's
  levels in memory.
- **What writes nothing.** A rule-engine turn (the judge is off or failed), a `needs: null`
  verdict, a turn silenced by an `away_from_phone` stretch (it runs no judge), and every group
  or voice turn.
- **Whose state.** The persona instance's. The newest rated row is read across all the
  instance's sessions, so a second session does not reset her hunger.

The judge's `needs` and the levels it was shown are audited with the rest of the verdict; see
[llm-audit.md](llm-audit.md#pde-judge-payload).

### Drift between ratings

Nothing is stored between ratings. The current state is the newest rating plus the time
since. Each need moves one level toward its resting level for every full period elapsed since
the rating, and stops there:

| need | settles at | one step every |
|---|---|---|
| `hunger` | `high` | 4 h |
| `social` | `high` | 12 h |
| `stamina` | `mid` | 8 h |
| `emotional` | `mid` | 12 h |

Time away makes her hungry and wanting company; tiredness and emotional need settle toward
the middle. A `low` hunger rated at noon reads `mid` at 16:00 and `high` from 20:00. The
constants are in `crates/eros-engine-core/src/needs.rs`.

The moment the rating is carried to depends on the reader:

| reader | carried to |
|---|---|
| a text turn: the judge's line, the reply, the `body_state` frame | the send time of the turn's own user row |
| `GET …/effects`, an event draw | the moment of the call |
| a voice call | the call's first turn, then frozen |

A text turn reads the newest rating strictly before its own row, so a turn that is driven
again shows the judge the same line.

### Where needs show up

- **The judge** gets one `[需求]` line on every judged turn, before `[近期事件]` and
  `[身体状态]`:

  ```
  [需求] 饥饿=高 社交欲=低 体力=中 情感需求=中
  ```

  Before the first rating it reads `[需求] 尚未判定`. The line is never absent, because a
  missing line would read as "all low".
- **The reply** gets the levels that are not `mid`, in the order of the table above, joined
  by `；`, as a line of `[character_state]`:

  ```
  [character_state]（这段关系里目前的状况，不是人设；与上面冲突时以上面为准）
  - 此刻的需要：饿了；很累
  ```

  All four at `mid`, or no rating, adds nothing and leaves the prompt unchanged. A judged
  turn's reply is written with the levels the turn ends with: the verdict's when it rated,
  otherwise the carried levels the judge was shown. A rule-engine turn in a deployment that
  has the judge uses the carried levels.
- **The caller** gets the same levels on the `body_state` frame (`needs`, an object or `null`),
  sent when a give's effect is at work or needs are known, and re-sent on an idempotent
  replay. `GET …/effects` returns them carried to the moment of the call, or `null` with no
  rating or no judge. See
  [the message stream](api-reference.md#post-compchatsession_idmessagestream) and
  [the effects route](api-reference.md#get-v2compsessionsession_ideffects).
- **A voice call** freezes the levels carried to its first turn into the bootstrap snapshot.
  They render as `此刻的需要：…` inside the `[身体状态]` block, and every turn's `body_state`
  frame carries the frozen levels. The call never rates or advances them. See
  [the voice turn](api-reference.md#post-compvoicesession_idturnstream).
- **Not shown to:** `/open` greetings, event messages, image edit and group turns.

The engine acts on needs in one place: the weights of the event draw (below). No ghost rule
and no image rule reads them. A tired persona is not ghosting-prone by rule; the judge sees
the levels and decides.

## Life events

### Categories

An event is one of 28 kinds. Each kind belongs to a category, and the category sets its
shape:

| `category` | shape | lasts | category weight | category cooldown |
|---|---|---|---|---|
| `short` | incident | up to 1 h | 5 | none |
| `mid` | incident | up to 4 h | 4 | none |
| `body` | background | 1 to 7 days | 0.5 | 14 days |
| `trip` | background | 1 to 7 days | 0.5 | 7 days |

An **incident** is a foreground event: she is called in to work, an ex texts, she is reading,
in the shower, not looking at her phone. A **background** is a state that lasts days (a trip,
an illness, her period) and incidents still happen inside it. The kinds, their durations and
what each means are in
[the events route](api-reference.md#post-v2compsessionsession_idevents). Per-kind weights,
cooldowns and ranges are in `rule`, and the category values in `category_rule`, in
`crates/eros-engine-core/src/events.rs`.

A duration written as a range is drawn uniformly in whole hours when the event starts.

### The draw

The draw is a pure function of the inputs plus three random numbers (one per stage, one for
the length).

1. **Eligibility.** A kind is eligible when all of these hold:
   - it is in the caller's `kinds` (absent means all 28; `[]` means none);
   - it passes its gate (below);
   - its own cooldown has elapsed;
   - its category's cooldown has elapsed;
   - the background rules allow it (below).
2. **Category.** Each category with at least one eligible kind weighs its category weight
   times its persona factor. A roll picks one by cumulative weight.
3. **Kind.** Each eligible kind in that category weighs its base weight times its
   [needs factor](#1-needs-weight-the-kind-pick) times its `prefer_events` factor. A second
   roll picks one.
4. **Duration.** A fixed length is taken as is; a range is rolled.

No eligible kind means no event: the response is `{"event": null, "message": null}`. Whenever
some kind is eligible, the draw yields an event. How eventful her day is follows from when the
caller asks and from the cooldowns.

Because the draw is in two stages, a category's share does not depend on how many kinds it
holds. With every category eligible and no persona settings, a draw lands in `short` half the
time, in `mid` 40 % of the time, and in `body` and `trip` 5 % each. A category with no
eligible kind drops out of the first stage and the others share its weight.

### One incident and one background at a time

- While an incident is under way no draw happens at all, not even for a background.
- While a background is under way, no second background is drawn. Incidents still are, except
  that during `hospitalized` and `post_surgery` the ones that need her out and about are not:
  `temp_work`, `police_encounter`, `animal_encounter`, `hit_someone`, `got_hit`,
  `working_out`, `grocery_shopping`, `in_class`, `on_the_way`.
- One incident and one background can run together. An incident drawn inside a background is
  told from inside it.
- This is per persona instance across all its sessions: she has one life. A draw reads the
  instance's events of the last 90 days, the longest cooldown there is.

The write enforces the same rule atomically: an incident is not written while another incident
is under way, and a background is not written while any event is.

### Cooldowns and caps

- **Per kind.** A kind cannot start again until its cooldown has elapsed since its newest event
  started, not ended. Cooldowns run from 8 hours (`weather_change`) up; the heavy kinds are
  long (`hit_someone` 14 days, `breakup` 30, `hospitalized` 60, `post_surgery` 90). The four
  `trip` kinds have none of their own; the category cooldown covers them.
- **Per category.** `body` cannot start more than once a fortnight and `trip` more than once
  a week, whichever kinds are involved. `short` and `mid` have none.
- **Per key.** The same `event_key` in the same session returns the same event with no draw
  and no generation.
- **Not capped.** The engine sets no limit on how often a caller may ask. Frequency is the
  cooldowns plus the caller's own policy. A draw that generates a message takes a slot under the
  per-user in-flight cap that every LLM entry point shares (`429` at the cap).

### Gates

A gate decides whether a kind can be drawn at all. A preference or a needs factor scales a
weight and never lifts a gate.

| kind | gate |
|---|---|
| `ex_message`, `breakup` | intimacy rung 2 or higher |
| `sudden_desire` | intimacy rung 3 |
| `period` | the persona's `art_metadata.gender` is `"female"` |
| `away_from_phone` | the ghost protections allow it (see [below](#away_from_phone)) |

The rung (1 to 3) is read from the session's affinity, time-decayed as in a chat turn; see
[affinity-model.md](affinity-model.md).

### Persona settings

Three keys in the persona genome's `art_metadata`, read by the engine and written by whoever
authors the persona, sit beside `gender`, `birthday` and `timezone`:

| key | value | effect |
|---|---|---|
| `prefer_events` | array of `mid` kinds | each listed kind weighs 3 times as much within `mid` |
| `health` | `"weak"`, `"normal"` (default), `"strong"` | the `body` category weighs 2 times, 1 time, half as much |
| `prefer_trip` | `true`, `false` (default) | the `trip` category weighs 3 times as much |

Reads are lenient. A value of the wrong type reads as absent; in `prefer_events`, an entry that
is not a known `mid` kind is dropped and the rest are kept. `health` scales the whole `body`
category, `period` included, and `period`'s own 28-day cooldown keeps it near monthly either
way. `prefer_trip` raises the odds of a trip, not its frequency: the weekly category cooldown
holds.

The caller's `kinds` allowlist is the other lever: it is how a persona's world rules a kind
out without the engine losing it.

### Asking for an event

The caller decides when to ask and the engine decides whether and what. Nothing starts a draw
on its own, and nothing is sent when an event ends. Each draw carries an `event_key` (scoped to
the session, so a retry is safe), an optional `kinds` allowlist, and the locale, tier and scope
fields a chat turn would carry, because the engine does not keep them between calls. The
response is the event and the message that tells it, or both `null`. A group or voice session
is refused with `409 wrong_channel`.

### How she tells it

Every kind except `away_from_phone` is told in one proactive message. It is generated by the
same companion model path as a reply (persona, history window, memory recall, `[now]`), with
no judge, no streaming and no user row. The engine adds a stage cue as the final `user` turn
of the request. The cue is never persisted. It states a fact and carries no instruction:

```
（你刚临时接到一份工作，得马上去处理。）
```

She fills in the rest (what work, what the ex wrote, which animal) from her persona and her
world. Two cue variants:

- A background's cue carries its drawn length (`period`'s does not):
  `（你刚出发去外地出差，要待 24 小时。）`.
- An incident drawn during a background opens with the background's state:
  `（你正在外地出差。你刚在路上遇到了一只小动物。）`.

Then:

- Output regex rules apply as on any reply. A blank reply, or one the rules strip to empty,
  writes nothing and the response's `event` and `message` are both `null`. A failed generation returns the upstream error
  and writes nothing.
- The event row and the message are written in one transaction. The message's `sent_at` is
  the event's `started_at`, one instant taken after generation; `ends_at` is that instant plus
  the drawn length. If the write loses to an event already under way, the message goes with it.
- The message is an ordinary assistant row with `assistant_action_type = 'proactive'` and
  `metadata.proactive = 'event'`, unread until the read call. It runs no post-process: no
  memory write, no affinity event, no change to the ghost streak. It bumps the session's
  `last_active_at`.
- **The user does not win a race.** An event does not yield to a user message that lands while
  it is generated: it happened whether or not the user spoke.

### While it lasts

The events under way at a moment are the background and the incident whose
`started_at <= moment < ends_at`. They reach three readers.

- **The judge** gets one `[近期事件]` line, after `[需求]` and before `[身体状态]`. It lists the
  background, then the incident, joined by `；`, each as
  `{label}（{span}前开始，约 {span}后结束）`:

  ```
  [近期事件] 长途旅游（2 天前开始，约 3 天后结束）；在看书（不到 1 小时前开始，约 2 小时后结束）
  ```

  A span under one hour reads `不到 1 小时` (and the end then drops the `约`); under 48 hours it
  is whole hours, rounded; from 48 hours it is whole days, rounded. `{label}` is a fixed
  Chinese name per kind (`reading` is `在看书`, `long_trip` is `长途旅游`). The line is absent
  when nothing is under way and no away stretch is unexplained (below).
- **The reply** gets `- 正在经历：` in `[character_state]`, after `此刻的需要`, listing the same
  two events with only the end:

  ```
  - 正在经历：长途旅游（约 3 天后结束）；在看书（约 2 小时后结束）
  ```

  This works with or without the LLM judge. `/open` greetings and event messages carry the same
  line. Voice, group and image-edit requests read no events.
- **The caller** gets them from `GET …/effects`: `event` is the incident under way, and
  `background_event` the background. An `away_from_phone` under way is reported as `event`
  like any incident; how a client shows it is its call.

A text turn reads the events as of its own user row's send time, so a turn that is driven
again sees the same ones.

### `away_from_phone`

The one kind that is silence. Nothing is generated when it starts.

**Drawn.** No model call. The event row is written with `message_id` NULL and `started_at` at
the moment of the call, lasting 1 to 3 hours. The response carries the event with
`"message": null`. It is drawn only where the persona may go silent, so the draw applies two
gates borrowed from the ghost protections. A gated draw leaves `away_from_phone` out of the
allowed kinds and the others draw as usual:

- not while the deployment's ghosting switch is off (`ghosting = false` on
  `[tasks.pde_decision]`);
- not in a session with fewer than 10 user messages, the floor `ghost_permitted` enforces.

The gates apply to the draw, in the session the caller draws in. A stretch that exists
silences the persona's turns in every session.

**While it lasts.** Every 1:1 text turn of the persona, in any session, ghosts without the
judge and without the ghost score:

- The user's row is marked ghosted. It carries no read receipt, because no judge read it, and
  still carries none after she explains.
- `record_ghost` is not called: `ghost_streak`, `total_ghosts` and `last_ghost_at` stay put,
  and the judge's own ghost vetoes are untouched. The silence is circumstance, not her reading
  of the user.
- The turn is three frames, `meta(ghost)`, `done`, `final`. No judge ran, so there is no
  `reasoning` frame and no `body_state` frame, and no needs are rated.
- `/open` returns `greeting: null` without generating. The check happens before generation; a
  stretch that starts while a greeting is being generated does not stop it.

Two kinds of turn are exempt and take the normal path:

- **An action turn** (a give, a tip, a kiss and the rest). An action is interaction in person,
  which needs no phone, so it is answered. Its `正在经历` line still lists the away stretch.
- **A forced-image turn** (`image.force = true`), which is always an image.

With the ghosting switch off, a stretch that is under way silences nothing; turns take the
normal path and the `正在经历` line still lists it. The rest of the mechanics are in
[ghost-mechanics.md](ghost-mechanics.md#ghosting-the-engine-imposes).

**Afterwards.** The explanation is derived, not flagged. In a session `S`, an away event `E`
is unexplained at a moment when:

- `E` is the instance's newest `away_from_phone` that has ended, within the last 90 days;
- `S` holds a user row that was ghosted inside `E`'s window;
- `S` holds no assistant row written since `E` ended.

While `E` is unexplained, every persona message generated in `S` carries the line below: a
turn's reply, an `/open` greeting or an event message. The judge's `[近期事件]` line ends with
`没看手机（2 小时前结束，期间对方的消息你没有回）`. The first assistant row written after `E`
ended closes it. A reply the persona wrote inside the window (an action turn) does not.

```
- 刚才：有段时间没看手机，对方那时发来的消息你现在才看到
```

### Next to `/open`

An event's message counts as a message for the `/open` birthday and holiday check, so neither
greeting follows it on the same local day. An opener that is being generated when an event
message lands yields and returns `null`; an event does not yield to an opener. A greeting
carries the `正在经历` and `刚才` lines and no needs line, and `/open` returns `null` while
`away_from_phone` is under way.

### Storage

- `engine.persona_events` holds one row per event: `instance_id` (cascades with the instance),
  `session_id` (where it was drawn and told; set to NULL if the session goes), `event_key`
  (unique within the session), `kind`, `started_at`, `ends_at` and `message_id` (the message
  that tells it; NULL for `away_from_phone`, and set to NULL if the message goes). Category and
  background-ness derive from the kind and are not stored. A row whose kind the build does not
  know is skipped on read.
- The message is a row of `engine.chat_messages` with `metadata.proactive = 'event'`; the
  event's kind and times live in `persona_events`.
- Needs live on the rated user row, as `metadata.needs`, with a partial index
  (`chat_messages_needs_idx`) that keeps the newest-rating read an index probe per session.

## How the two interact

### 1. Needs weight the kind pick

A needs factor scales a kind's weight within its category, in the second stage of the draw. It
steers which kind comes up, never whether a category does. The factors, from
`needs_factor` in `crates/eros-engine-core/src/events.rs`:

| need level | kinds | factor |
|---|---|---|
| `stamina` is `low` | `temp_work`, `working_out` | ×0.5 |
| `stamina` is `low` | `unwell`, `sick`, `bathing` | ×2 |
| `hunger` is `high` | `grocery_shopping` | ×2 |
| `emotional` is `high` | `ex_message` | ×3 |
| `emotional` is `high` | `animal_encounter`, `irritable` | ×2 |

`social` weights nothing. The factors apply only when the deployment runs the judge and the
persona has a rating, carried to the moment of the call; otherwise every factor is 1. A
factor is not a gate: a kind weighted ×3 still has to be eligible.

### 2. Events never change a stored need

Nothing writes a need from an event. The judge reads the `[近期事件]` line on its next turn
and rates the needs itself with the event in view; the reference judge prompt in
`examples/model_config.toml` tells it to start from `[需求]` and to take the conversation,
`[近期事件]`, `[身体状态]` and the elapsed time into account. If the judge abstains
(`needs: null`), the previous rating keeps drifting whatever happened. Needs keep one
source: the judge.

### 3. The away stretch writes no rating

A turn silenced by `away_from_phone` runs no judge, so it rates nothing. The last rating keeps
drifting through the stretch, and the first judged turn afterwards is shown it carried across
all of it: after a long stretch she reads hungrier and lonelier than before.

Put together, the two form a loop with the judge in the middle: a hungry persona is likelier
to be out buying groceries; the event reaches the judge's context; the judge rates hunger
again.

## Without the LLM judge

| | Without `[tasks.pde_decision].filter_prompt` |
|---|---|
| Needs | none: no rating, no lines, `needs: null`, no draw factors |
| The draw | works, with no needs factors |
| An event's message | generated and told as usual |
| `正在经历` and `刚才` | reach replies, `/open` greetings and event messages |
| `[近期事件]` | not rendered: there is no judge to read it |
| `away_from_phone` | silences turns as with the judge, since the check runs before it: the ghosting switch and the action and forced-image exemptions apply |

## Code pointers

- `crates/eros-engine-core/src/needs.rs`: the levels, the drift constants, `carried`.
- `crates/eros-engine-core/src/events.rs`: categories and kinds, per-kind rules, gates, the
  needs and persona factors, `draw`, `lookback`.
- `crates/eros-engine-server/src/routes/session_events.rs`: the events route, the draw's
  inputs, the `away_from_phone` gates.
- `crates/eros-engine-server/src/pipeline/proactive.rs`: the event message, its cue and its
  write.
- `crates/eros-engine-server/src/pipeline/stream.rs::run_stream`: the needs read and write, the
  `[需求]` and `[近期事件]` lines (`build_pde_ctx`), the away branch ahead of the judge,
  `body_state_frame`.
- `crates/eros-engine-server/src/prompt.rs`: `needs_line`, `events_judge_text`,
  `events_reply_text`, `persona_traits`, and the `[character_state]` lines.
- `crates/eros-engine-server/src/routes/session_effects.rs`: `GET …/effects`.
- `crates/eros-engine-server/src/pipeline/voice.rs`: the bootstrap's needs.
- `crates/eros-engine-store/src/events.rs::EventRepo`: `insert`, `by_key`, `recent`, `current`,
  `unexplained_away`.
- `crates/eros-engine-store/src/chat.rs::ChatRepo`: `set_needs` and `latest_needs`.
- `crates/eros-engine-store/migrations/0070_chat_messages_needs_idx.sql` and
  `0072_persona_events.sql`: the index and the table.

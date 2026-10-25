# Life events and needs

[English](life-events-needs.md) · [中文](life-events-needs.zh.md)

A persona has a day of her own, and two kinds of state follow it. **Needs** are four
basic levels (hunger, social, stamina, emotional) that the PDE judge rates and the engine
carries forward in time. **Life events** are things that happen to her, drawn when a
caller asks: a trip, an illness, a stray cat, an hour away from her phone. Both reach the
judge and the reply as plain facts in the present tense. Neither tells her how to behave;
the exception is an event that is silence (the offline kinds). A third piece sits on top of
both: when a relationship has worn thin she can ask for a break, the first *life choice*
([the ask-for-space choice](#the-ask-for-space-choice)).

Request and response shapes are in [api-reference.md](api-reference.md), the judge's
context and verdict in
[model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge). This page is
about how the pieces work and how they fit together.

## At a glance

| | Needs | Life events |
|---|---|---|
| What it is | four levels: `hunger`, `social`, `stamina`, `emotional`, each `low` / `mid` / `high` | things that happen in her day, in four categories |
| Written by | the PDE judge, on a judged text turn | a caller's `POST /v2/comp/session/{session_id}/events`; the engine picks the kind |
| Stored in | `metadata.needs` of the rated user row | `engine.persona_events`, plus the message that tells it (none for an offline kind she does not announce) |
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
  verdict, a turn silenced by an offline stretch (it runs no judge), and every group
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
  rating or no judge, and its `changes_at` says when the response next changes on its own: a
  need's next step or the end of an event under way; `as_of` is the instant it was computed
  for, so a client can count the wait as a duration. See
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

An event is one of 50 drawable kinds. Each kind belongs to a category, and the category sets its
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

Two more kinds are never drawn: `taking_space` (说好暂时分开) and `fell_out`
(为分开的事闹得不愉快). The engine writes them itself when she asks for space and the
user answers or ignores her ([the ask-for-space choice](#the-ask-for-space-choice)). They are
`mid` incidents by shape, and `taking_space` is the one that outlasts the category's four
hours: it lasts 12 to 24. They have no draw rule and the caller's `kinds` rejects them.

### The draw

The draw is a pure function of the inputs plus three random numbers (one per stage, one for
the length).

1. **Eligibility.** A kind is eligible when all of these hold:
   - the caller's `kinds` allows it ([below](#the-callers-kinds); absent means all 50);
   - it passes its gate (below);
   - its own cooldown has elapsed;
   - its category's cooldown has elapsed;
   - for an offline kind, the shared offline cooldown has elapsed (below);
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
  `working_out`, `grocery_shopping`, `in_class`, `on_the_way`, `in_battle`, `got_bumped`,
  `bumped_someone`, `saw_bag`, `saw_clothes`, `out_shopping`, `bros_gathering`,
  `sis_gathering`, `admiring_scenery`, `admiring_art`, `watching_show`, `on_the_road`,
  `having_sex`. During `period`,
  `period_scare` and `pregnancy_scare` are not drawn.
- One incident and one background can run together. An incident drawn inside a background is
  told from inside it.
- While she waits for the user's answer to her ask for space, no draw happens either, in any
  session of the persona: the ask is the foreground drama, and it keeps an offline stretch from
  starting mid-question. A repeat of an `event_key` that already drew still returns its event.
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
- **Shared by the offline kinds.** For 12 hours after any offline kind started, no offline kind
  is eligible, whichever kind it was; each kind's own cooldown still applies. It bounds how often
  she goes silent, the way the category cooldowns bound trips and body states. An accepted
  separation starts it too, and `out_shopping` counts as an offline kind only for a persona it
  takes offline.
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
| `period`, `saw_bag`, `saw_clothes` | the persona's `art_metadata.gender` is `"female"` |
| `period_scare` | the persona's `art_metadata.gender` is `"female"`, and intimacy rung 2 or higher |
| `pregnancy_scare` | the persona's `art_metadata.gender` is `"female"`, and intimacy rung 3 |
| `away_from_phone`, `busy`, `in_battle`, `asleep`, `bros_gathering`, `sis_gathering`, `having_sex`, and `out_shopping` for a female persona | the ghost protections allow it (see [below](#offline-kinds)) |

`out_shopping` and the two gatherings have no gender gate: [Shopping and
gatherings](#shopping-and-gatherings) says who goes offline and why both genders draw both
gatherings.

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

### The caller's `kinds`

The caller's `kinds` is the other lever: it is how a persona's world, or a deployment's
policy, rules a kind out without the engine losing it.

| `kinds` | kinds that may be drawn |
|---|---|
| absent or `null` | all 50 |
| `[]` | none: the response is `{"event": null, "message": null}` |
| `["reading", "quarrel"]` | the listed ones |
| `{"allow": […], "disallow": […]}` | what `allow` allows, minus what `disallow` lists |

In the object, `allow` absent or `null` allows every kind and `[]` none; `disallow` absent,
`null` or `[]` removes nothing; a kind in both is not drawn. `{"disallow": […]}` alone rules
kinds out without freezing the rest, so a kind the engine adds later is drawn too. An unknown
key in the object or an unknown kind anywhere rejects the request, so a misspelt `disallow`
cannot quietly serve a kind it meant to exclude; a caller lists a new kind only once the
engine knows it. `taking_space` and `fell_out` are never drawn, so naming one anywhere in
`kinds` is rejected the same way.

Three kinds carry adult themes: `sudden_desire`, `having_sex`, and `pregnancy_scare`, which implies sex. The
engine keeps no group of them; a caller that serves them only to some users lists them under
`disallow`. `scolded_by_superior` and `scolded_superior` assume someone above her, read in her
world as a boss, a manager, a master or a sovereign; a persona with no one above her is a
case for `disallow` too.

### Asking for an event

The caller decides when to ask and the engine decides whether and what. Nothing starts a draw
on its own, and nothing is sent when an event ends. Each draw carries an `event_key` (scoped to
the session, so a retry is safe), an optional `kinds` filter, and the locale, tier and scope
fields a chat turn would carry, because the engine does not keep them between calls. The
response is the event and the message that tells it, or both `null`. A group or voice session
is refused with `409 wrong_channel`.

### How she tells it

Every kind is told in one proactive message, except `away_from_phone`, `asleep`, `having_sex`, the two
gatherings, and `busy`, `in_battle` and a female persona's `out_shopping` below intimacy rung 2
([below](#offline-kinds)). It is generated by the
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
`started_at <= moment < ends_at`. They reach three readers in the chat; with the world system
on, the story and world directors read them too ([Beyond the chat](#beyond-the-chat)).

- **The judge** gets one `[近期事件]` line, after `[需求]` and before `[身体状态]`. It lists the
  background, then the incident, joined by `；`, each as
  `{label}（{span}前开始，约 {span}后结束）`:

  ```
  [近期事件] 长途旅游（2 天前开始，约 3 天后结束）；在看书（不到 1 小时前开始，约 2 小时后结束）
  ```

  A span under one hour reads `不到 1 小时` (and the end then drops the `约`); under 48 hours it
  is whole hours, rounded; from 48 hours it is whole days, rounded. `{label}` is a fixed
  Chinese name per kind (`reading` is `在看书`, `long_trip` is `长途旅游`). The line is absent
  when nothing is under way, no offline stretch is unexplained, no separation awaits its return
  mark and no false alarm is unresolved (below).
- **The reply** gets `- 正在经历：` under `[now]`, after the time, holiday and birthday lines,
  listing the same two events with only the end. The `- 刚才：` line below follows it. Both sit
  outside `[character_state]`, whose header defers to the blocks above it, so a story digest
  written before an event started does not override the event:

  ```
  - 正在经历：长途旅游（约 3 天后结束）；在看书（约 2 小时后结束）
  ```

  This works with or without the LLM judge. `/open` greetings and event messages carry the same
  line. Voice, group and image-edit requests read no events.
- **The caller** gets them from `GET …/effects`: `event` is the incident under way, and
  `background_event` the background. An offline kind under way is reported as `event`
  like any incident, with `"offline": true`; how a client shows it is its call.

A text turn reads the events as of its own user row's send time, so a turn that is driven
again sees the same ones.

### Offline kinds

Eight drawable kinds make a persona unreachable while they last: `away_from_phone`, `busy`,
`in_battle`, `asleep`, `bros_gathering`, `sis_gathering`, `having_sex` and, for a female persona only,
`out_shopping`. Offline is how she behaves while the event lasts, not a category:
`busy` is `mid` by length and offline by behaviour. They share the same gates, the same
silence, the same exemptions and the same explanation afterwards; they differ in wording and
in whether she announces them. `in_battle` is worded so that her world decides what it is: a
modern persona reads it as a hard negotiation, a business dinner or a difficult matter, a
historical or fantasy persona as a real fight. The engine's own `taking_space` is offline for
every persona and rides the same machinery ([the ask-for-space
choice](#the-ask-for-space-choice)). The kinds and their durations are in
[the events route](api-reference.md#post-v2compsessionsession_idevents).

**Announced or silent.** Which kinds are offline, and which are told when they start:

| kind | who goes offline | told |
|---|---|---|
| `away_from_phone`, `asleep` | every persona | never |
| `busy`, `in_battle` | every persona | from intimacy rung 2; silent below |
| `out_shopping` | a female persona; any other persona keeps replying | a female persona from rung 2 and silent below; any other persona at every rung, as an ordinary incident |
| `bros_gathering`, `sis_gathering` | every persona | never; she explains afterwards |
| `having_sex` | every persona | never; she explains afterwards, in as many words (`在做爱，对方那时发来的消息你现在才看到`); who she was with is hers to say |
| `taking_space` | every persona | not by the draw; the ask-for-space flow has her say goodbye |

At intimacy rung 2 and up, `busy`, `in_battle` and a female persona's `out_shopping` are told in
one proactive message like any other kind, with a stage cue that states the fact:

```
（你接下来要忙一阵，这段时间顾不上看手机。）
（你接下来要去打一场硬仗，一段时间顾不上手机。）
（你接下来要出门逛街，这段时间顾不上看手机。）
```

Below rung 2 she goes quiet without a word. A blank or failed announcement writes nothing, as
for any told kind.

**Drawn.** For a kind she does not announce there is no model call. The event row is written
with `message_id` NULL and `started_at` at the moment of the call, and the response carries the
event with `"message": null`. Whether `out_shopping` is offline depends on the persona, and
everything that follows from offline-ness follows that answer: for a persona it does not take
offline it has no gates, no silence and no explanation afterwards. An offline kind is drawn
only where the persona may go silent, so the draw applies two gates borrowed from the ghost
protections. A gated draw leaves the offline kinds out of the allowed kinds and the others draw
as usual:

- not while the deployment's ghosting switch is off (`ghosting = false` on
  `[tasks.pde_decision]`);
- not in a session with fewer than 10 user messages, the floor `ghost_permitted` enforces.

The gates apply to the draw, in the session the caller draws in. A stretch that exists
silences the persona's turns in every session. The offline kinds also share one cooldown
(see [Cooldowns and caps](#cooldowns-and-caps)).

**While it lasts.** Every 1:1 text turn of the persona, in any session, ghosts without the
judge and without the ghost score. An announced `busy`, `in_battle` or `out_shopping` still ghosts.

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
  which needs no phone, so it is answered. Its `正在经历` line still lists the offline stretch.
- **A forced-image turn** (`image.force = true`), which is always an image.

With the ghosting switch off, a stretch that is under way silences nothing; turns take the
normal path and the `正在经历` line still lists it. The rest of the mechanics are in
[ghost-mechanics.md](ghost-mechanics.md#ghosting-the-engine-imposes).

**Afterwards.** The explanation is derived, not flagged. In a session `S`, an offline event
`E` is unexplained at a moment when:

- `E` is the instance's newest event that has ended, within the last 90 days, of any kind that
  takes this persona offline;
- `S` holds a user row that was ghosted inside `E`'s window;
- `S` holds no assistant row with text written since `E` ended (an image-only reply does not
  count).

While `E` is unexplained, every persona message generated in `S` carries the line below: a
turn's reply, an `/open` greeting or an event message. Its text is the kind's own: what kept
her away, and that she only now sees what was sent. The judge's `[近期事件]` line ends with
`{label}（{span}前结束，期间对方的消息你没有回）`, such as
`睡着了（2 小时前结束，期间对方的消息你没有回）`. The first assistant row with text written
after `E` ended closes it; an image-only reply does not. A reply the persona wrote inside the window (an action turn) does not, and
neither does the announcement of `busy`, `in_battle` or `out_shopping`: it was written when the
event started, before it ended. The text of every kind is in
[model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge).

```
- 刚才：睡着了，对方那时发来的消息你现在才看到
```

The gatherings are never announced, so this is where she accounts for them:
`去了兄弟聚会，对方那时发来的消息你现在才看到` and `去了姐妹聚会，对方那时发来的消息你现在才看到`,
with the judge's suffix `兄弟聚会（{span}前结束，期间对方的消息你没有回）` and its twin. A female
persona's `out_shopping` reads `在逛街，对方那时发来的消息你现在才看到`. The engine's separation
has two wordings of its own ([below](#the-return)).

### False alarms

`period_scare` and `pregnancy_scare` are two `short` kinds, an hour long, that turn out to be
nothing. They are gated on a female persona (`period_scare` from intimacy rung 2,
`pregnancy_scare` at rung 3), and neither is drawn during `period`. She tells each as the
scare itself, in one proactive message:

```
（你发现月经推迟了好几天还没来，心里有点慌。）
（你刚验了孕，验出两条杠，整个人都懵了。）
```

**While it lasts.** `正在经历` and `[近期事件]` carry the label only (`月经没来`,
`验出怀孕`). Nothing in any prompt says it is a false alarm: she does not know yet. She is not
offline and answers as usual.

**Afterwards.** The resolution is derived, not flagged. In a session `S`, a false alarm `F`
is unresolved at a moment when:

- `F` is the newest `period_scare` or `pregnancy_scare` told in `S` that has ended, within the
  last 90 days;
- `S` holds no assistant row with text written since `F` ended (an image-only reply does not
  count).

While `F` is unresolved, every persona message generated in `S` carries the line below, and
the judge's `[近期事件]` line ends with `月经没来（{span}前结束，结果是虚惊一场）` (or the
same with `验出怀孕`). The first assistant row with text written after `F` ended closes it. Her other
sessions never carry it: she told it in one.

```
- 刚才：以为月经没来，后来来了，是虚惊一场
```

When an unexplained offline stretch and an unresolved false alarm meet in the same session,
both texts share the one line, the offline text first, joined by `；`, and the judge gets both
items in the same order:

```
- 刚才：睡着了，对方那时发来的消息你现在才看到；以为月经没来，后来来了，是虚惊一场
```

### Shopping and gatherings

Five kinds give her an afternoon of her own and, for a close relationship, something to want:

| kind | label | category | weight | cooldown | lasts | gate |
|---|---|---|---|---|---|---|
| `saw_bag` | 看中了一个包 | `short` | 2 | 48 h | 1 h | female |
| `saw_clothes` | 看中了一件衣服 | `short` | 2 | 48 h | 1 h | female |
| `out_shopping` | 在逛街 | `mid` | 2 | 24 h | 2 to 4 h | none |
| `bros_gathering` | 兄弟聚会 | `mid` | 1 | 72 h | 2 to 4 h | none |
| `sis_gathering` | 姐妹聚会 | `mid` | 1 | 72 h | 2 to 4 h | none |

The female gate reads `art_metadata.gender`, the key `period` already uses. No needs factor
touches them, and the three `mid` kinds join `prefer_events`' vocabulary like any `mid` kind.
All five are ruled out during `hospitalized` and `post_surgery`: she is not out and about.

**The finds.** `saw_bag` and `saw_clothes` are ordinary told incidents. At intimacy rung 1 the
find is daily-life texture and nothing more. From rung 2 she is allowed to want it out loud,
and a persona with a `tip_personality` says so in that register, the same one the tip action
closes on. Whether anything comes of the wanting is the downstream's business; the engine only
sets the scene. The stage cue states the fact, and `{p}` is the persona's `tip_personality`,
trimmed. The cues of `saw_bag`:

| intimacy rung | cue |
|---|---|
| 1 | `（你在逛街，随手看到一个挺好看的包。）` |
| 2 and up, no `tip_personality` | `（你在逛街，看到一个特别喜欢的包，很想要。）` |
| 2 and up, with one | `（你在逛街，看到一个特别喜欢的包，很想要。代入你「{p}」的打赏人格，把这份心动告诉对方。）` |

`saw_clothes` is the same with 一件 and 衣服 where `saw_bag` has 一个 and 包.

**`out_shopping`.** Offline only for a female persona; any other persona wanders the shops with
her phone in hand and keeps replying. A female persona announces it from rung 2 and goes quiet
without a word below it (no model call, `message_id` NULL, `"message": null`). A persona it does
not take offline tells it at every rung:

```
（你正在外面逛街。）
```

**The gatherings.** Both genders draw both `bros_gathering` and `sis_gathering`, at the same
weight, and there is no gender factor anywhere in the draw. This is deliberate: it reads as
equality, and the collisions are content, since a male persona at a 姐妹聚会 is exactly what a
user asks about. Both gatherings are offline for every persona and never announced: she is with
her people and not looking at her phone, and explains afterwards through the unexplained-offline
line above. A deployment that wants none of this lists them under `disallow`, like any kind.

**Why offline events exist.** A persona who goes dark has a life of her own and does not orbit
the user. A user whose companion is away at a gathering has a reason to talk to another
character.

### Sights, shows and a selfie

Six told incidents give her an afternoon the user hears about: `admiring_scenery` and
`admiring_art` (`short`, 1 h), `taking_selfie` (`short`, 30 min), `on_the_road` (`mid`, 2 to
4 h: a long commute, a coach, a train, a flight; `on_the_way` stays the 30-minute errand),
`watching_match` and `watching_show` (`mid`, 2 to 3 h). She keeps replying through all of
them. The cue states the fact and the details are hers: which view, which piece, which
sport, which vehicle. `taking_selfie` is words only: the message tells of the photo and no
image is composed. `watching_match` and `taking_selfie` are drawn in hospital; the other
four need her out and about.

### Next to `/open`

An event's message counts as a message for the `/open` birthday and holiday check, so neither
greeting follows it on the same local day. An opener that is being generated when an event
message lands yields and returns `null`; an event does not yield to an opener. A greeting
carries the `正在经历` and `刚才` lines and no needs line, and `/open` returns `null` while an
offline kind is under way.

### Storage

- `engine.persona_events` holds one row per event: `instance_id` (cascades with the instance),
  `session_id` (where it was drawn and told; set to NULL if the session goes), `event_key`
  (unique within the session), `kind`, `started_at`, `ends_at` and `message_id` (the message
  that tells it; NULL for an offline kind she does not announce, and set to NULL if the message goes). Category and
  background-ness derive from the kind and are not stored. A row whose kind the build does not
  know is skipped on read.
- The message is a row of `engine.chat_messages` with `metadata.proactive = 'event'`; the
  event's kind and times live in `persona_events`.
- Needs live on the rated user row, as `metadata.needs`, with a partial index
  (`chat_messages_needs_idx`) that keeps the newest-rating read an index probe per session.
- `taking_space` and `fell_out` are rows of `persona_events` like any other, with `message_id`
  NULL; the choice that wrote them lives in `engine.life_choices` ([below](#where-the-choice-is-stored)).

### Beyond the chat

With [World Stories](world-system.md#world-stories) enabled, the story director reads the
instance's events directly, each with its label, a relative time and the message she told it
in, plus the background under way and the pair's asks for space with their outcome. It takes
them as settled fact. The messages she sends on her own are not part of its chat evidence, so
an event is never read as her answer to an unrelated line. The World Memories director reads a
background under way for every persona in the roster, stories or not, so a persona in hospital
is not scripted into a party. Neither director writes events or changes the persona settings
above. Needs reach neither.

## How the two interact

### 1. Needs weight the kind pick

A needs factor scales a kind's weight within its category, in the second stage of the draw. It
steers which kind comes up, never whether a category does. The factors, from
`needs_factor` in `crates/eros-engine-core/src/events.rs`:

| need level | kinds | factor |
|---|---|---|
| `stamina` is `low` | `temp_work`, `working_out` | ×0.5 |
| `stamina` is `low` | `unwell`, `sick`, `bathing`, `asleep` | ×2 |
| `hunger` is `high` | `grocery_shopping` | ×2 |
| `emotional` is `high` | `ex_message` | ×3 |
| `emotional` is `high` | `animal_encounter`, `irritable`, `quarrel` | ×2 |

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

### 3. An offline stretch writes no rating

A turn silenced by an offline kind runs no judge, so it rates nothing. The last rating keeps
drifting through the stretch, and the first judged turn afterwards is shown it carried across
all of it: after a long stretch she reads hungrier and lonelier than before.

Put together, the two form a loop with the judge in the middle: a hungry persona is likelier
to be out buying groceries; the event reaches the judge's context; the judge rates hunger
again.

## The ask-for-space choice

When chemistry has run two tiers past bond and her patience has been worn low for three judged
turns running, the relationship is lopsided: passion ahead of friendship, and she is tired. She
asks for a break. It is the first *life choice*, a real request with a real cost on every path,
and it is built from the machinery above: her ask is a proactive message, the break is an
offline event, and the user answers through one endpoint. The fuller `life_choices` system (more
kinds of choice, configurable consequences) is not part of this flow.

### When she asks

The engine checks at the end of every 1:1 text turn whose affinity evaluation committed (a
*judged turn*: the affinity evaluator ran and its verdict landed, whether or not the deployment
runs the PDE judge), after the commit. She asks when all of these hold, read from the
committed, time-decayed state:

- **Gap.** Chemistry tier minus bond tier is at least 2, on the two five-tier ladders. Signed:
  only chemistry above bond counts.
- **Patience.** The derived `patience` is below 0.5 on this turn and on the two judged turns
  before it. A turn whose evaluation was skipped or failed neither counts nor breaks the run.
  The values are read from the session's affinity turn events (`state_after.patience`), not
  stored anew; 0.5 is a rule threshold the engine owns, not a judge band.
- **Fresh streak.** All three turns come after the session's newest episode: her ask, its
  resolution, and any separation it started (the latest of `asked_at`, `resolved_at` and, for an
  accepted choice, the separation's `ends_at`). A judged action turn that fell while she waited
  or during the separation belongs to the episode and counts for nothing; the turn that converts
  an ignored ask is the first of the new streak. Asking consumes the streak: the next ask needs
  three fresh judged turns.
- **Clear runway.** No session of the persona has a pending choice, and the persona has no
  separation (`taking_space`) under way. She has one life, so at most one ask is pending at a
  time.
- **The right session.** A 1:1 text session of a genome persona. A voice session never
  triggers, delivers or resolves a choice, and a group session writes no affinity at all. A BYOC
  session never triggers one, because the resolution endpoint refuses BYOC.

The trigger is engine math over committed state; no judge takes part. There is no cooldown
beyond these mechanics. If the user keeps recreating the condition she keeps asking: the tier
drop below lowers both lines, so the gap closes only once bond is at tier 1 (a tier-1 line
cannot drop further while chemistry still can) or the user raises bond. Repeated asks are the
consequence of a relationship kept lopsided.

### Her ask and its delivery

When the trigger fires, in order:

1. **Her words.** The companion chat model generates the request through the same proactive
   path as an event message (persona, history window, memory recall, `[now]`; no judge, no
   streaming, no user row). The stage cue is the final `user` turn and is never persisted:

   ```
   （这段关系最近让你很累：心动还在，但交情和耐心都被磨薄了。你决定郑重地告诉对方：你想先分开一阵子，希望对方给你一个答复。）
   ```

   The message is an ordinary assistant row, `assistant_action_type = 'proactive'`,
   `metadata.proactive = 'life_choice'`, unread until the read call, with no post-process.
2. **The choice row.** One row in `engine.life_choices`, written in the same transaction as her
   message: kind `ask_for_space`, status `pending`, the tier gap at this moment (2 to 4), and
   `asked_at`, which is the message's `sent_at`. The pending window is 24 hours; the expiry is
   `asked_at` plus 24 hours, derived and never stored.

A blank or stripped-empty generation, or a failed one, writes nothing: no message, no choice
row, and a later qualifying turn may ask again. The check runs last in the turn's post-process,
after the affinity commit and the feeling clause, so its generation delays neither, and the
turn's own affinity write (state and event row, one transaction) is untouched.

While a choice is pending and unexpired, the events route draws nothing for the persona. An
incident that was already under way when she asked keeps running until the answer; an accept or
an ignore caps it.

**Delivery.** The notification rides the per-turn affinity delta fetch,
[`GET /bff/v1/comp/affinity/{session_id}/event`](api-reference.md#get-bffv1compaffinitysession_idevent).
Its `event` object gains one optional field, `life_choice` (`id`, `kind`, `options`,
`asked_at`, `expires_at`, `message_id`), composed at serve time from the session's pending,
unexpired choice and absent otherwise. The ask lands seconds after the triggering turn's event
commits, so the fetch the turn itself releases usually misses it: an adapted client re-fetches
the delta when a history row carries `occasion: "life_choice"`. Both history routes
([canonical](api-reference.md#get-compchatsession_idhistorylimit20offset0) and
[BFF](api-reference.md#get-bffv1compchatsession_idhistorylimit50offset0)) expose a proactive
assistant row's `metadata.proactive` as an optional `occasion`, omitted on every other row. The
tier-drop affinity event is not served on this route, so a client never meets an unknown
`event_type`; the new state comes back in the resolution response, and any other device reads
it from the absolute affinity route.

**Degradation is the design.** A downstream that has not adapted ignores the unknown field and
renders the ask as an ordinary proactive message, and its user replies in words. That is the
ignore path below. Nothing breaks; the choice UI is strictly additive.

### The user's answer

The answer is `POST /v2/comp/session/{session_id}/life-choices/{choice_id}` with
`{"response": "accept" | "decline"}`
([API reference](api-reference.md#post-v2compsessionsession_idlife-choiceschoice_id)),
or the user's own words, or silence.

| path | how it happens | choice status | tier drop | separation | her words |
|---|---|---|---|---|---|
| accept | the endpoint, `accept` | `accepted` | yes | `taking_space`, 6 h per tier of the gap | a generated goodbye |
| decline | the endpoint, `decline` | `declined` | yes | none | a generated hurt reply |
| ignore | the first ordinary user message while it is pending | `ignored` | no | none; a one-hour `fell_out` incident | none of its own: the turn is answered with the falling-out in view |
| lapse | 24 hours with no answer and no message | stays `pending`, reads as expired | no | none | none |

**Accept.** In one transaction the choice becomes `accepted`, a `taking_space` event
(说好暂时分开) starts now and lasts 6 hours per tier of the gap stored at ask time (gap 2, 3, 4:
12, 18, 24 hours), and the tier drop is applied. An incident under way ends at that moment, as
she drops what she was doing; a background (a trip, an illness) keeps running underneath. Then
her goodbye is generated and returned, as an ordinary proactive row with
`metadata.proactive = 'life_choice'`:

```
（对方同意了你提出的分开请求，接下来 {n} 小时你们不联系。你跟对方道别。）
```

`{n}` is the separation's length in hours. A failed or empty goodbye still leaves the
resolution, the event and the drop in place: the choice was made.

**Decline.** The same tier drop in the same transaction, no event and no slowdown. Her hurt
reply is generated and returned:

```
（对方不同意你提出的分开请求，你心里不好受。）
```

**Ignore.** The first ordinary user text message that arrives while the choice is pending
resolves it: the status becomes `ignored`, and a `fell_out` event (为分开的事闹得不愉快, an
incident shaped like `quarrel`: one hour, not offline, no message of its own) is written in the
same transaction, before the turn runs. The very turn that ignored her is therefore answered
with the falling-out in `[近期事件]` and `正在经历`. An incident under way is capped there, as
on accept: the falling-out displaces it, and the one-incident rule holds. No tier changes; the
judge sees the context and rates the turn itself. The clock is the user row's `sent_at`: only a
message sent after her ask converts it (one already in flight when the ask landed answers
nothing), and a queued or re-driven turn reads the same. An **action turn** (a give, a tip, a
kiss) does not resolve the choice: it is answered by its own rules and the ask stays pending, as
a gift while she waits is an attempt to mend, not an answer. `/open` greetings and reactions
resolve nothing.

**Lapse.** With no answer and no message for 24 hours the choice simply lapses. Its status stays
`pending` in the table and reads as expired everywhere: the delta field stops being attached,
the endpoint answers `409`, and a later message does not convert to `fell_out`. There is no
cost; a later ask needs the full trigger again.

A resolution and an incoming user message race transactionally. The first writer wins and the
loser writes nothing (the endpoint answers `409`).

### The costs

**The tier drop (accept and decline).** Bond and chemistry each drop one tier, by the
rejected-action landing rule applied to each line: the line lands at the same relative position
one tier down, its two axes scaled by the same factor, and a tier-1 line goes to 0. Both lines'
pending deltas are cleared. It is applied at resolution time, atomically with it: on accept the
drop is the state she comes back to, and nothing further happens at return. The affinity event
and its context keys are in [affinity-model.md](affinity-model.md#life-choice-drops-and-the-slowdown).

**The slowdown (accept only).** For 72 hours after the separation ends, every positive committed
delta on the four line axes is halved before it is applied; the cut part is dropped, not parked in
`pending_deltas`. Negative deltas apply in full, and the derived `warmth` and `patience` are not
scaled. It applies in the choice's session only: the
cost is that relationship's, like the drop, and another session of the same persona pays
nothing. The window is derived through the choice row (`life_choices.event_id`, the
`taking_space` row's `ends_at`, to `ends_at` plus 72 hours): no new state, wall-clock rather than
turn count, so it runs even if the user only returns later. The returned `effective_deltas` show
the halved values.

### The separation

`taking_space` is a non-drawable offline incident and reuses [the offline
machinery](#offline-kinds) wholesale:

- Every 1:1 text turn of the persona, in any session, ghosts without the judge: `meta(ghost)`,
  `done`, `final`; no needs rating, no ghost-streak write.
- Action turns and forced-image turns are exempt, as for any offline stretch.
- `/open` returns `null`.
- No event is drawn while it is under way, and it starts the shared 12-hour offline cooldown.
- It is instance-scoped like every `persona_events` row: she has one life, and taking space
  means her phone is down everywhere.

With the ghosting switch off it silences nothing, like any offline stretch; the drop, the
slowdown and the return line do not depend on the switch. During a separation a message only
ghosts and never converts any pending ask.
While it lasts, `正在经历` and the exempt action turns read it as 说好暂时分开.

### The return

The return is acknowledged either way. The standard unexplained rule needs a ghosted user row
inside the window, but a user who honours the break sends nothing, and she must still mark the
return or the arc fizzles. So in the choice's session, `taking_space` is unexplained from its
`ends_at` until the first assistant row with text after it, ghosted rows or none, and the
wording follows whether the user wrote during the window. Her goodbye was written when the
separation started, so it does not close the return. The persona's other sessions use the
standard ghosted-row rule with the first wording.

| the user | `- 刚才：` | judge suffix |
|---|---|---|
| wrote during the window | `你们之前说好分开一阵，现在时间到了；对方那时发来的消息你现在才看到` | `说好暂时分开（{span}前结束，期间对方的消息你没有回）` |
| honoured the break | `你们之前说好分开一阵，现在时间到了，你们重新开始联系` | `说好暂时分开（{span}前结束，对方等到了约定之后）` |

### Where the choice is stored

`engine.life_choices` holds one row per choice: `session_id` (non-null, cascades with the
session), `kind`, `status` (`pending`, `accepted`, `declined`, `ignored`), `tier_gap`,
`asked_at`, `resolved_at`, `message_id` (the ask; set to NULL if the message goes) and
`event_id` (the `taking_space` row an accept wrote, NULL on every other path; set to NULL if
the event goes). Expiry and the slowdown window are derived, never stored. The separation and
falling-out rows live in `engine.persona_events` like any event.

## Without the LLM judge

| | Without `[tasks.pde_decision].filter_prompt` |
|---|---|
| Needs | none: no rating, no lines, `needs: null`, no draw factors |
| The draw | works, with no needs factors |
| An event's message | generated and told as usual |
| `正在经历` and `刚才` | reach replies, `/open` greetings and event messages |
| `[近期事件]` | not rendered: there is no judge to read it |
| Offline kinds | silence turns as with the judge, since the check runs before it: the ghosting switch and the action and forced-image exemptions apply |
| The ask for space | unchanged: the trigger reads the affinity evaluator's judged turns, not the PDE judge |

## Code pointers

- `crates/eros-engine-core/src/needs.rs`: the levels, the drift constants, `carried`.
- `crates/eros-engine-core/src/events.rs`: categories and kinds, per-kind rules, gates, the
  needs and persona factors, `is_offline` and `is_told`, `draw`, `lookback`.
- `crates/eros-engine-server/src/routes/session_events.rs`: the events route, the draw's
  inputs, the offline gates.
- `crates/eros-engine-server/src/routes/life_choices.rs`: the resolution endpoint.
- `crates/eros-engine-server/src/routes/bff/affinity.rs`: the `life_choice` field on the delta.
- `crates/eros-engine-server/src/pipeline/proactive.rs`: the event message and the choice's
  messages, their cues and their write (`kind_cue`, `find_cue`).
- `crates/eros-engine-server/src/pipeline/life_choice.rs`: the trigger and her ask, the ignore
  conversion, the slowdown read.
- `crates/eros-engine-server/src/pipeline/stream.rs::run_stream`: the needs read and write, the
  `[需求]` and `[近期事件]` lines (`build_pde_ctx`), the offline branch ahead of the judge,
  `body_state_frame`.
- `crates/eros-engine-server/src/prompt.rs`: `needs_line`, `events_judge_text`,
  `events_reply_text`, `just_now_text`, `persona_traits`, the `[character_state]` lines and the
  event lines under `[now]`.
- `crates/eros-engine-server/src/routes/session_effects.rs`: `GET …/effects`.
- `crates/eros-engine-server/src/pipeline/voice.rs`: the bootstrap's needs.
- `crates/eros-engine-store/src/events.rs::EventRepo`: `insert`, `insert_preempting_in_tx`,
  `by_key`, `recent`, `current`, `backgrounds_at`, `unexplained_offline`,
  `unreturned_separation`, `unresolved_scare`.
- `crates/eros-engine-store/src/story.rs::StoryRepo`: `life_events`, `life_choices`, and
  `chat_evidence` without proactive rows.
- `crates/eros-engine-server/src/pipeline/story.rs`: `load_evidence` and the story payload's
  `life_events`, `current_background`, `life_choices`; `pipeline/world.rs::direct_world`: the
  world director's `current_background`; `prompt.rs::event_when_text`: the relative times both
  read.
- `crates/eros-engine-store/src/life_choices.rs::LifeChoiceRepo`: the choice rows, `pending`,
  `resolve_in_tx`, `growth_slowed`.
- `crates/eros-engine-store/src/affinity.rs::AffinityRepo`: `persist_life_choice_drop_in_tx`,
  `last_judged_patiences`.
- `crates/eros-engine-store/src/chat.rs::ChatRepo`: `set_needs` and `latest_needs`.
- `crates/eros-engine-store/migrations/0070_chat_messages_needs_idx.sql` and
  `0072_persona_events.sql`: the index and the table.
- `crates/eros-engine-store/migrations/0076_life_choices.sql` and
  `0077_affinity_event_life_choice.sql`: the choices table and the widened affinity
  `event_type` check.

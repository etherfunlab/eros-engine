# Life events and needs

[English](life-events-needs.md) · [中文](life-events-needs.zh.md)

A persona has a day of her own, and two kinds of state follow it. **Needs** are four
basic levels (social, emotional, outing, indulgence) that the PDE judge
rates, the engine carries forward in time, and an accepted give moves. **Life events** are
things that happen to her, drawn when a caller asks: a trip, an illness, a stray cat, an
hour away from her phone. Both reach the judge and the reply as plain facts in the present
tense. Neither tells her how to behave; the exception is an event that is silence (the
offline kinds). A
third piece sits on top of both: *life choices*. She can ask the user what they are to each
other (confess, offer a partnership, propose, break up), ask to spend the night, to bathe
together or to go away together ([life choices](#life-choices)); and when the user wears her
patience out, the pair falls into a cold war ([the cold war](#the-cold-war)).

Request and response shapes are in [api-reference.md](api-reference.md), the judge's
context and verdict in
[model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge). This page is
about how the pieces work and how they fit together.

## At a glance

| | Needs | Life events |
|---|---|---|
| What it is | four levels: `social`, `emotional`, `outing`, `indulgence`, each `low` / `mid` / `high` | things that happen in her day, in four categories |
| Written by | the PDE judge, on a judged text turn; the engine too, on an accepted give ([Gives move needs](#gives-move-needs)) | a caller's `POST /v2/comp/session/{session_id}/events`; the engine picks the kind |
| Stored in | `metadata.needs` of the rated user row | `engine.persona_events`, plus the message that tells it (none for an offline kind she does not announce) |
| Scope | one state per persona instance, across all its sessions | the same |
| Time | drifts between ratings, computed on read | `started_at` and `ends_at`; cooldowns count from the start |
| Needs the LLM judge | yes | no |
| Judge sees | the `[需求]` line | the `[近期事件]` line |
| Reply sees | `- 此刻的需要：…` | `- 正在经历：…` and `- 刚才：…` |
| Picture composer sees | — | the `[此刻状态]` section |
| Caller gets | `needs` on the `body_state` frame and on `GET …/effects` | the events route response; `event` and `background_event` on `GET …/effects` |

Voice calls read needs and no events. Group turns read neither in the reply; only a group picture's composer reads the speaker's events. Details follow.

## Needs

### The four needs

| need | `low` | `high` | reply wording, `low` / `high` |
|---|---|---|---|
| `social` | wants no company | wants someone to talk to | 不想跟人打交道 / 想找人说说话 |
| `emotional` | content | badly wants to be cared about | 心里挺满足 / 很想被在乎 |
| `outing` | wants to stay in | wants to go out | 只想待在屋里 / 想出门走走 |
| `indulgence` | no urge | craves what she's hooked on: a smoke, a drink, weed, a coffee, a good meal | — / 很想抽根烟, 很想喝一杯, 很想抽口大麻, 很想喝杯咖啡, 很想吃顿好的 or 很想放纵一下 ([below](#where-needs-show-up)) |

Levels are ordinals. The judge never reads or writes a number, and `mid` is the unremarkable
middle.

### Who rates them

Needs belong to the opt-in LLM judge (`[tasks.pde_decision].filter_prompt` set). Without it
there are no needs: no `[需求]` line, no reply line, `needs: null` on the frame and the
effects route, no needs factor in the event draw, nothing in a voice call.

- **The verdict.** The judge's JSON may carry `needs`: an object with all four keys, or `null`
  for "no change, keep the previous rating". A value that does not parse reads as `null` and
  never fails the verdict. The strict `response_format` requires all four keys inside a
  non-null object. An object that has `social` and `emotional` but lacks `outing` or
  `indulgence` (a judge prompt that does not ask for them, or a non-strict judge) keeps, for
  each missing key, the level the judge was shown (`mid` before the first rating); one that
  lacks `social` or `emotional` does not parse. Keys the engine does not rate, such as
  `hunger` or `stamina` from an older judge prompt, are ignored.
- **What it rates.** The persona alone, never the relationship; the affinity evaluator rates
  that.
- **Where it is stored.** On the user row of the turn the judge rated, as `metadata.needs`,
  whatever the turn's outcome: reply, image, ghost or product question. On a turn with an
  accepted give the engine writes the stored levels, even when the judge abstained: the needs
  the item moves come from the baseline, and the rest from the judge, or from the baseline when
  it abstained ([below](#gives-move-needs)). The write is awaited before the turn goes on. A failed write is
  logged and the turn carries on with the levels it meant to write.
- **What writes nothing.** A rule-engine turn (the judge is off or failed), a `needs: null`
  verdict, a turn silenced by an offline stretch (it runs no judge), and every group
  or voice turn. The exception is an accepted give that moves a need, in a deployment that
  runs the judge: it writes on a turn whose judge failed or returned `needs: null` too, once a
  rating precedes it. A deployment without the judge reads no baseline, so a give there writes
  nothing.
- **Whose state.** The persona instance's. The newest rated row is read across all the
  instance's sessions, so a second session does not reset her needs.

The judge's `needs` and the levels it was shown are audited with the rest of the verdict; see
[llm-audit.md](llm-audit.md#pde-judge-payload).

### Gives move needs

In a deployment that runs the judge, on a 1:1 text turn whose `give` is accepted, the engine
moves two needs by one level each:

| `give` item | `social` | `indulgence` |
|---|---|---|
| `cigarette`, `alcohol`, `herb`, `cuisine` | +1 | +1 |
| `medicine`, `coffee` | −1 | −1 |
| any other item, `food` included | | |

−1 is one level toward `low` and +1 one level toward `high`, stopping at the ends. The
quantity is ignored: one give moves one level. An accepted `smoke_together`,
`smoke_weed_together` or `drink_together` moves them as a give of one `cigarette`, `herb` or
`alcohol` does ([together actions](#showering-bathing-and-the-sauna-together)).

*Accepted* means the turn's effective response is accept. A judge refusal, or the habit roll
forcing a refusal ([the habit roll](model-config.md#taskspde_decision--opt-in-llm-pde-judge)),
moves nothing. A fail-open turn (the judge failed) accepts the give, since a failed judge means accept, so
the move applies there too.

What gets written:

- **The baseline.** The levels the judge was shown: the newest rating before this user row,
  carried to its `sent_at`.
- **The moved needs.** For each need the item moves, the stored level is the baseline moved by
  one. The judge's rating of that need this turn is discarded: it sees the give in
  `[用户动作]` and often moves those needs itself, and shifting on top of its rating would count
  the give twice.
- **The other needs.** The judge's rating, resolved against the baseline as on any turn. If the
  judge abstained or failed, they keep the baseline.
- **No baseline.** With no rating yet, the judge's rating is written unshifted; with no judge
  rating either, nothing is written. The engine never invents levels from nothing.
- **One write.** The result goes to the user row once, with the write above. On a give turn it
  is written even when the judge abstained, because the engine has its own reason to rate.
- **Readers.** The reply, the `body_state` frame and `GET …/effects` read the written levels as
  they do any rating. The judge's audit keeps what the judge said.

An item that is not in the table moves nothing, and the judge's rating of that turn is written
as on any other. Voice and group turns write no needs, give or not. Life events move needs only
through the judge ([below](#2-events-never-change-a-stored-need)).

### Drift between ratings

Nothing is stored between ratings. The current state is the newest rating plus the time
since. Each need moves one level toward its resting level for every full period elapsed since
the rating, and stops there:

| need | settles at | one step every |
|---|---|---|
| `social` | `high` | 12 h |
| `emotional` | `mid` | 12 h |
| `outing` | `mid` | 8 h |
| `indulgence`, no habit `true` | `mid` | 8 h |
| `indulgence`, one habit `true` | `high` | 12 h |
| `indulgence`, two habits `true` | `high` | 8 h |
| `indulgence`, three or more habits `true` | `high` | 4 h |

Time away makes her want company; emotional need and the wish to go out or stay in settle
toward the middle. A `low` social rated at noon reads `mid` at midnight and `high` from noon
the next day. The habits are the persona's `smoking`, `alcoholic`, `drug`, `coffee` and `gluttony` keys
([Persona settings](#persona-settings)); `false` and absent count alike. The more of them are
`true`, the sooner the craving comes back after the judge rated it down. A rating
stored without `outing` and `indulgence` reads both as `mid`; one that still carries `hunger`
or `stamina` reads with those keys ignored. A BYOC persona's keys live on
her card, which the engine opens only on a turn: outside a turn (`GET …/effects`, a replay that
falls back to the carried levels) her `indulgence` drifts as for a persona with no habit. The
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
  [需求] 社交欲=低 情感需求=高 外出=中
  [需求] 社交欲=低 情感需求=高 外出=中 享受=高
  ```

  `外出` is always on the line; `享受=高` is there only when `indulgence` is `high`, and its
  absence reads as "not high" (the reference judge prompt says so). A strict judge still rates
  `indulgence` on every judged turn without seeing whether it was `low` or `mid`, so in
  continuous chat it re-guesses that level each turn, and the habit-count drift acts in the
  gaps between ratings. Before the first rating it reads `[需求] 尚未判定`. The line is never
  absent, because a missing line would read as "all low".
- **The reply** gets the levels that are not `mid`, in the order of the table above, joined
  by `；`, as a line of `[character_state]`:

  ```
  [character_state]（这段关系里目前的状况，不是人设；与上面冲突时以上面为准）
  - 此刻的需要：想找人说说话；很想被在乎
  ```

  `indulgence` shows only at `high`. It names one thing she uses, picked at random each turn
  among the habit keys set to `true`: `smoking` 很想抽根烟, `alcoholic` 很想喝一杯, `drug`
  很想抽口大麻, `coffee` 很想喝杯咖啡, `gluttony` 很想吃顿好的. With no key `true` it reads 很想放纵一下. Every need at `mid` (a `low`
  `indulgence` counts the same), or no rating, adds nothing and leaves the prompt
  unchanged. A judged turn's reply is written with the levels the turn ends with: the
  levels written to its row (with the needs an accepted give moved taken from the baseline), otherwise the
  carried levels the judge was shown. A rule-engine turn in a deployment that has the judge
  uses the carried levels, moved by an accepted give.
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
  frame carries the frozen levels. The call never rates or advances them, and a high
  `indulgence` names the same thing for the whole call: the pick derives from the snapshot's
  creation time. See
  [the voice turn](api-reference.md#post-compvoicesession_idturnstream).
- **Not shown to:** `/open` greetings, event messages, image edit and group turns.

The engine acts on needs in one place: the weights of the event draw (below). No ghost rule
and no image rule reads them. A persona who wants no company is not ghosting-prone by rule;
the judge sees the levels and decides.

## Life events

### Categories

An event is one of 70 drawable kinds. Each kind belongs to a category, and the category sets its
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

Twenty-five more kinds are never drawn: `taking_space` (说好暂时分开), `fell_out`
(为分开的事闹得不愉快), `traveling_together` (和对方一起旅行), `bathing_together`
(和对方一起洗澡), `on_a_date` (和对方约会), `showering_together` (和对方一起淋浴),
`sauna_together` (和对方一起蒸桑拿) and the eighteen kinds of the other together actions, from
`using_toilet_together` to `hiking_together`
([together actions](#showering-bathing-and-the-sauna-together)). The first two belong to the
retired ask for space: the engine no longer writes them, and they remain only in stored data
([where the choice is stored](#where-the-choice-is-stored)). They are
`mid` incidents by shape, and `taking_space` is the one that outlasts the category's four
hours: it lasts 12 to 24. `traveling_together` is a `trip` background the engine writes when the
user accepts her suggestion ([going away together](#going-away-together)),
`showering_together` and `bathing_together` 30-minute `short` incidents it writes when the user
accepts her ask for a bath, one or the other at even odds ([a bath together](#a-bath-together)),
or when she accepts the user's `shower_together` or `bath_together`, `sauna_together` a 1-hour
`short` incident it writes when she accepts the user's `sauna_together`
([together actions](#showering-bathing-and-the-sauna-together)), the eighteen others the incidents
it writes when she accepts the user's other together actions, all `short` but the `mid`
`asleep_together`, `fighting_together` and `hiking_together` (`having_sex_together` and
`asleep_together` are also written when the user accepts her `sleep_together` or `fall_asleep_together`,
[the romantic choices](#the-romantic-choices)), and `on_a_date` a `short`
incident of 1 or 4 hours it writes when she accepts the user's date invitation
([activities.md](activities.md#the-event)).
None of the twenty-five has a draw rule, and the caller's `kinds` rejects them.

### The draw

The draw is a pure function of the inputs plus three random numbers (one per stage, one for
the length).

1. **Eligibility.** A kind is eligible when all of these hold:
   - the caller's `kinds` allows it ([below](#the-callers-kinds); absent means all 70);
   - it fits her world ([below](#her-world));
   - it passes its gate (below);
   - its own cooldown has elapsed;
   - its category's cooldown has elapsed;
   - for an offline kind, the shared offline cooldown has elapsed (below);
   - the background rules allow it (below);
   - for a kind that needs a background (`got_lost`, `missed_ride`, the commute kinds and the
     vehicles), one of its backgrounds is under way (below).
2. **Category.** Each category with at least one eligible kind weighs its category weight
   times its persona factor. A roll picks one by cumulative weight.
3. **Kind.** Each eligible kind in that category weighs its base weight times its
   [needs factors](#1-needs-weight-the-kind-pick), its `prefer_events` factor, its
   [follow-on factor](#follow-on-factors) and, for a few kinds, a persona factor
   ([`is_virgin`](#persona-settings), [`homesick`](#persona-settings),
   [`sex_addict`](#persona-settings), [`hygienic`](#persona-settings),
   [`employed`](#persona-settings), [`outdoorsy`](#outdoors-or-in)). A second roll picks
   one.
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
  `working_out`, `on_the_way`, `in_battle`, `got_bumped`, `saw_bag`, `saw_clothes`,
  `out_shopping`, `bros_gathering`, `sis_gathering`, `admiring_scenery`, `admiring_art`,
  `watching_show`, `on_the_road`, `having_sex`, `bird_encounter`, `streaking`, `got_lost`,
  `drinking`, `smoking`, `smoking_weed`, `just_have_sex`. During `period`,
  `period_scare` and `pregnancy_scare` are not drawn. During `traveling_together` no kind that
  can take her offline is drawn, nor `just_have_sex`: she is with the user.
- Ten kinds need a background rather than being ruled out by one. `got_lost` and `on_a_cruise`
  are drawn only while a `short_trip`, `long_trip`, `hometown_visit` or `traveling_together` is
  under way, never on a `business_trip` or outside a trip. `missed_ride`, `on_the_way`,
  `on_the_road`, `on_a_boat`, `on_a_bus`, `on_a_train`, `on_a_plane` and `driving` are drawn
  only while one of those or a `business_trip` is under way.
- One incident and one background can run together. An incident drawn inside a background is
  told from inside it.
- While she waits for the user's answer to one of her life choices, no draw happens either, in any
  session of the persona: the ask is the foreground drama, and it keeps an offline stretch from
  starting mid-question. A repeat of an `event_key` that already drew still returns its event.
- While a date of hers is not settled yet, under way or ended, no draw happens either, in any
  session of the persona: she is out with the user, or back with the date not yet in her memory
  ([activities.md](activities.md#what-pauses)). A repeat of an `event_key` still returns its
  event.
- This is per persona instance across all its sessions: she has one life. A draw reads the
  instance's events of the last 90 days, the longest cooldown there is.

The write enforces the same rule atomically: an incident is not written while another incident
is under way, and a background is not written while any event is.

### Cooldowns and caps

- **Per kind.** A kind cannot start again until its cooldown has elapsed since its newest event
  started, not ended. Cooldowns run from 12 hours (`showering`, `reading` and others) up; the heavy kinds are
  long (`hit_someone` 14 days, `breakup` 30, `hospitalized` 60, `post_surgery` 90). The four
  `trip` kinds have none of their own; the category cooldown covers them.
- **Per category.** `body` cannot start more than once a fortnight and `trip` more than once
  a week, whichever kinds are involved. `short` and `mid` have none.
- **Shared by the offline kinds.** For 12 hours after any offline kind started, no offline kind
  is eligible, whichever kind it was; each kind's own cooldown still applies. It bounds how often
  she goes silent, the way the category cooldowns bound trips and body states. `out_shopping`
  counts as an offline kind only for a persona it takes offline.
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
| `ex_message`, `breakup`, `ex_wants_back` | intimacy rung 2 or higher |
| `sudden_desire`, `just_have_sex` | intimacy rung 3 |
| `masturbating` | the persona's `art_metadata.sex_addict` is `true`, and intimacy rung 3 |
| `drinking`, `smoking`, `smoking_weed` | the persona's `art_metadata.alcoholic`, `smoking`, `drug` respectively is `true` |
| `got_lost`, `on_a_cruise` | a `short_trip`, `long_trip`, `hometown_visit` or `traveling_together` under way ([above](#one-incident-and-one-background-at-a-time)) |
| `missed_ride`, `on_the_way`, `on_the_road`, `on_a_boat`, `on_a_bus`, `on_a_train`, `on_a_plane`, `driving` | a `business_trip`, `short_trip`, `long_trip`, `hometown_visit` or `traveling_together` under way |
| `strip`, `streaking` | intimacy rung 3, and the session's newest recorded `affinity_scope` is romance-compatible (a chemistry axis in scope); none recorded is not compatible |
| `saw_bag`, `saw_clothes` | the persona's `art_metadata.gender` is `"female"` |
| `period` | the persona's `art_metadata.gender` is `"female"`, and she is not a machine (`art_metadata.species` `"robot"` or `"android"`) |
| `want_to_quit_job` | the persona's `art_metadata.employed` is not `"free"` |
| `period_scare` | the persona's `art_metadata.gender` is `"female"`, she is not a machine, and intimacy rung 2 or higher |
| `pregnancy_scare` | the persona's `art_metadata.gender` is `"female"`, she is not a machine, and intimacy rung 3 |
| `using_toilet` | she is not a machine |
| `having_sex` | the persona's `art_metadata.sex_addict` is `true` |
| `away_from_phone`, `busy`, `in_battle`, `asleep`, `bros_gathering`, `sis_gathering`, `having_sex`, and `out_shopping` for a female persona | the ghost protections allow it (see [below](#offline-kinds)) |

`just_have_sex` reads only the rung: it has no romance-compatibility check, unlike `strip`.

`out_shopping` and the two gatherings have no gender gate: [Shopping and
gatherings](#shopping-and-gatherings) says who goes offline and why both genders draw both
gatherings.

The rung (1 to 3) is read from the session's affinity, time-decayed as in a chat turn; see
[affinity-model.md](affinity-model.md).

### Persona settings

Sixteen keys in the persona genome's `art_metadata`, read by the engine and written by whoever
authors the persona, sit beside `gender`, `birthday` and `timezone`:

| key | value | effect |
|---|---|---|
| `prefer_events` | array of `mid` kinds | each listed kind weighs 3 times as much within `mid` |
| `health` | `"weak"`, `"normal"` (default), `"strong"` | the `body` category weighs 2 times, 1 time, half as much |
| `prefer_trip` | `true`, `false` (default) | the `trip` category weighs 3 times as much |
| `homesick` | `true`, `false`, absent (default) | within `trip`: `true` weighs `hometown_visit` 3 times and `short_trip` / `long_trip` a third; `false` weighs `hometown_visit` a third; `business_trip` is work and untouched either way |
| `is_virgin` | `true`, `false` (default) | while she is a virgin, `having_sex` weighs a fifth within `mid` and `just_have_sex` a fifth within `short` |
| `sex_addict` | `true`, `false`, absent (default) | within `short`: `true` weighs `sudden_desire`, `strip`, `streaking` and `just_have_sex` 3 times, `false` a third; their gates hold either way. Only `true` lets her draw `having_sex` and `masturbating` |
| `hygienic` | `true`, `false`, absent (default) | within `short`: `true` weighs `showering`, `bathing`, `want_hot_spring` and `want_sauna` 3 times, `false` a third; it also sets the chemistry tier [a bath together](#a-bath-together) needs |
| `employed` | `"free"`, `"part_time"` (default), `"full_time"` | how much of her day is work: `temp_work`, `scolded_by_superior`, `scolded_superior` and `business_trip` weigh a third, 1 time, 3 times within their categories; a `free` persona never draws `want_to_quit_job`; any other value reads as `part_time`. `full_time` also makes her career-minded: while the deployment has the `focused` tone on, a turn off work with no incident under way is `focused` 1 time in 8 ([at work](#at-work)) |
| `outdoorsy` | `"outdoor"`, `"neutral"` (default), `"indoor"` | within each category, `outdoor` weighs the outdoor kinds 3 times and the indoor kinds a third, `indoor` the reverse; neutral kinds are untouched ([below](#outdoors-or-in)); any other value reads as `neutral` |
| `setting` | `"modern"` (default), `"fantasy"` | the world she lives in ([below](#her-world)); a persona that is not `fantasy` refuses the user's `fight_together`, and a `fantasy` one the user's `cycle_together` ([together actions](#showering-bathing-and-the-sauna-together)); any other value reads as `modern` |
| `species` | any word | `"robot"` and `"android"` are machines: she never draws `period`, `period_scare`, `pregnancy_scare` or `using_toilet`, and refuses the user's `toilet_together` ([together actions](#showering-bathing-and-the-sauna-together)); every other word leaves the draw alone. What the word does to the prompt is in [eros-character-v1.1.md](eros-character-v1.1.md) |
| `smoking`, `alcoholic`, `drug`, `coffee`, `gluttony` | each `true`, `false`, absent (default) | whether she smokes, drinks, uses weed, drinks coffee, loves good food (`false`: she keeps off rich food): how fast `indulgence` comes back ([drift](#drift-between-ratings)), what a high `indulgence` names in the reply, and how she takes a give of that thing, or the user's ask to smoke, smoke weed or drink with her (below); `smoking`, `alcoholic` and `drug` set to `true` also let her draw `smoking`, `drinking` and `smoking_weed` |

`prefer_trip` says whether she moves at all; `homesick` says where she goes when she does. A
flight attendant with both `true` travels often, works her trips as before, and spends her
leave at home rather than elsewhere.

Her virginity as the draw sees it is derived: the card says `is_virgin` and the instance has
no `having_sex`, `just_have_sex` or `having_sex_together` event, ever. The first row of any of
them ends it, with no write anywhere: the next draw reads the row. A `sex_together` she accepts
inside a date places no row and leaves it as it was. The key reaches no prompt; what she is like is written into her
persona by its author, as every other persona fact. `sex_addict` reaches no prompt either.

`hygienic`, `employed` and `outdoorsy` reach no prompt and are not habits: they do not move `indulgence`.
`hygienic` is its own factor, so it multiplies with a needs factor on the same kind. `employed`
leaves `busy` and `in_battle` alone, and changes no cooldown or length.

#### Outdoors or in

Every incident kind is outdoor, indoor or neutral, and `outdoorsy` reads only that. Outdoor
means out in the open air: a shop, a class, an office or a gallery is not outdoor.

| place | `short` | `mid` |
|---|---|---|
| outdoor | `police_encounter`, `animal_encounter`, `on_the_way`, `got_bumped`, `admiring_scenery`, `bird_encounter`, `want_to_see_sea`, `want_hot_spring`, `got_lost`, `missed_ride` | `on_the_road`, `on_a_boat`, `on_a_cruise`, `on_a_bus`, `on_a_train`, `on_a_plane`, `driving`, `want_to_go_out` |
| indoor | `showering`, `bathing`, `scolded_by_superior`, `scolded_superior`, `admiring_art`, `want_sauna`, `using_toilet` | `reading`, `gaming`, `want_to_go_home` |
| neutral | every other `short` kind, the sex kinds and `streaking` among them | every other `mid` kind |

Every kind that can take her offline (`asleep`, `in_battle`, `busy`, `away_from_phone`,
`out_shopping`, the gatherings, `having_sex`) is neutral, so `outdoorsy` never changes how
often she is out of reach. It multiplies with the other factors on the same kind and changes
no gate, cooldown or length, nor how often each category comes up.

Reads are lenient. A value of the wrong type reads as absent; in `prefer_events`, an entry that
is not a known `mid` kind is dropped and the rest are kept. `health` scales the whole `body`
category, `period` included, and `period`'s own 28-day cooldown keeps it near monthly either
way. `prefer_trip` raises the odds of a trip, not its frequency: the weekly category cooldown
holds.

The five habit keys reach the draw through the drift of `indulgence`, and three of them
(`smoking`, `alcoholic`, `drug`) also gate the habit kinds
([below](#getting-lost-her-habits-and-sex)). The judge's
persona block (`[角色人格]`) does not list them, so she does not bring up tobacco, alcohol,
weed, coffee or food at every turn; a persona whose habits should colour her talk says so in her persona text. The engine
never writes them.

**A give of something she does not use.** When a turn's action is a `give` of `cigarette`,
`alcohol`, `herb`, `coffee` or `cuisine` and the matching key (`smoking`, `alcoholic`, `drug`, `coffee`, `gluttony`) is `false`, the
engine rolls once before the judge runs. One time in three the turn goes on as usual.
Otherwise she refuses this give:

- the judge's `[用户动作]` line ends with `，这次你不会收下`, so its `inner_state` is written for
  a refusal; the line states the outcome, not a reason;
- the response in effect is `refuse`, whatever the judge answers, and without the LLM judge the
  rule engine's accept becomes `refuse` too.

It is an ordinary refusal from there: the `action_response` frame and the stored `response`
say `refuse`, the give leaves no lingering effect, the bond line drops one tier
([refused actions](affinity-model.md#refused-actions)), and the reply is written for a
refusal. The judge's own answer stays in its audit payload. `true` and absent never roll, nor
does any other item. A re-driven turn rolls again, as the judge decides again.

A `smoke_together`, `smoke_weed_together` or `drink_together` is rolled the same way, as a give
of one `cigarette`, `herb` or `alcohol`, on the `smoking`, `drug` or `alcoholic` key. Refused,
the `[用户动作]` line ends with `，这次你不会答应` instead, and the chemistry line drops a tier,
not bond. No other action rolls.

### Follow-on factors

Her day hangs together: an event that is **live** makes the kinds that follow it in life
likelier. A prior is live while it is under way, or for 48 hours after it ended. In the
second stage of the draw, a kind weighs 3 times as much for every distinct live prior that
leads to it; two live priors make it 9 times. The factor never lifts a gate (a live `sick`
makes `period_scare` likelier only for a female persona at rung 2 or above), never touches
the first stage, and does not shorten a cooldown.

| live prior | kinds weighed 3 times |
|---|---|
| `ex_message`, `sudden_desire`, `strip` | `having_sex`, `just_have_sex` |
| `sudden_desire` | `strip`, `masturbating` |
| `sick` | `period_scare` |
| `having_sex`, `just_have_sex`, `having_sex_together` | `pregnancy_scare` |
| `business_trip` | `on_the_way`, `busy`, `temp_work` |
| `short_trip`, `long_trip`, `hometown_visit`, `traveling_together` | `on_the_road`, `admiring_scenery`, `taking_selfie` |
| `hometown_visit` | `bros_gathering`, `sis_gathering` |
| `hospitalized`, `post_surgery` | `reading`, `gaming` |
| `period` | `irritable`, `unwell` |
| `breakup`, `quarrel`, `scolded_by_superior`, `in_battle` | `irritable` |
| `got_hit`, `got_bumped` | `unwell` |
| `working_out` | `showering`, `bathing` |
| `ex_message`, `breakup` | `ex_wants_back` |
| `scolded_by_superior`, `temp_work` | `want_to_quit_job` |
| `breakup`, `quarrel`, `want_to_quit_job` | `pondering_life` |
| `pondering_life` | `want_to_see_sea` |

One kind is weighed down rather than ruled out while what leads to it has never happened:
`pregnancy_scare` weighs a fifth while the instance has no `having_sex`, `just_have_sex` or
`having_sex_together` event, ever. There is no hard "only after" gate and no state machine: a deployment where none of them ever happens
keeps the scare at a fifth of its weight rather than losing it. The table and the constants
(`FOLLOW_FACTOR`, `FOLLOW_WINDOW_HOURS`, `UNLIKELY_FACTOR`) are in
`crates/eros-engine-core/src/events.rs`.

### The caller's `kinds`

The caller's `kinds` is the other lever: it is how a persona's world, or a deployment's
policy, rules a kind out without the engine losing it.

| `kinds` | kinds that may be drawn |
|---|---|
| absent or `null` | all 70 |
| `[]` | none: the response is `{"event": null, "message": null}` |
| `["reading", "quarrel"]` | the listed ones |
| `{"allow": […], "disallow": […]}` | what `allow` allows, minus what `disallow` lists |

In the object, `allow` absent or `null` allows every kind and `[]` none; `disallow` absent,
`null` or `[]` removes nothing; a kind in both is not drawn. `{"disallow": […]}` alone rules
kinds out without freezing the rest, so a kind the engine adds later is drawn too. An unknown
key in the object or an unknown kind anywhere rejects the request, so a misspelt `disallow`
cannot quietly serve a kind it meant to exclude; a caller lists a new kind only once the
engine knows it. `taking_space`, `fell_out`, `traveling_together`, `bathing_together`, `on_a_date`, `showering_together`, `sauna_together` and the eighteen kinds of the other together actions are never drawn, so naming one anywhere in
`kinds` is rejected the same way.

Seven kinds carry adult themes: `sudden_desire`, `strip`, `streaking`, `having_sex`, `just_have_sex`, `masturbating`, and `pregnancy_scare`, which implies sex. The
engine keeps no group of them; a caller that serves them only to some users lists them under
`disallow`. `having_sex_together` and `streaking_together` carry them too, but no draw yields
them: only an accepted `sex_together` or `streak_together` does, so such a caller does not send
those actions. `scolded_by_superior` and `scolded_superior` assume someone above her, read in her
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

**In the emotional tone.** While the deployment has
[`tones.emotional`](model-config.md#tones--chat-tones-chat-task-only) above 0, sixteen kinds
are told in the `emotional` tone: `quarrel`, `scolded_by_superior`, `scolded_superior`, `got_hit`, `hit_someone`, `police_encounter`, `ex_wants_back`, `irritable`, `want_to_quit_job`, `unwell`, `sick`, `period`, `sudden_desire`, `drinking`, `smoking_weed` and `just_have_sex`. Their request is then the emotional system prompt
and the cue alone: no history window, no memory recall, no user profile and no affinity
conclusions; `[shared_memories]` holds the relationship's three latest episodes, and `[now]`
keeps the events under way. The message's row carries `metadata.tone = "emotional"`. Every
other kind, and every kind while the tone is off, is told as above.

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
- **Streamed, on request.** The route can answer as a server-sent event stream: the drawn
  event first, then her message as it is generated. The event is still written with the
  message, so the client keeps it only once the message's `done` arrives. See
  [the API reference](api-reference.md#streaming-open-events-and-life-choices).

### While it lasts

The events under way at a moment are the background and the incident whose
`started_at <= moment < ends_at`. They reach four readers in the chat, and the tone draw
([below](#at-work)); with the world system
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
  when nothing is under way, no offline stretch is unexplained, no stored separation awaits its
  return mark and no false alarm is unresolved (below).
- **The reply** gets `- 正在经历：` under `[now]`, after the time, holiday, birthday and anniversary lines,
  listing the same two events with only the end. The `- 刚才：` line below follows it. Both sit
  outside `[character_state]`, whose header defers to the blocks above it, so a story digest
  written before an event started does not override the event:

  ```
  - 正在经历：长途旅游（约 3 天后结束）；在看书（约 2 小时后结束）
  ```

  This works with or without the LLM judge. `/open` greetings and event messages carry the same
  line. The reply prompts of voice calls, group turns and image edits read no events.
- **The image composer** gets one `[此刻状态]` section, after `[身体状态]`, with the labels of the
  background and then the incident, joined by `；`:

  ```
  [此刻状态]
  短途旅游；在坐火车
  ```

  Every picture the chat pipeline draws carries it: a chat turn's, an event message's own, and a
  group turn's, which names its speaker's state. With nothing under way the payload has no such
  section. The event being started counts as under way
  though it is not stored yet: an event message's picture shows the event it tells, and the
  picture of an accepted [together action](#showering-bathing-and-the-sauna-together) shows the
  event it places. The standalone compose endpoint and image edit send no such section. See
  [the composer](model-config.md#taskschat_image_prompt_compose--image-prompt-composer-required-for-image-turns).
  When the chat request carries `user_gender` and one of the two is a kind she shares with the
  user (`traveling_together`, `on_a_date` or any `…_together` kind), a `[对方]` section after it
  gives the user's gender, so a picture that shows the user draws them as it.
- **The caller** gets them from `GET …/effects`: `event` is the incident under way, and
  `background_event` the background. An offline kind under way is reported as `event`
  like any incident, with `"offline": true`; how a client shows it is its call.

A text turn reads the events as of its own user row's send time, so a turn that is driven
again sees the same ones.

### At work

While the deployment has [`tones.focused`](model-config.md#tones--chat-tones-chat-task-only)
above 0, some turns are answered in the `focused` tone: like someone with work in hand. She is
at work when the incident under way is `temp_work`, or when the background is `business_trip`
and no incident is under way. Both kinds are `modern`-only and neither is offline, so only a
modern persona is ever at work.

| the turn | chance it is `focused` |
|---|---|
| she is at work | the value of `tones.focused` |
| off work, `employed` is `"full_time"` ([persona settings](#persona-settings)), no incident under way, any background | 1 in 8, whatever the key's value |
| any other | none |

Only a 1:1 text turn without a user action draws, and a turn that comes up `emotional` stays
`emotional`. A `focused` reply keeps the chat history, the user profile, the memories and the
relationship (`[relationship]`, `[mood]`, `[inner_state]`); it drops `[feelings]` and
`[emotional_context]`, is held to one sentence of at most 40 characters, and its `[this_turn]`
holds one line instead of the rolled nudges:

```
- 你手上有工作，心思在工作上：回得简短利落，先顾手上的事
```

The reply's rows carry `metadata.tone = "focused"`. The draw is in
[model-config.md](model-config.md#tones--chat-tones-chat-task-only).

### Offline kinds

Eight drawable kinds make a persona unreachable while they last: `away_from_phone`, `busy`,
`in_battle`, `asleep`, `bros_gathering`, `sis_gathering`, `having_sex` and, for a female persona only,
`out_shopping`. Offline is how she behaves while the event lasts, not a category:
`busy` is `mid` by length and offline by behaviour. They share the same gates, the same
silence, the same exemptions and the same explanation afterwards; they differ in wording and
in whether she announces them. `in_battle` is worded so that her world decides what it is: a
modern persona reads it as a hard negotiation, a business dinner or a difficult matter, a
historical or fantasy persona as a real fight. A stored `taking_space` is offline for every
persona and rides the same machinery ([stored data of the ask for
space](#where-the-choice-is-stored)). The kinds and their durations are in
[the events route](api-reference.md#post-v2compsessionsession_idevents).

**Announced or silent.** Which kinds are offline, and which are told when they start:

| kind | who goes offline | told |
|---|---|---|
| `away_from_phone`, `asleep` | every persona | never |
| `busy`, `in_battle` | every persona | from intimacy rung 2; silent below |
| `out_shopping` | a female persona; any other persona keeps replying | a female persona from rung 2 and silent below; any other persona at every rung, as an ordinary incident |
| `bros_gathering`, `sis_gathering` | every persona | never; she explains afterwards |
| `having_sex` | a sex addict, the only persona it is drawn for | never; she explains afterwards, in as many words (`在做爱，对方那时发来的消息你现在才看到`); who she was with is hers to say |
| `taking_space` (stored rows only) | every persona | never |

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

With the ghosting switch off, a stretch that is under way silences no turn; turns take the
normal path and the `正在经历` line still lists it. `/open` still returns no greeting while it
lasts. The rest of the mechanics are in
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
persona's `out_shopping` reads `在逛街，对方那时发来的消息你现在才看到`. A stored separation
has two wordings of its own
([model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge)).

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
`saw_bag`, `saw_clothes` and `out_shopping` fit a `modern` persona only ([her world](#her-world)).

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

Five told incidents give her an afternoon the user hears about: `admiring_scenery` and
`admiring_art` (`short`, 1 h), `taking_selfie` (`short`, 30 min), `on_the_road` (`mid`, 2 to
4 h: a long stretch of travel; only on a trip, and only in a `fantasy` world, where a modern
persona has [the vehicles](#on-the-move) instead; `on_the_way` stays the 30-minute errand, also
only on a trip) and `watching_show` (`mid`, 2 to 3 h). She keeps replying through all of
them. The cue states the fact and the details are hers: which view, which piece, which
show, which vehicle. When the request carries `image` and the deployment configures the
composer, a `taking_selfie` message carries the selfie: her words come first and the composer
draws from them (see [api-reference](api-reference.md#post-v2compsessionsession_idevents));
otherwise it is words only. `taking_selfie` is drawn in hospital; the other four need her out
and about.

### She undresses for you

`strip` (`short`, 30 min, weight 1, 48 h cooldown) is her undressing for the user. It needs
intimacy rung 3 and a romance-compatible pair ([gates](#gates)). Sudden desire makes it likelier,
it makes having sex likelier, and `sex_addict` scales it as it scales `sudden_desire`. With
`image` in the request and the composer configured, the message carries her picture, as a
selfie does; her cue says she just undressed and sent it, otherwise that she wants the user to
see. Her clothing in the message is 刚把衣服脱了, or 刚把「{outfit}」脱了 over the request's
`outfit`, so the picture never shows her dressed. It carries adult themes, like
`sudden_desire`; a caller that serves those only to some users disallows it.

### Showering, bathing and the sauna together

The user can ask to take a shower, a bath or a sauna with her: the `shower_together`,
`bath_together` and `sauna_together` actions, none of which carries a field
([the send route](api-reference.md#post-compchatsession_idmessagestream)). Eighteen more together
actions, also without a field, ask her to do something else with the user (the table below). The
judge decides, as for any action. Before it, a shower, a bath, a `sex_together` or a
`streak_together` is ruled out for a pair whose `affinity_scope` is not romance-compatible, as
`strip` is, a `toilet_together` for a machine persona ([persona settings](#persona-settings)),
and an action whose event does not fit her world ([her world](#her-world)): a `fight_together`
for a persona that is not `fantasy`, a `cycle_together` for one that is. She refuses whatever
the judge answers, with or without the LLM judge. The other together actions
have no such rule; `smoke_together`, `smoke_weed_together` and `drink_together` take the habit
roll (below). A refusal places nothing and drops the chemistry line a tier, but an action her
world rules out drops no line; her own
`bathe_together` ask ([a bath together](#a-bath-together)) and its cooldown neither read these
actions nor are read by them.

An accepted one puts her in an event, written in the same transaction as the stored answer:

| action | event | label | lasts |
|---|---|---|---|
| `shower_together` | `showering_together` | 和对方一起淋浴 | 30 min |
| `bath_together` | `bathing_together` | 和对方一起洗澡 | 30 min |
| `sauna_together` | `sauna_together` | 和对方一起蒸桑拿 | 1 h |
| `toilet_together` | `using_toilet_together` | 和对方一起上厕所 | 30 min |
| `smoke_together` | `smoking_together` | 和对方一起抽烟 | 30 min |
| `fall_asleep_together` | `asleep_together` | 和对方睡在一起 | 1–4 h |
| `sex_together` | `having_sex_together` | 和对方上床 | 1 h |
| `walk_together` | `walking_together` | 和对方一起散步 | 1 h |
| `smoke_weed_together` | `smoking_weed_together` | 和对方一起抽大麻 | 30 min |
| `jog_together` | `jogging_together` | 和对方一起慢跑 | 1 h |
| `yoga_together` | `yoga_together` | 和对方一起做瑜伽 | 1 h |
| `stargaze_together` | `stargazing_together` | 和对方一起看星星 | 1 h |
| `ride_horse_together` | `horse_riding_together` | 和对方一起骑马 | 1 h |
| `streak_together` | `streaking_together` | 和对方一起裸奔 | 30 min |
| `drink_together` | `drinking_together` | 和对方一起喝酒 | 1 h |
| `hot_spring_together` | `hot_spring_together` | 和对方一起泡温泉 | 1 h |
| `fight_together` | `fighting_together` | 和对方并肩作战 | 1–2 h |
| `workout_together` | `working_out_together` | 和对方一起健身 | 1 h |
| `swim_together` | `swimming_together` | 和对方一起游泳 | 1 h |
| `cycle_together` | `cycling_together` | 和对方一起骑单车 | 1 h |
| `hike_together` | `hiking_together` | 和对方一起爬山 | 2–3 h |

Each is a `short` incident but `asleep_together`, `fighting_together` and `hiking_together`,
`mid` ones whose length is drawn in whole hours. Starting it ends an incident under way and leaves a background going, as the bath she asks for
does. The event is keyed on the user's message, so a re-driven turn places it once. While a date
of hers is on, nothing is placed: the date's `on_a_date` stays as it is and the date goes on.
She does not tell the event, since her reply to the action already does; none is offline, so she
still answers during `asleep_together`, and none of the twenty-one is drawn. While it lasts it is the
incident under way, so `正在经历`, the judge and the picture composer read it like any incident.
`showering_together`, `bathing_together`, `having_sex_together`, `streaking_together`,
`using_toilet_together` and `asleep_together` stay out of the story director's events, as the
outcome of her sleep ask does; the other fifteen reach it.

`smoke_together`, `smoke_weed_together` and `drink_together` have her take one `cigarette`,
`herb` or `alcohol`, and count as a give of it for three things only: the habit roll
([persona settings](#persona-settings)), the needs an accepted one moves
([gives move needs](#gives-move-needs)) and the lingering effect it leaves
([the send route](api-reference.md#post-compchatsession_idmessagestream)). A refusal drops
chemistry, not bond, and no moment is written to [the journal](memory-layers.md#the-journal).

`sex_together` is her own `sleep_together` ask made by the user. An accepted one settles her ask
for the session ([the romantic choices](#the-romantic-choices)), and her accepted
`sleep_together` places the same event. That event is sex: it ends
her virginity ([persona settings](#persona-settings)) and makes a pregnancy scare likelier
([follow-on factors](#follow-on-factors)). The user may ask again after an accept; the judge
decides each time. `fall_asleep_together` is likewise her own `fall_asleep_together` ask made by
the user: an accepted one settles that ask for the session, and her accepted
`fall_asleep_together` places the same `asleep_together`.

An accepted together action is a picture turn when the request carries `image` and the
deployment has the composer, like an accepted `photo`, `strip` or `ask_dancing`: her words, then
one `image_request`. The composer is told the event as her state, so the picture shows it; inside
a date, where nothing is placed, the action's kind is her state for that turn only. A refused one
is text. While the deployment has [`tones.emotional`](model-config.md#tones--chat-tones-chat-task-only)
above 0, the reply to a shower, a bath, a `sex_together` or a `streak_together` is in the
`emotional` tone, accepted or refused; the other together actions' is not.

### Small things in her day

Eight told incidents fill in her day. All are `short`; none takes her offline or carries a
picture, and all but `want_to_quit_job` fit both worlds ([below](#her-world)):

| kind | label | category | weight | cooldown | lasts | gate | cue |
|---|---|---|---|---|---|---|---|
| `bird_encounter` | 遇到一只鸟 | `short` | 2 | 12 h | 30 min | none | `（你刚看到一只鸟停在不远处。）` |
| `ex_wants_back` | 前任来求复合 | `short` | 0.5 | 30 days | 1 h | intimacy rung 2 | `（你的前任刚来找你，说想复合。）` |
| `want_to_quit_job` | 想辞职 | `short` | 1 | 7 days | 1 h | `employed` is not `"free"` | `（你刚冒出想辞职的念头。）` |
| `pondering_life` | 在思考人生 | `short` | 2 | 48 h | 1 h | none | `（你刚一个人发呆，在想人生的事。）` |
| `streaking` | 刚裸奔了一圈 | `short` | 0.5 | 30 days | 30 min | intimacy rung 3, a romance-compatible pair | `（你刚一时兴起，脱光了跑了一圈。）` |
| `want_to_see_sea` | 想去看海 | `short` | 2 | 72 h | 1 h | none | `（你现在很想去看海。）` |
| `want_hot_spring` | 想去泡温泉 | `short` | 1 | 72 h | 1 h | none | `（你现在很想去泡温泉。）` |
| `want_sauna` | 想去洗桑拿 | `short` | 1 | 72 h | 1 h | none | `（你现在很想去洗桑拿。）` |

The cue states the fact and the details are hers: which bird, what the ex said, what she does
about the job. A high `outing` weighs `want_to_see_sea` twice
([below](#1-needs-weight-the-kind-pick)); `hygienic` scales `want_hot_spring` and `want_sauna`
with the shower and the bath ([persona settings](#persona-settings)). One thing leads to another: her ex texting or a
breakup makes the ex likelier to want her back, a scolding or a work call makes her want to
quit, a breakup, a quarrel or the wish to quit sets her thinking about her life, and that makes
her want to see the sea ([follow-on factors](#follow-on-factors)). In hospital or after surgery
she meets no bird and does not streak; the wants and the thoughts come there too.

`streaking` carries adult themes, like `strip`: `sex_addict` scales it as it scales
`sudden_desire`, and a caller that serves those only to some users disallows it.

### Getting lost, her habits and sex

Six more told incidents. All are `short`; none takes her offline or carries a picture, and all
fit both worlds ([below](#her-world)):

| kind | label | category | weight | cooldown | lasts | gate | cue |
|---|---|---|---|---|---|---|---|
| `got_lost` | 迷路了 | `short` | 3 | 48 h | 1 h | a `short_trip`, `long_trip`, `hometown_visit` or `traveling_together` under way | `（你刚迷了路，找不到要去的地方。）`; during `traveling_together`, `（你们一起出来玩，刚迷了路。）` |
| `drinking` | 在喝酒 | `short` | 2 | 12 h | 30 min | `alcoholic` is `true` | `（你正在喝酒。）` |
| `smoking` | 在抽烟 | `short` | 2 | 12 h | 30 min | `smoking` is `true` | `（你正在抽烟。）` |
| `smoking_weed` | 在抽大麻 | `short` | 2 | 12 h | 30 min | `drug` is `true` | `（你正在抽大麻。）` |
| `just_have_sex` | 刚做完爱 | `short` | 1 | 48 h | 30 min | intimacy rung 3 | `（你刚和人做完爱。）` |
| `masturbating` | 自慰中 | `short` | 1 | 24 h | 30 min | `sex_addict` is `true`, and intimacy rung 3 | `（你正在自慰。）` |

`got_lost` needs a background ([above](#one-incident-and-one-background-at-a-time)):
on her own trips its cue opens with the trip's state, as any incident drawn during a background
does (`（你正在短途旅行。你刚迷了路，找不到要去的地方。）`); on the trip with the user it is the
two of them who are lost.

The habit kinds follow the habit keys ([persona settings](#persona-settings)): she drinks,
smokes or smokes weed only if her card says she does, and a high `indulgence` weighs each of the
three 3 times ([below](#1-needs-weight-the-kind-pick)). Coffee and good food have no event.

`just_have_sex` is sex in her life, told afterwards: who it was with is hers to say. It reads
only the rung, with no romance-compatibility check. `having_sex`, the offline stretch she
explains afterwards, is drawn only for a persona whose `sex_addict` is `true`; any other persona
has only `just_have_sex`, from rung 3. `sex_addict` weighs `just_have_sex` 3 times or a third, as
it does `sudden_desire`; while she is a virgin it weighs a fifth, as `having_sex` does, and the
first row of either kind, or of `having_sex_together`, ends her virginity ([persona settings](#persona-settings)). Her ex texting, sudden desire and her undressing make it likelier, and it makes a
pregnancy scare likelier, as `having_sex` does; sudden desire makes `masturbating` likelier
too ([follow-on factors](#follow-on-factors)).

In hospital or after surgery she does not drink, smoke, smoke weed or have sex, and she is on
no trip to get lost on; she can still masturbate. On the trip with
the user she does not draw `just_have_sex`: she is away with the user.

`just_have_sex` and `masturbating` carry adult themes; a caller that serves those only to some
users disallows them.

### Missing her ride

| kind | label | category | weight | cooldown | lasts | gate | cue |
|---|---|---|---|---|---|---|---|
| `missed_ride` | 没赶上车 | `short` | 3 | 48 h | 1 h | a `business_trip`, `short_trip`, `long_trip`, `hometown_visit` or `traveling_together` under way | `（你刚没赶上车。）`; during `traveling_together`, `（你们一起出来玩，刚没赶上车。）` |

Told, never offline, no picture, both worlds (a carriage is a ride too). Unlike `got_lost`, a
business trip counts.

### Using the toilet

| kind | label | category | weight | cooldown | lasts | gate | cue |
|---|---|---|---|---|---|---|---|
| `using_toilet` | 上厕所中 | `short` | 1 | 12 h | 30 min | she is not a machine ([persona settings](#persona-settings)) | `（你正在上厕所。）` |

Told, never offline, no picture, both worlds, no intimacy gate. She keeps replying. It is an
indoor kind, so `outdoorsy` scales it ([below](#outdoors-or-in)); no background rules it out, no
event makes it likelier and no needs factor touches it.

### On the move

Six `mid` kinds say how she is travelling. Each is drawn only while a trip is under way:

| kind | label | weight | cooldown | lasts | world | cue | during `traveling_together` |
|---|---|---|---|---|---|---|---|
| `on_a_boat` | 在船上 | 1 | 48 h | 1–2 h | both | `（你正在坐船。）` | `（你们一起出来玩，正在坐船。）` |
| `on_a_cruise` | 在游轮上 | 1 | 48 h | 2–4 h | `modern` | `（你正在游轮上。）` | `（你们一起出来玩，正在游轮上。）` |
| `on_a_bus` | 在坐大巴 | 1 | 48 h | 2–4 h | `modern` | `（你正在坐大巴。）` | `（你们一起出来玩，正在坐大巴。）` |
| `on_a_train` | 在坐火车 | 1 | 48 h | 2–4 h | `modern` | `（你正在坐火车。）` | `（你们一起出来玩，正在坐火车。）` |
| `on_a_plane` | 在飞机上 | 1 | 48 h | 2–4 h | `modern` | `（你正在飞机上。）` | `（你们一起出来玩，正在飞机上。）` |
| `driving` | 在开车 | 1 | 48 h | 1–3 h | `modern` | `（你正在开车。）` | `（你们一起出来玩，正开车在路上。）` |

Any trip counts, a `business_trip` included, except for `on_a_cruise`, which comes up only on a
`short_trip`, `long_trip`, `hometown_visit` or `traveling_together`. During her own trip the cue
opens with the trip: `（你正在短途旅行。你正在坐火车。）`. All six are told, never offline, carry
no picture and are outdoor; none has a gate beyond the trip, and each keeps its own cooldown, so
a train can follow a plane on one trip. Being `mid`, each can be named in `prefer_events`.

They take the place of `on_the_road` for a `modern` persona: the six together weigh what
`on_the_road` weighed on a leisure trip, and five sixths of it on a business trip. A `fantasy`
persona draws `on_the_road` and `on_a_boat`.

### Her world

`art_metadata.setting` says which world she lives in: `modern` (the default, and what any other
value reads as) or `fantasy`. A kind that does not fit her world is never drawn for her,
whatever the request's `kinds` or her `prefer_events` say:

| fits | kinds |
|---|---|
| `modern` only | `police_encounter`, `showering`, `taking_selfie`, `gaming`, `watching_show`, `business_trip`, `hospitalized`, `post_surgery`, `temp_work`, `scolded_superior`, `on_a_cruise`, `on_a_bus`, `on_a_train`, `on_a_plane`, `driving`, `saw_bag`, `saw_clothes`, `out_shopping`, `want_to_quit_job`, `admiring_art`, `cycling_together` |
| `fantasy` only | `on_the_road`, `fighting_together` |
| both | every other kind |

`on_the_road` is the one drawn `fantasy`-only kind: a modern persona travels by [vehicle](#on-the-move)
instead. A modern persona whose `prefer_events` lists it keeps a value that does nothing, and so
does a fantasy persona whose list names `out_shopping`. `fighting_together` and
`cycling_together` are never drawn: a together action whose event does not fit her world is
refused before the judge and costs nothing, so only a
`fantasy` persona can accept the user's `fight_together` and only a `modern` one the user's
`cycle_together` ([together actions](#showering-bathing-and-the-sauna-together)).

### Going out and going home

Two told incidents put her `outing` need into her day:

| kind | label | category | weight | cooldown | lasts | cue |
|---|---|---|---|---|---|---|
| `want_to_go_out` | 想出门 | `mid` | 1 | 24 h | 1 to 3 h | `（你现在很想出门走走。）` |
| `want_to_go_home` | 想回家 | `mid` | 1 | 24 h | 1 to 3 h | `（你现在很想回家。）` |

She is not offline and keeps replying. There is no gate, and both are drawn inside every
background, hospital included, where the cue opens with the background as every incident's
does: `（你正在住院。你现在很想出门走走。）`. Both join `prefer_events`' vocabulary like any
`mid` kind. A high `outing` weighs `want_to_go_out` 3 times and a low one `want_to_go_home`;
a high `indulgence` weighs `want_to_go_out` twice ([below](#1-needs-weight-the-kind-pick)).

### Next to `/open`

An event's message counts as a message for the `/open` birthday and holiday check, so neither
greeting follows it on the same local day. An opener that is being generated when an event
message lands yields and returns `null`; an event does not yield to an opener. A greeting
carries the `正在经历` and `刚才` lines and no needs line, and `/open` returns `null` while an
offline kind or a date is under way.

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
- Stored `taking_space` and `fell_out` rows are rows of `persona_events` like any other, with
  `message_id` NULL; the choice that wrote them lives in `engine.life_choices`
  ([below](#where-the-choice-is-stored)).

### Beyond the chat

With [World Stories](world-system.md#world-stories) enabled, the story director reads the
instance's events directly, each with its label, a relative time and the message she told it
in, plus the background under way, the pair's relationship status and cold war, and its
`confess`, `partner_up`, `propose` and `break_up` choices (and stored `ask_for_space` ones) with
their outcome. It takes them as settled fact. A bath together, a shower together, a date, and
`having_sex_together`, `streaking_together`, `using_toilet_together` and `asleep_together` are
left out: like the outcome of her sleep, bath and trip asks, they live in the conversation only. The messages she sends on her own are not part of its chat evidence, so
an event is never read as her answer to an unrelated line. The World Memories director reads a
background under way for every persona in the roster, stories or not, so a persona in hospital
is not scripted into a party. Neither director writes events or changes the persona settings
above. Needs reach neither.

## How the two interact

The needs factors below are second-stage factors; the others ([follow-on
factors](#follow-on-factors), `prefer_events`, `is_virgin`, `homesick`, `sex_addict`, `hygienic`, `employed`) read no need.

### 1. Needs weight the kind pick

A needs factor scales a kind's weight within its category, in the second stage of the draw. It
steers which kind comes up, never whether a category does. The factors, from
`needs_factor` and `indulgence_factor` in `crates/eros-engine-core/src/events.rs`:

| need level | kinds | factor |
|---|---|---|
| `emotional` is `high` | `ex_message` | ×3 |
| `emotional` is `high` | `animal_encounter`, `irritable`, `quarrel` | ×2 |
| `outing` is `high` | `want_to_go_out` | ×3 |
| `outing` is `high` | `want_to_see_sea` | ×2 |
| `outing` is `low` | `want_to_go_home` | ×3 |
| `indulgence` is `high` | `want_to_go_out`, `irritable` | ×2 |
| `indulgence` is `high` | `drinking`, `smoking`, `smoking_weed` | ×3 |

The `indulgence` rows are a factor of their own and multiply with the rows above: `outing` and
`indulgence` both `high` weigh `want_to_go_out` ×6, and `indulgence` and `emotional` both
`high` weigh `irritable` ×4. `social` weights nothing. The factors apply only when the
deployment runs the judge and the persona has a rating, carried to the moment of the call;
otherwise every factor is 1. A factor is not a gate: a kind weighted ×3 still has to be
eligible.

### 2. Events never change a stored need

Nothing writes a need from an event. The judge reads the `[近期事件]` line, and a give's
`[用户动作]` and later `[身体状态]` lines, and rates the needs itself with them in view; the
reference judge prompt in `examples/model_config.toml` tells it to start from `[需求]` and to
take the conversation, `[近期事件]`, `[身体状态]` and the elapsed time into account. If the
judge abstains (`needs: null`), the previous rating keeps drifting whatever happened. The one
write that is not the judge's is an accepted give's move of two needs
([Gives move needs](#gives-move-needs)), and it replaces the judge's rating of just those.

### 3. An offline stretch writes no rating

A turn silenced by an offline kind runs no judge, so it rates nothing. The last rating keeps
drifting through the stretch, and the first judged turn afterwards is shown it carried across
all of it: after a long stretch she reads lonelier than before.

Put together, the two form a loop with the judge in the middle: a persona who wants to go out
is likelier to draw `want_to_go_out`; the event reaches the judge's context; the judge rates
`outing` again.

## Life choices

A *life choice* is a real request she puts to the user, with a real cost on some answers. It is
built from the machinery above: her ask is a proactive message, an accepted ask can place an
event, and the user answers through one endpoint. There are eight kinds:

| kind | she asks the user | rules |
|---|---|---|
| `confess` | to be together | [the romantic choices](#the-romantic-choices) |
| `partner_up` | to become partners | [the romantic choices](#the-romantic-choices) |
| `propose` | to marry her | [the romantic choices](#the-romantic-choices) |
| `break_up` | to break up | [the romantic choices](#the-romantic-choices) |
| `sleep_together` | to sleep with her | [the romantic choices](#the-romantic-choices) |
| `fall_asleep_together` | to fall asleep side by side, and no more | [the romantic choices](#the-romantic-choices) |
| `travel_together` | to go away together | [going away together](#going-away-together) |
| `bathe_together` | to take a bath together | [a bath together](#a-bath-together) |

The first four are the only things that move the pair's
[relationship status](affinity-model.md#relationship-status). Over any status the pair can fall
into a [cold war](#the-cold-war), which no one asks for.

### When she asks

The engine checks at the end of every 1:1 text turn whose affinity evaluation committed (a
*judged turn*: the affinity evaluator ran and its verdict landed, whether or not the deployment
runs the PDE judge), after the commit. It first starts or ends a [cold war](#the-cold-war), then
checks the kinds in this order: `break_up`, `confess`, `partner_up`, then `propose` or her sleep
ask, then `bathe_together`, then `travel_together`. Each kind has a trigger of its own, read from
the committed, time-decayed state, and every kind also needs:

- **A fresh streak.** Only judged turns after the session's *streak anchor* count. The anchor is
  the latest of its newest choice's `asked_at` and `resolved_at` (for a stored accepted
  `ask_for_space`, the end of its separation) and its newest `relationship` event, which every
  change of status or cold war writes. A judged action turn that fell while she waited for an
  answer that came belongs to that episode and counts for nothing; a lapsed ask anchors at
  `asked_at`, so action turns during its wait do count. The turn that converts an ignored ask is
  the first of the new streak. Asking consumes the streak: the next ask needs fresh judged turns.
- **A clear runway.** No session of the persona has a pending choice, and the persona has no
  [date](activities.md) and no stored separation (`taking_space`) under way. She has one life, so at most one ask is pending at a time: once
  one kind has asked, the later checks of the same turn find the runway closed. A date's own turns
  are never judged turns, so they never trigger one.
- **The right session.** A 1:1 text session of a genome persona. A voice session never triggers,
  delivers or resolves a choice, and a group session writes no affinity at all. A BYOC session
  never triggers one, because the resolution endpoint refuses BYOC.

The triggers are engine math over committed state; no judge takes part.

### Her ask and its delivery

When a trigger fires, in order:

1. **Her words.** The companion chat model generates the request through the same proactive
   path as an event message (persona, history window, memory recall, `[now]`; no judge, no
   streaming, no user row), under the scopes the pair's replies recorded, the ones her ordinary
   replies read; a pair with none recorded reads as romance-compatible. The stage cue is the final `user`
   turn and is never persisted; the cue of every kind is in
   [model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge). The message is an
   ordinary assistant row, `assistant_action_type = 'proactive'`,
   `metadata.proactive = 'life_choice'`, unread until the read call, with no post-process.
2. **The choice row.** One row in `engine.life_choices`, written in the same transaction as her
   message: the kind, status `pending`, and `asked_at`, which is the message's `sent_at`. The
   pending window is 24 hours; the expiry is `asked_at` plus 24 hours, derived and never stored.
   The write yields to a choice already pending in the persona, and to a streak anchor that moved
   since the trigger read it.

A blank or stripped-empty generation, or a failed one, writes nothing: no message, no choice
row, and a later qualifying turn may ask again. The check runs last in the turn's post-process,
after the affinity commit and the feeling clause, so its generation delays neither, and the
turn's own affinity write (state and event row, one transaction) is untouched. The pair's
recorded scopes decide eligibility and nothing else; her replies at resolution use the
resolution request's own scopes.

While a choice is pending and unexpired, the events route draws nothing for the persona. An
incident that was already under way when she asked keeps running until the answer; an accept
that places an event, and an accepted `confess`, `partner_up` or `propose`, caps it.

**Delivery.** The notification rides the per-turn affinity delta fetch,
[`GET /bff/v1/comp/affinity/{session_id}/event`](api-reference.md#get-bffv1compaffinitysession_idevent).
Its `event` object gains one optional field, `life_choice` (`id`, `kind`, `options`,
`asked_at`, `expires_at`, `message_id`), composed at serve time from the session's pending,
unexpired choice and absent otherwise. The ask lands seconds after the triggering turn's event
commits, so the fetch the turn itself releases usually misses it: an adapted client re-fetches
the delta when a history row carries `occasion: "life_choice"`. Both history routes
([canonical](api-reference.md#get-compchatsession_idhistorylimit20offset0) and
[BFF](api-reference.md#get-bffv1compchatsession_idhistorylimit50offset0)) expose a proactive
assistant row's `metadata.proactive` as an optional `occasion`, omitted on every other row. Her
ask row also carries a `life_choice` receipt (`id`, `kind`, `status`, `resolved_at`) on both
history routes and in the BFF start bundle, so a reload shows how the choice ended. Neither the
tier drop nor the status move of an answer is served on this route, so a client never meets an
unknown `event_type`; the new state comes back in the resolution response, and any other device
reads it from the absolute affinity route.

**Degradation is the design.** A downstream that has not adapted ignores the unknown field and
renders the ask as an ordinary proactive message, and its user replies in words. That is the
ignore path below. Nothing breaks; the choice UI is strictly additive.

### The user's answer

The answer is `POST /v2/comp/session/{session_id}/life-choices/{choice_id}` with
`{"response": "accept" | "decline"}`
([API reference](api-reference.md#post-v2compsessionsession_idlife-choiceschoice_id)),
or the user's own words, or silence:

| path | how it happens | choice status | cost |
|---|---|---|---|
| accept | the endpoint, `accept` | `accepted` | none; what it starts or moves depends on the kind |
| decline | the endpoint, `decline` | `declined` | a tier drop for some kinds |
| ignore | the first ordinary user message while it is pending | `ignored` | none, and no event |
| lapse | 24 hours with no answer and no message | stays `pending`, reads as expired | none |

**Accept and decline.** One transaction answers the choice and pays for it: the event an accept
places, the status it moves and the line a decline drops. What each kind does is in its section.
She then answers in one proactive message, `metadata.proactive = 'life_choice'`, generated with
the request's scopes and returned; a failed or empty reply leaves the answer and everything it
did in place. The tier drop's affinity event and its context keys are in
[affinity-model.md](affinity-model.md#life-choice-drops-and-the-cold-war-halving).

**A status that moved.** An action turn leaves her pending ask open, so the status can move
while the ask waits: her `propose` is pending and the user's own `break_up` is accepted. An
answer to `confess`, `partner_up`, `propose` or `break_up` that arrives when the status is no
longer one the kind is asked from is refused with `409` and writes nothing; the ask lapses on its
own. Only the status is read: a `break_up` asked in a cold war that has since ended can still be
answered.

**Ignore.** The first ordinary user text message that arrives while the choice is pending
resolves it: the status becomes `ignored`, at no cost and with no event, whatever the kind. No
tier changes; the judge reads the user's words like any other turn's and rates the turn itself.
The clock is the user row's `sent_at`: only a message sent after her ask converts it (one already
in flight when the ask landed answers nothing), and a queued or re-driven turn reads the same. An
**action turn** (a give, a tip, a kiss) does not resolve the choice: it is answered by its own
rules and the ask stays pending, as a gift while she waits is an attempt to mend, not an answer.
`/open` greetings and reactions resolve nothing.

**Lapse.** With no answer and no message for 24 hours the choice simply lapses. Its status stays
`pending` in the table and reads as expired everywhere: the delta field stops being attached,
the endpoint answers `409`, and a later message converts nothing. There is no cost; a later ask
needs its full trigger again.

A resolution and an incoming user message race transactionally. The first writer wins and the
loser writes nothing (the endpoint answers `409`).

The endpoint can answer as a server-sent event stream: a `resolved` frame with the committed
answer and the new affinity first, then her reply as it is generated. See
[the API reference](api-reference.md#streaming-open-events-and-life-choices).

### The cold war

When the user wears her patience out, the pair falls into a **cold war**. It is a flag on the
session's affinity row, over whatever status the pair has; ending it uncovers the status
underneath unchanged ([affinity-model.md](affinity-model.md#the-cold-war)). No one asks for it.

**When it starts.** After every judged 1:1 text turn, before her asks are checked, the engine
reads the judge's patience levels (`patience_grade`, 1 to 3, from each judged turn's
`state_after`) of the judged turns after the streak anchor, newest first. A cold war starts when
the newest three are all at the lowest level, 1, or when at least five of the newest seven are.
It also starts when a break-up is refused, hers or the user's, unless one is already on. No tier
gap, compatibility or status is read.

The levels are the judge's own, not the derived `patience`: the derived value is the level
boosted by the bond line, so with a low bond even the top level stays under 0.5, and a threshold
on it could hold a new relationship in a cold war for good. A turn whose `state_after` lacks the
level reads as the middle level, 2.

**When it ends.** When the newest three judged turns after the anchor are all above the lowest
level, or when a break-up is accepted. Each start and each end writes a `relationship` event,
which moves the streak anchor: the turns that started a cold war do not count toward ending it,
and its end starts a fresh streak for every ask.

**What she says.** When a streak starts it, she says so in one proactive message through the same
path as an ask, under the engine's default scopes and with no choice row:
`metadata.proactive = 'cold_war'` (the history routes'
`occasion`), on the cue

```
（你对对方的耐心被磨光了。你决定不再给对方好脸色，告诉对方你现在不想好好说话。）
```

The flag is committed first: a failed or blank message leaves the cold war on, and she says
nothing. A cold war started by a refused break-up has no message of its own, her refusal being
that turn's reply, and ending one writes no message.

**While it lasts.**

- She keeps replying, coldly: the reply's `[relationship]` block ends with `你们正在冷战`, and
  the judge reads `冷战中=是`.
- Every positive committed delta on the four line axes is halved, on every judged turn of the
  session and on a date's settlement ([affinity-model.md](affinity-model.md#life-choice-drops-and-the-cold-war-halving)).
  Nothing drops when it starts.
- She asks for nothing warm: no `confess`, `partner_up`, `propose`, `sleep_together`,
  `fall_asleep_together`, `bathe_together` or `travel_together`.
- From a breakable status she can ask to break up ([below](#the-romantic-choices)).

A pending choice or a date under way holds a cold war back neither from starting nor from ending.
Only a 1:1 text session of a genome persona has one; a BYOC session stays at `none` with no cold
war.

### The romantic choices

Six kinds follow the relationship itself. Four move its status: `confess`, where she confesses
and asks to be together; `partner_up`, where she offers to become partners; `propose`; and
`break_up`. The other two are her ask to spend the night: `sleep_together`, for sex, or
`fall_asleep_together`, to fall asleep side by side and no more. The pair's status decides which
of the two she asks.

**Who qualifies.** `confess`, `propose`, `sleep_together` and `fall_asleep_together` are asked
only of a pair the caller has marked romantically compatible through the `affinity_scope` it
sends (the criterion is in the [API
reference](api-reference.md#post-v2compsessionsession_idlife-choiceschoice_id)). A pair that does
not meet it, or has no recorded scopes, is never asked them. `partner_up` and `break_up` do not
read the scopes: an incompatible pair whose chemistry leads is asked nothing, and once bond
overtakes, she offers a partnership. A status already reached is not undone when the scopes
change. The engine takes no gender or orientation and decides nothing about who is attracted to
whom.

**The status they are asked from.**

| kind | asked from |
|---|---|
| `confess` | `none`, `ex`, `partner`, `peer` |
| `partner_up` | `none`, `ex`, `peer` |
| `propose` | `dating`, `fwb`, `situationship` |
| `break_up` | a breakable status (`dating`, `partner`, `fwb`, `situationship`, `engaged`) while a cold war is on |
| `sleep_together` | `dating`, `engaged`, `fwb`, `situationship`, and `partner` at chemistry tier 4 or more |
| `fall_asleep_together` | `none`, `peer`, `ex`, and `partner` at chemistry tier 3 |

So `dating`, `engaged`, `fwb` and `situationship` are never offered a partnership, even when bond
later overtakes chemistry, and `engaged` is asked neither to confess nor to marry.

**When she asks.** The checks run where every choice's do, over the same committed, time-decayed
state and the same clear runway and right session; none of the warm ones while a cold war is on.
The *leading* line is the one with the higher tier; on equal tiers, the higher score, and an exact
tie is bond's. A judged turn is *warm* when its derived `warmth` and `patience` are both above
0.7, read from the turn's `state_after`; 0.7 is a rule threshold the engine owns, not a judge
band.

| kind | tiers | judged turns | waits |
|---|---|---|---|
| `break_up` | none | the newest 3 all at the judge's lowest patience level | none beyond a fresh streak |
| `confess` | chemistry leads, at tier 3 or more | the newest 3 all with `warmth` above 0.7 | 14 days from the session's latest status change, and from `confess`'s last answer of any kind |
| `partner_up` | bond leads, at tier 3 or more | the newest 3 all with `patience` above 0.7 | 14 days from the session's latest status change, and from `partner_up`'s last answer of any kind |
| `propose` | chemistry and bond at tier 4 or more | at least 5 of the newest 8 warm | 14 days from an accepted or ignored `propose`; after a decline or a lapse, a fresh streak |
| `sleep_together`, `fall_asleep_together` | chemistry tier 3 or more | the newest 3 all warm | never again once settled (below) |

`confess` reads warmth alone and `partner_up` patience alone: warmth is boosted by chemistry and
patience by bond, so with the other line under about 0.415 a warm turn cannot happen, and a
clearly one-sided pair would never be asked. A *status change* is a `relationship` event whose
status moved; a cold war starting or ending is not one. An answer of any kind is an accept, a
decline or an ignore; a lapse is not. The turn that starts a cold war moves the streak anchor, so
`break_up` needs three more turns at the lowest level after it. When `propose` and her sleep ask
both qualify on one turn, `propose` fires.

The four status kinds can be asked again whenever their status, wait and streak allow, so after a
break-up the pair can go round again. Her sleep ask is once only, each kind on its own: a kind the
user accepted or ignored in the session never fires there again, nor does it once she has accepted
the user's own ask of it in the session or in a date of it, `sex_together` for `sleep_together`
([together actions](#showering-bathing-and-the-sauna-together)) and `fall_asleep_together` for
`fall_asleep_together`. A refused one settles nothing, and her ask already pending stays as it is,
to be answered or to lapse. A pair once asked to fall asleep together can still be asked for sex
once its status qualifies.

**The user's `propose` and `break_up`.** Both carry no field and are not together actions: they
place no event and are not picture turns of their own. Before the judge, the user's `propose` is
refused unless the status is `dating`, `fwb` or `situationship`, and the user's `break_up` unless
the status is breakable; such a refusal has no effect, and the judge is told the outcome.
Otherwise the judge decides: an accepted `propose` makes the pair `engaged`; an accepted
`break_up` makes them `ex` and ends a cold war, and a refused one keeps the status and starts a
cold war if none is on. A refusal drops no line. A turn whose judge failed accepts, as for any
action, and the status moves. The status moves in the transaction that stores
the answer, on the turn's session, or on a date on the session the invitation was sent in. On a
BYOC instance no status moves: the user's `propose` goes to the judge from any status and moves
nothing, and `break_up` is always refused. The user may ask again; the judge decides each time.
While the deployment has [`tones.emotional`](model-config.md#tones--chat-tones-chat-task-only)
above 0, her reply to either is in the `emotional` tone, accepted or refused.

**Her ask.** It is delivered as every ask is ([above](#her-ask-and-its-delivery)), with no tier
gap. The stage cues are in
[model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge).

**The answer.** The same endpoint, the same `409` rules, a `409` too when the status has moved
([above](#the-users-answer)), and the same response shape.

| kind | accept | decline |
|---|---|---|
| `confess` | `none` or `ex` becomes `dating`, `partner` becomes `fwb`, `peer` becomes `situationship`; an incident under way ends | the status stays; chemistry drops one tier |
| `partner_up` | the pair becomes `partner`; an incident under way ends | the pair becomes `peer`; nothing drops |
| `propose` | the pair becomes `engaged`; an incident under way ends | the status stays; bond and chemistry each drop one tier |
| `break_up` | the pair becomes `ex` and the cold war ends | the status stays and the cold war goes on, or starts if it had ended; nothing drops |
| `sleep_together` | a one-hour `having_sex_together` starts now, returned as `event`, as the user's accepted `sex_together` does; an incident under way ends | chemistry drops one tier |
| `fall_asleep_together` | an `asleep_together` of one to four hours starts now, returned as `event`, as the user's accepted `fall_asleep_together` does; she keeps replying, and an incident under way ends | chemistry drops one tier |

An accept costs nothing and leaves a background running; only `sleep_together` and
`fall_asleep_together` return an `event`. An ignore and a lapse cost nothing, whatever the kind.
A status move is written with a `relationship` event in the same transaction
([affinity-model.md](affinity-model.md#relationship-status)). A decline's drop is the landing
rule of a refused action applied to the lines named, in the same transaction as the resolution,
and its pending deltas are cleared. An action turn resolves nothing.

The story director reads the four status kinds with their outcome, and the pair's status and
cold war, as engine-recorded facts ([beyond the chat](#beyond-the-chat)); the outcome of her
sleep ask lives in the conversation only.

### Going away together

`travel_together`: she suggests that the two of them go away together. It is warm like the
romantic choices, but it does not read the pair's scopes, so a pair they never reach can be asked.

**When she asks.** The check runs last, after the romantic ones and the bath's, after every
judged 1:1 text turn, over the same committed, time-decayed state and the same clear runway and
right session, and never while a cold war is on. It needs bond tier 3 or more and the 3 newest
judged turns warm, and no background may be under way. An accepted or ignored `travel_together`
holds the next ask back for 14 days from its answer; after a decline or a lapse she asks again
once the session has a fresh streak. When an earlier kind qualifies on the same turn it goes
first, and its pending ask closes the runway.

**Her ask.** It is delivered as every ask is: her words first, then the choice row beside them,
no tier gap, the same 24-hour window and the same `life_choice` field on the affinity delta.

**The answer.** The same endpoint, the same `409` rules and the same response shape.

| path | choice status | cost | what happens | her words |
|---|---|---|---|---|
| accept | `accepted` | none | a `traveling_together` event starts now, returned as `event` | a generated reply that knows how long the trip is |
| decline | `declined` | none | nothing | a generated, disappointed reply |
| ignore | `ignored` | none | nothing | none of its own: the turn runs as any other |
| lapse | stays `pending`, reads as expired | none | nothing | none |

**The trip.** `traveling_together` (和对方一起旅行) is a non-drawable background in the `trip`
category, lasting 48 to 72 hours, drawn uniformly in whole hours when the user accepts. Starting
it ends an incident under way. She does not tell it as an event, since her reply to the yes
already does, and she keeps replying throughout: it is not offline. `正在经历` and the judge read
it like any background. While it lasts no second background and no kind that can take her offline
is drawn, `on_the_road`, `admiring_scenery` and `taking_selfie` follow it as they follow her
own trips, and [the vehicles](#on-the-move) come up with the cue that she is travelling with the
user. It is instance-scoped like every event.

**Other asks during the trip.** An accepted trip anchors the streak at the answer, so another
ask can come while they are away.

### A bath together

`bathe_together`: she asks the user to take a bath with her. Like the romantic choices, she asks
it only of a romantically compatible pair, a reply typed in words settles it at no cost, and only
a decline costs anything.

**When she asks.** The check runs after the romantic ones and before the trip's, after every
judged 1:1 text turn, over the same committed, time-decayed state and the same clear runway and
right session, and never while a cold war is on. It needs all of:

- a romantically compatible pair, read as for the romantic choices;
- chemistry tier 2 or more for a persona whose `hygienic` is `true`, tier 3 or more when the key
  is absent; a persona whose `hygienic` is `false` never asks;
- at least 3 of the newest 7 judged turns with `warmth` 0.5 or more, read from the turn's
  `state_after`. Patience is not read, and 0.5 is a rule threshold the engine owns, not a judge
  band. As for every choice, only judged turns after the session's streak anchor count.

An accepted or ignored `bathe_together` holds the next ask back for 7 days from its answer;
after a decline or a lapse she asks again on the next turn that qualifies.

It is a trigger of its own, not a step on the way to the bed: some people go to bed together
easily but not into a bath, and a couple can wash apart and still sleep together. Neither ask
waits for the other to settle, and a trip under way does not hold the bath back. She has one ask
pending at a time, so when a romantic kind qualifies on the same turn it goes first and its
pending ask closes the runway.

**Her ask.** It is delivered as every ask is: her words first, then the choice row beside
them, no tier gap, the same 24-hour window and the same `life_choice` field on the affinity
delta. Her ask is generated on the cue
`（你们最近越来越亲密，你很想和对方一起洗个澡。你决定主动开口，问对方愿不愿意和你一起洗，希望对方给你一个答复。）`;
her replies at resolution use the resolution request's own scopes.

**The answer.** The same endpoint, the same `409` rules and the same response shape.

| path | choice status | cost | what happens | her words |
|---|---|---|---|---|
| accept | `accepted` | none | a `showering_together` or `bathing_together` event starts now, one or the other at even odds, returned as `event` | a generated reply on `（对方答应了和你一起洗澡，你们现在一起站在花洒下。）` for a shower, `（对方答应了和你一起洗澡，你们现在一起泡在浴缸里。）` for a bath |
| decline | `declined` | chemistry drops one tier, as for `sleep_together` | nothing | a generated reply on `（对方没有答应和你一起洗澡，你有点失落。）` |
| ignore | `ignored` | none | nothing | none of its own: the turn runs as any other |
| lapse | stays `pending`, reads as expired | none | nothing | none |

**The bath.** The accept turns out a shower or a bath at even odds: `showering_together`
(和对方一起淋浴) or `bathing_together` (和对方一起洗澡), the two kinds the user's
`shower_together` and `bath_together` place. Either is a non-drawable incident in the `short`
category, lasting 30 minutes from the accept. Starting it ends an incident under way; a
background under way goes on. She does not tell it as an event, since her reply to the yes
already does, and she is with the user: it is not offline. While it lasts it is the incident
under way, so no other incident is drawn, and `正在经历` and the judge read it like any incident.
It is instance-scoped like every event. No picture comes with the accept.

The bath stays out of her world story: the story director's events leave out every
`showering_together` and `bathing_together`, so, as with her sleep ask, its outcome lives in the
conversation only.

### Where the choice is stored

`engine.life_choices` holds one row per choice: `session_id` (non-null, cascades with the
session), `kind` (`confess`, `partner_up`, `propose`, `break_up`, `sleep_together`,
`fall_asleep_together`, `travel_together` or `bathe_together`, and `ask_for_space` on stored
rows), `status` (`pending`, `accepted`, `declined`, `ignored`), `tier_gap` (set only on stored
`ask_for_space` rows), `asked_at`, `resolved_at`, `message_id` (the ask; set to NULL if the
message goes) and `event_id` (the `traveling_together`, `showering_together`,
`bathing_together`, `having_sex_together` or `asleep_together` row an accept wrote, NULL on every
other path, an accept of a status kind included; set to NULL if the event goes). Expiry is
derived, never stored. The status and the cold war are not stored here: they sit on the session's
affinity row ([affinity-model.md](affinity-model.md#relationship-columns)).

**Stored data of the ask for space.** Her ask for space (`ask_for_space`) is retired: the engine
writes no such choice and no `taking_space` or `fell_out` event, but rows from earlier stay
readable. A stored `ask_for_space` shows in the history receipts and in the story director's
choices with its outcome; a pending one lapses on its own or is converted by a typed reply at no
cost, and answering it at the endpoint returns `409`. A `taking_space` (说好暂时分开) still under
way keeps her offline like any offline kind until it ends, and her return is then marked in its
choice's session with the two wordings in
[model-config.md](model-config.md#taskspde_decision--opt-in-llm-pde-judge); a `fell_out`
(为分开的事闹得不愉快) reads as any ended incident. Neither kind can be drawn or named in `kinds`.

## Without the LLM judge

| | Without `[tasks.pde_decision].filter_prompt` |
|---|---|
| Needs | none: no rating, no lines, `needs: null`, no draw factors |
| The draw | works, with no needs factors |
| An event's message | generated and told as usual |
| `正在经历` and `刚才` | reach replies, `/open` greetings and event messages |
| `[近期事件]` | not rendered: there is no judge to read it |
| Offline kinds | silence turns as with the judge, since the check runs before it: the ghosting switch and the action and forced-image exemptions apply |
| The life choices and the cold war | unchanged: the triggers read the affinity evaluator's judged turns and patience levels, not the PDE judge. The user's `propose` and `break_up` keep their status gate, and past it the rule engine accepts |
| `smoking`, `alcoholic`, `drug`, `coffee`, `gluttony` | a `false` key still rolls on a give, or on the together action that counts as one, and can turn it into a refusal; `smoking`, `alcoholic` and `drug` still gate the habit kinds; there is no `indulgence` to drift, name or weigh them |

## Code pointers

- `crates/eros-engine-core/src/needs.rs`: the levels, the drift constants, `Habits`, `carried`,
  `after_give` (the give's move).
- `crates/eros-engine-core/src/pde.rs`: `refuses_give` and `UNUSED_SUBSTANCE_ACCEPT`, the give
  roll.
- `crates/eros-engine-core/src/events.rs`: categories and kinds, per-kind rules, gates, the
  needs and persona factors, `is_offline` and `is_told`, `draw`, `lookback`.
- `crates/eros-engine-server/src/routes/session_events.rs`: the events route, the draw's
  inputs, the offline gates.
- `crates/eros-engine-server/src/routes/life_choices.rs`: the resolution endpoint.
- `crates/eros-engine-server/src/routes/bff/affinity.rs`: the `life_choice` field on the delta.
- `crates/eros-engine-server/src/pipeline/proactive.rs`: the event message and the choice's
  messages, their cues and their write (`kind_cue`, `find_cue`).
- `crates/eros-engine-server/src/pipeline/life_choice.rs`: the cold war, the triggers and her
  ask, the ignore conversion, the growth scale of a cold war.
- `crates/eros-engine-core/src/relationship.rs`: the statuses, `settle`, `leading_line`, the
  cold-war streak rules and `sleep_ask_is_sex`.
- `crates/eros-engine-core/src/scope.rs`: `romance_compatible`, the criterion the romantic
  choices read.
- `crates/eros-engine-core/src/tone.rs`: `at_work` and `focused_probability`, the chance of a
  `focused` turn.
- `crates/eros-engine-server/src/pipeline/stream.rs::run_stream`: the needs read and write (the
  accepted give's move goes through `after_give`), the `[需求]` and `[近期事件]` lines
  (`build_pde_ctx`), the offline branch ahead of the judge, the give roll's refusal
  (`apply_give_refusal`), `body_state_frame`.
- `crates/eros-engine-server/src/prompt.rs`: `needs_line`, `events_judge_text`,
  `events_reply_text`, `just_now_text`, `persona_traits`, `persona_habits`, the `[character_state]` lines and the
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
  `resolve_in_tx`, `settled`, `settled_since`, `answered_since`, `streak_anchor`.
- `crates/eros-engine-store/src/affinity.rs::AffinityRepo`: `persist_life_choice_drop_in_tx`,
  `last_judged_endpoints`, `latest_status_change`, `write_relationship_in_tx`, `set_cold_war`.
- `crates/eros-engine-store/src/chat.rs::ChatRepo`: `set_needs` and `latest_needs`; `latest_scopes`,
  the scopes the romantic choices judge a pair by.
- `crates/eros-engine-store/migrations/0000_baseline.sql`: the events and choices
  tables, the needs index, and the widened affinity `event_type` check (folded
  from the pre-baseline chain).

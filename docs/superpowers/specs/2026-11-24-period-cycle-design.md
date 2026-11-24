# Her period on a 28-day cycle — Design

- **Date:** 2026-11-24
- **Status:** Approved
- **Type:** Engine change. One migration, an index only. One new card key. Additive wire
  change (`cycle_event` on the effects response); one behaviour change on the
  wire (a `period` under way leaves `background_event`); `period_scare`
  changes from a one-hour incident to a state of several days.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-15-life-events-v2-design.md` (the draw, the
  background), `2026-10-16-life-events-offline-and-scares-design.md` §5 (the
  false alarms), `2026-10-25-life-events-follow-ons-and-persona-keys-design.md`
  (`health`, the follow-ons), `2026-11-19-vehicles-actions-species-design.md`
  §5 (`species`, machines), `2026-10-28-outing-indulgence-needs-and-habits-design.md`
  §3.1 (a refusal rolled before the judge),
  `2026-11-23-twelve-together-actions-and-propose-design.md` (the together
  actions), `2026-10-30` and `2026-11-23-affinity-50-design.md` §7 (her asks).

## 1. Intent

Today her period is one more random draw: it may never come, or come at any
interval, and it lasts three to seven days. This design gives her a cycle.
The first `period` an instance draws sets it; from then on her period comes
every 28 days on its own, for as long as her constitution says, without
anything being drawn or written. While it lasts she mostly turns down sex and
anything in water, and does not ask for either herself. A late-period scare
becomes part of the cycle: it can only happen in the days before her period
is due, sex in those days makes it likelier, and it delays that period until
the scare ends.

## 2. Goals, non-goals, anti-goals

**Goals.**

- A card switch, `period`, saying whether she has a period at all.
- A per-instance 28-day cycle that starts at the instance's own first drawn
  `period` and needs no stored date.
- A period length read from `health`: strong 1 day, normal 3, weak 5.
- Her period in every place that today shows a `period` background, also
  while a trip or an illness is under way.
- During her period: a roll before the judge that refuses `sex_together` and
  the water actions two times in three; no `sleep_together` or
  `bathe_together` ask from her.
- `period_scare` as a late period: drawn only in the three days before the
  due moment, three times as likely after sex in those days, lasting past the
  due moment and delaying that one period.

**Non-goals.**

- A date on the card. *(Owner's ruling: the card carries the switch only;
  every instance's cycle is set by its draw.)*
- Pregnancy. `pregnancy_scare` stays exactly as it is. *(Owner's ruling: the
  same reason there is an engagement and no marriage. The product renders
  what being together feels like and settles nothing.)*
- Any effect of sex on the cycle's dates. Sex only raises the odds of a
  scare, and a scare delays one period, not the grid.
- A cycle length other than 28 days, or one the card can set.
- Any other effect of her period on events, needs, tone or affinity beyond
  what a `period` background already does today (§7).
- Announcing a period that arrives on the cycle. Nothing calls the engine at
  that moment; she is simply in it.

**Anti-goals.**

- Writing the cycle anywhere. One card serves many users' instances and the
  card belongs to the downstream; the cycle is derived from event rows the
  instance already has, as her virginity is.
- Costing the relationship for a refusal her period caused: it is her body,
  not the user being turned down.
- A special case that keeps her illness rate where it is today (§5).

## 3. Approaches

Three ways to make the period arrive every 28 days after the first draw:

- **A. Derive it when read (picked).** The cycle is computed from the
  instance's newest `period` row, its newest `period_scare` row, and
  `health`. Nothing is written. Exact however long nobody talked to her, and
  it follows a changed `health` at once. Cost: every reader of "what is under
  way" must go through one derivation, and `engine.persona_events` only ever
  holds the first period.
- **B. Write a row on first read inside the window.** Downstream readers stay
  as they are. Cost: writes on read paths with an idempotency key, holes in
  the history for windows nobody read, rows that go stale when `health`
  changes.
- **C. A background task writes the rows.** Cost: a new sweeper, rows written
  forever for users who left, and nothing on a deployment that runs its
  sweepers off.

The owner picked A: it keeps one source for the fact and adds no storage.

Two further choices the owner made, each from three sketched options:

- *Where a card with no date would get one.* Derive it per instance and never
  write the card (picked); a new column on the instance; write it back to the
  card. The owner then removed the card date altogether.
- *What a late-period scare becomes.* A state of several days that spans the
  due moment and ends where the period begins (picked); a one-hour incident
  with the period arriving a few days late on its own; odds only, no delay.

## 4. Who has a period

`art_metadata.period` is a boolean. A value that is not a boolean reads as
absent, as for every other key.

| persona | `period` absent | `period: true` | `period: false` |
|---|---|---|---|
| female, human (`species` absent, blank or `human`) | has one | has one | none |
| female, any other species that is not a machine | none | has one | none |
| female, `robot` or `android` | none | none | none |
| not `gender: "female"` | none | none | none |

*(Owner's rulings.)* `PersonaTraits` gains one field holding the result of
this table; the gates on `period` and `period_scare` read it in place of
`female && !machine`. `pregnancy_scare` keeps `female && !machine`.

This narrows today's behaviour: a female elf, beastkin and so on draws
`period` and `period_scare` today and stops unless her card sets
`period: true`.

A persona with no period has no cycle whatever rows her instance holds: an
old `period` row still under way stops showing at deploy.

No card version change: the key joins the 1.1 document, as `species` did.

## 5. The cycle

Constants: the cycle is 28 days; the period lasts 1, 3 or 5 days for
`health` strong, normal (also absent), weak; the scare window is the 3 days
before a due moment; a scare ends 1 to 3 whole days after the due moment.

All arithmetic is on instants. No time zone is involved: a period starts at
the time of day the anchor did.

**The anchor** is the instance's newest `period` row, not bounded by the
draw's lookback. Its `started_at` is the cycle's origin. No such row, no
cycle.

**Due moments.** `due(n) = anchor.started_at + n × 28 days`, for `n ≥ 1`.

**A scare belongs to cycle `n`** when its row started in
`[due(n) − 3 days, due(n))`. At most one does (§6).

**Period `n`** starts at `start(n)`: `max(due(n), scare.ends_at)` when a
scare belongs to cycle `n`, otherwise `due(n)`. It lasts the days `health`
gives, read when the derivation runs. `due(n + 1)` does not move. *(Owner's
ruling: only the one period is late.)*

**Period 0** is the anchor row itself, with the span the row stores.

**The cycle state at a moment `t`** is the first of:

1. the anchor row under way at `t`: *period*, the row's span;
2. a `period_scare` row under way at `t`: *late*, the row's span;
3. with `n = floor((t − anchor.started_at) / 28 days) ≥ 1`, `t` inside
   period `n`: *period*, that span;
4. nothing.

The two rows the derivation needs are the newest `period` row and the newest
`period_scare` row that started at or before `t`. No older scare can matter:
a scare outlives its due moment, the next window opens 25 days after that
moment, and the longest period ends 8 days after it.

The derivation is a pure function in `eros-engine-core`.

**Drawing `period`.** Only an instance with no anchor draws it, and so at
most once. It keeps its category, weight and gates otherwise, and lasts the
days `health` gives in place of today's 3 to 7. She tells it as today
(「你的月经来了。」). A caller that keeps `period` out of the draw's `kinds`
keeps the instance without a cycle.

**Her illness rate rises once she has an anchor.** `period` shares the
`body` category with `sick`, `hospitalized` and `post_surgery`. With `period`
out of the draw, every `body` pick is one of those three, and no drawn period
cools the category. She then falls ill as often as a persona with no period
does today, roughly 1.4 to 2 times her rate today. *(Owner's ruling:
accepted, with no compensation.)*

## 6. `period_scare`

Today: a one-hour foreground incident drawn at any time, 28-day cooldown,
gated on rung 2. From this design on:

- **When.** Eligible only when the persona has a period, the instance has an
  anchor, the rung gate holds, and now is inside
  `[due(n) − 3 days, due(n))` for the next due moment, with no cycle state
  under way. Its own cooldown drops to 0: the window and its own length allow
  one per cycle. An instance with no anchor never draws it.
- **How likely.** Its weight in `short` stays 0.5. Any event of a kind in
  `EventKind::SEX` that started inside the window multiplies it by
  `FOLLOW_FACTOR` (3), once. `sick` leading to it stays. *(Owner's ruling:
  sex before her period raises the odds. Engine's call: the existing
  follow-on multiplier.)*
- **How long.** It ends `D` whole days after the due moment, `D` uniform in
  1 to 3 from the draw's duration roll. Its stored span is the whole of it.
- **What it is while it lasts.** A cycle state, *late*. It is not a
  foreground incident: it does not stop the draw, it does not occupy the
  foreground, and other incidents happen during it. It keeps its `short`
  category for the draw's first stage and on the wire. *(Engine's call.)*
- **What it does.** The period of that cycle starts when it ends (§5). It
  changes nothing about user actions or her asks: she is not on her period.
- **How it is told.** As today: one message when it starts, at rung 2; while
  under way its label alone; after it ends, the aftermath
  「以为月经没来，后来来了，是虚惊一场」 until she has spoken, which is now
  also the moment her period begins.
- **Its opening cue** changes, because it now starts before the due moment.
  Proposed: 「（按日子月经这两天就该来了，身体却一点动静都没有，你心里有点慌。）」
  *(Engine's wording; the owner reads the copy before it ships.)* The label
  「月经没来」 stays.

`pregnancy_scare` is untouched: one hour, foreground, drawn at any time.

## 7. The cycle slot

A trip or an illness and her period now overlap, and one background slot
cannot hold both. The cycle state gets a slot of its own.

- `CurrentEvents` gains a third slot beside `foreground` and `background`. A
  `period` or `period_scare` row never lands in the other two. The slot holds
  the kind and the span; a derived period has no row id, session or message.
- **One read.** Every reader of what is under way gets the cycle state from
  the same function, so none can leave it out. The two rows it needs come in
  the statement `EventRepo::current` already runs: no extra round trip on a
  turn. A reader that has no persona in hand loads one when the instance has
  an anchor.
- **Readers.** The chat turn (reply prompt, judge, picture composer), the
  proactive paths (an event's message, `/open`, her asks), the effects route
  and the draw.
- **Not the world system.** The world director's roster read
  (`pipeline/world.rs`) and the story round (`pipeline/story.rs`) read event
  rows only and never the cycle state, so they hear of the anchor row and of
  no later period. A `period` row is kept out of the background they read
  (`backgrounds_at`, the story round's `current_background`), so it cannot
  take the place of a trip; it stays in the story round's `life_events` rows.
  The director's rules and prompts do not change. *(Owner's ruling.)*
- **Rendering.** Wherever a background is rendered today, the cycle state is
  rendered right after it and before the foreground, with the labels
  「月经中」 and 「月经没来」 and the spans in the existing form. *Late* under
  way is its label alone, as a false alarm is today. A foreground event's cue
  during her period opens 「你正在经期」 as today; when a trip or an illness is
  also under way, the background's opening comes first and the period's
  second.

**What a period under way does to the draw** is what a `period` background
does today, for a derived period as for the row:

- no `trip` or `body` kind is drawn;
- the two false alarms are ruled out;
- `irritable` and `unwell` are follow-ons while it lasts and for 48 hours
  after.

*Late* under way rules nothing out and leads to nothing.

Cooldowns read rows only. A derived period does not cool the `body`
category; the anchor row cools it once, as today.

## 8. User actions

During her period (the cycle state at the turn is *period*) these actions
are rolled before the judge: `sex_together`, `bath_together`,
`hot_spring_together`, `sauna_together`, `swim_together`. *(Owner's rulings:
the roll, the one-in-three, the list. `shower_together` and every other
action are not on it.)*

- A uniform roll at or above 1/3 refuses the action before the judge. The
  judge is told 「这次你不会答应」 as for every refusal ruled before it; her
  period is already in its `[近期事件]` and in the reply prompt's state.
- That refusal drops nothing.
- Below 1/3 the action goes to the judge as on any other day; a refusal the
  judge then decides drops chemistry as usual.
- The existing rule-outs come first and keep their own outcomes: a pair that
  is not romance-compatible, a world that lacks the action.
- An accepted action places its event as usual, during her period included.

The roll cannot be recomputed after the turn, so its outcome has to reach
the place that decides the drop (`tier_drop_for`), which today recomputes its
exemptions from the event alone.

## 9. Her asks

While the cycle state is *period*:

- `try_romance` does not ask `sleep_together`. It is not replaced by
  `fall_asleep_together`; `propose` and `fall_asleep_together` are asked as
  on any other day. `sleep_together` is asked once per session, so her period
  only delays it.
- `try_bathe` does not ask `bathe_together`. *(Engine's call, following the
  owner's choice of the water actions in §8; the owner's own list named
  `sleep_together` only.)*

Her period holds back no other ask. `try_travel` does not ask while a
background is under way, and today a `period` background counts; from this
design on her period is not a background, so she can suggest a trip during
it. *(Owner's ruling: her period touches events, the actions of §8 and her
`sleep_together` ask, nothing else.)*

An ask already pending when her period starts stands, and accepting it places
its event.

## 10. The wire

- **Effects response.** New `cycle_event`: an `EventView` or `null`, beside
  `event` and `background_event`. `kind` is `period` or `period_scare`;
  `offline` is false; `message_id` is the row's, `null` for a derived period.
  `changes_at` also counts the end of `cycle_event` and, when nothing is
  under way, the start of her next period.
- **Behaviour change.** A `period` under way is no longer in
  `background_event`.
- **Draw response.** A drawn `period_scare` comes back with an `ends_at`
  days away. Nothing else changes shape.
- `period` and `period_scare` remain kinds a caller can list in `kinds`.

## 11. Existing data and deploy

- One migration, an index only: a partial index on
  `engine.persona_events (instance_id, kind, started_at DESC)` for the two
  cycle kinds. The anchor is read on every turn and can be any age; without
  the index that read walks the instance's whole event history. No column,
  no stored fact. Migrate before the new image starts.
- An instance that already holds `period` rows goes on the grid of its newest
  one at deploy.
- Old one-hour `period_scare` rows are long over and fall outside every
  window or end before their due moment, so they delay nothing.
- **Rollback.** An image from before this design reads a multi-day
  `period_scare` row as a foreground incident, which stops that instance's
  draws until the row ends. The migrating note says so.

## 12. Documents and the downstream

In a documents PR of its own, after the code PR: `docs/life-events-needs.md`
and its `.zh`, `docs/eros-character-v1.1.md` (the `period` key),
`docs/api-reference.md` and its `.zh`, and a migrating note for the release.

The downstream gets one issue: the card switch (shown for a female persona
that is not a machine, on by default for a human), `cycle_event`, and
`period` leaving `background_event`.

## 13. Left to the builder

- The shape of the cycle slot, of the derivation's inputs and outputs, and of
  what the draw is handed.
- Where the single read lives and how `EventRepo::current` returns the two
  rows.
- How the outcome of §8's roll travels to `tier_drop_for`.
- How the draw computes a scare's length, which the fixed `Lasts` table
  cannot express.
- Names.

## 14. Tests

- Core: the derivation over a table of moments (before the first due moment,
  on it, inside and after each length, a scare in the window, a scare outside
  it, an old one-hour scare, no anchor, no period trait); §4's table; the
  draw (`period` only without an anchor, the scare's window, the sex factor,
  the scare's length, what a derived period rules out, the `body` cooldown
  untouched by a derived period, a scare under way not stopping the draw).
- Server: a turn during a derived period shows it beside a trip; §8's roll
  both ways with the drop it leaves; §9's two asks held back and the travel
  ask not; the effects response; a `period` row out of the world system's
  background.
- Timestamps in assertions are fixed and microsecond-aligned.

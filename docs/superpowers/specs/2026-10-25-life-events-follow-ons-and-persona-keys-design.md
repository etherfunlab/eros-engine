# Life events: follow-on factors, `is_virgin` and `homesick` — Design

- **Date:** 2026-10-25
- **Status:** Approved
- **Type:** Engine change. No migration, no API change. Two new keys read
  from `art_metadata`; one new input to `draw` (`DrawInput` gains a field,
  `PersonaTraits` gains two: breaks struct literals); one new store read.
  Ships in v2.3.1 with `2026-10-25-effects-as-of-and-three-kinds-design.md`.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-15-life-events-v2-design.md` (*the v2 spec*: the
  two-stage draw, the factors, `art_metadata` keys) and
  `2026-10-25-effects-as-of-and-three-kinds-design.md` (its seven kinds). Everything there stands.

## 1. Intent

Her day should hang together. An ex's message makes a night with someone
likelier; a week in bed makes a late period a worry; a business trip puts
her on the road. Today every draw is memoryless beyond cooldowns and the
hospital rule-outs. This spec adds one table-driven mechanism and no
state machine: **follow-on factors**. While event A is under way, or for
a while after it ended, kind B weighs more; and a kind that in life follows
another weighs less while that other has never happened.

A state machine was considered and rejected: the existing rules (one
incident and one background, cooldowns, rule-outs) plus this table express
"likelier after" and "unlikely without"; a transition graph would make
every pair not in the graph unreachable, which removes capability rather
than raising a threshold. For the same reason there is no hard "B only
after A" gate: a deployment where A cannot be drawn (an offline kind with
the ghosting switch off) must not lose B.

Two persona-card keys ride along: `is_virgin` and `homesick`.

## 2. Follow-on factors

### 2.1 Mechanism

A fixed table of `(prior, kind)` pairs. In the second stage of the draw, a
kind's weight is multiplied by `FOLLOW_FACTOR = 3` once for every distinct
`prior` in the table that is **live**: it is in the instance's history and
either under way at `now` or ended within `FOLLOW_WINDOW = 48` hours before
`now`. Two live priors for the same kind multiply (×9). The factor joins
the needs and preference factors; it never lifts a gate, never touches the
first stage, and ignores the caller's `kinds` only in the sense that a
disallowed kind is simply not there to weigh.

The history a draw already reads (`lookback()`, 90 days) covers every
prior.

### 2.2 The table

| prior (live) | kind weighed ×3 | why |
|---|---|---|
| `ex_message` | `having_sex` | the user's pair |
| `sudden_desire` | `having_sex` | |
| `sick` | `period_scare` | the user's pair |
| `having_sex` | `pregnancy_scare` | with the gate in §3 |
| `business_trip` | `on_the_road`, `on_the_way`, `busy`, `temp_work` | the user's pair; a trip is work |
| `short_trip`, `long_trip`, `hometown_visit` | `on_the_road`, `admiring_scenery`, `taking_selfie` | travel days |
| `hometown_visit` | `bros_gathering`, `sis_gathering` | old friends at home |
| `hospitalized`, `post_surgery` | `reading`, `gaming` | bedridden |
| `period` | `irritable`, `unwell` | |
| `breakup`, `quarrel`, `scolded_by_superior`, `in_battle` | `irritable` | |
| `got_hit`, `got_bumped`, `weather_change` | `unwell` | |
| `working_out` | `showering`, `bathing` | |

Gates still apply: `sick` makes `period_scare` likelier only for a female
persona at rung 2 or above; `having_sex` makes `pregnancy_scare` likelier
only at rung 3. Cooldowns still apply: a live `business_trip` cannot make
`on_the_way` happen twice in 12 hours.

## 3. Without a prior

The one kind that in life follows another is weighed down, not ruled out,
while the other has never happened:

| kind | while the instance has no `having_sex` row, ever | weighs |
|---|---|---|
| `pregnancy_scare` | | `UNLIKELY_FACTOR = 0.2` |

"Ever" is the same read as §4 (`DrawInput.ever_had_sex`). With a row it
weighs ×1, and ×3 on top while that row is live (§2.2). A deployment that
never draws `having_sex` keeps the scare at a fifth of its weight rather
than losing it, and nothing goes dark on the day the kind ships.

## 4. `is_virgin`

A new `art_metadata` key beside `gender`, `health`, `prefer_trip`,
`prefer_events`: `is_virgin`, boolean, lenient (absent or not a boolean
reads as `false`). The engine never writes it.

Her virginity as the draw sees it is derived: the card says `is_virgin`
**and** the instance has no `having_sex` row, ever. While she is a virgin,
`having_sex` weighs `UNLIKELY_FACTOR = 0.2` in the second stage: the first
time is rare and a big thing, not impossible. After the first `having_sex`
row she is not a virgin, with no write anywhere: the next draw reads the
row. "Ever" is not bounded by `lookback()`: it is one `EXISTS` over
`persona_events` for the instance and the kind, run by the events route
before the draw and passed in as `DrawInput.ever_had_sex`. The card key
itself lands on `PersonaTraits.is_virgin`.

`pregnancy_scare` needs no virgin rule: §3 already weighs it down for an
instance with no `having_sex` row, which is every virgin.

The key reaches no prompt: a persona's virginity is written into her
persona by its author, as every other persona fact.

## 5. `homesick`

`prefer_trip` says whether she moves at all: it scales the whole `trip`
category in the first stage. `homesick` says where she goes when she does:
home, or anywhere else. A new `art_metadata` key, boolean, lenient (absent
or not a boolean reads as no preference), on
`PersonaTraits.homesick: Option<bool>`. In the second stage, within `trip`:

| `homesick` | `hometown_visit` | `short_trip`, `long_trip` | `business_trip` |
|---|---|---|---|
| `true` (her home is her base) | ×3 | ×⅓ | ×1 |
| `false` (she roams) | ×⅓ | ×1 | ×1 |
| absent | ×1 | ×1 | ×1 |

`business_trip` is work and neither key moves it within the category. A
flight attendant with `prefer_trip` and `homesick` both `true` travels
often, works her trips as before, and spends her leave at home rather than
elsewhere.

## 6. Out of scope

- A state machine or any hard "B only after A" gate.
- Writing `is_virgin` or `homesick` from the engine; a prompt line for
  either.
- Follow-on factors in the first stage (a prior making a whole category
  likelier). `health` and `prefer_trip` remain the only category factors.
- Any change to the factors' magnitudes being configurable; `3`, `48`
  and `0.2` are engine constants like `PREFERENCE_FACTOR`.

## 7. Testing

- Core: for each table row, with the prior live, the kind's weight is ×3
  and other kinds' weights unchanged (assert through the draw with pinned
  rolls, as the existing factor tests do); a prior ended 49 hours ago is
  not live; a prior under way is; two live priors multiply; a live prior
  never lifts a gate (`sick` + male persona ⇒ no `period_scare`).
- Core: `pregnancy_scare` ×0.2 with `ever_had_sex = false`, ×1 with
  `true`, still eligible either way (female, rung 3).
- Core: `is_virgin` + `ever_had_sex = false` ⇒ `having_sex` ×0.2;
  `is_virgin` + `ever_had_sex = true` ⇒ ×1; card absent ⇒ ×1.
- Core: `homesick` three states on `hometown_visit` and the two leisure
  trips; `business_trip` unchanged in all three.
- Server: `persona_traits` reads both keys leniently (wrong type ⇒
  default); the route passes `ever_had_sex` from the new read.
- Store: `EventRepo::ever(instance, kind)` true with a row older than
  `lookback()`, false with none, false for another instance.

## 8. Documentation

`docs/life-events-needs{,.zh}.md`: a "Follow-on factors" subsection with
the table, the window and the no-prior row, the two keys in the
persona-settings table, `is_virgin`'s derivation. `docs/api-
reference{,.zh}.md`: the keys where the others are listed. The v2.3.1
release notes.

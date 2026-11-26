# Together-action groups: general, naturism and thrill; water; naturism counterparts — Design

- **Date:** 2026-11-27
- **Status:** Approved
- **Type:** Engine change. No migration. Additive wire change: three new
  action types and three new event kinds (placed, never drawn). Behaviour
  changes: no together action is refused before the judge for romance
  compatibility any more; several pictures, period rolls, held-back asks
  and refusal lines change (§9).
- **Owner:** enriquephl (sole dev)
- **Builds on:**
  `2026-11-27-swim-outfit-ladder-and-fish-together-design.md` (the swim
  ladder, `ride_horse_naked_together`, the outdoor bond refusals),
  `2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-custom-together-design.md`
  (§5 hot spring and sauna clothing, §6 the chemistry-drop end, §7 the held
  asks), `2026-11-24-period-cycle-design.md` §8 (the period roll),
  `2026-11-23-twelve-together-actions-and-propose-design.md` (the romance
  gate on the together actions).

## 1. Intent

Every together action belongs to one of three groups, and what the engine
does with it follows from its group and from whether it happens in water:

- **general** — ordinary things done together;
- **naturism** (天然) — the same done naked, or naked by nature;
- **thrill** — adventure, crime, playing with fire, fighting.

Documentation calls the second group naturism (天然); the code names it
`Nude` (`TogetherGroup::Nude`), and the action and kind identifiers keep
`naked`. *(Owner's ruling.)*

A general and a naturism action that are the same activity are paired (a jog
and a streak). Three naturism actions are added: naked yoga, a naked hike and
a naked hot spring. The groups replace hand-kept lists, so a new action
only has to say its group and whether it is wet.

## 2. Goals, non-goals, anti-goals

**Goals.** The group, water and counterpart properties of §4; everything
of §5–§8 derived from them; the three actions of §6.

**Non-goals.** Naturism counterparts beyond the six pairs of §4.3 ("anything I
did not list, do not add"); any change to her own solo `bathing` /
`showering`, to `strip`'s compatibility refusal, or to the compatibility
her own romantic asks need; any change to the swim ladder's table.

**Anti-goals.** A hand-kept list for any fact the group, water or
counterpart property already carries.

## 3. Approaches

**Where the groups live.** Three ways: (a) properties on `EventKind`
(group, wet, clothed counterpart), with the action side reading them
through `UserAction::together_kind`, and the existing lists derived from
them; (b) keep the hand-kept lists and only change their members; (c)
properties on `UserAction` only. (a) is taken. (b) leaves no groups, which
is what was asked for, and the lists drift. (c) misses the rows her own
accepted asks place (`bathing_together`, `having_sex_together`), which the
event-side rules (the chemistry-drop end, the held asks, the story
director, the picture) must read.

## 4. The properties

### 4.1 Group

| group | actions → kinds |
|---|---|
| general | `walk_together`, `jog_together`, `yoga_together`, `stargaze_together`, `ride_horse_together`, `drink_together`, `smoke_together`, `smoke_weed_together`, `toilet_together`, `fall_asleep_together`, `workout_together`, `swim_together`, `cycle_together`, `hike_together`, `fish_together`, `dance_together`, `sing_together`, `hot_spring_together`, `sauna_together`, `custom_together` |
| naturism | `shower_together`, `bath_together`, `sex_together`, `streak_together`, `skinny_dip_together`, `ride_horse_naked_together`, and §6's `yoga_naked_together`, `hike_naked_together`, `hot_spring_naked_together` |
| thrill | `adventure_together`, `commit_crime_together`, `play_with_fire_together`, `fight_together` |

Each action's group is its kind's group. *(Owner's rulings: the three
groups; naturism: sex, shower, bath, skinny dip, streak, the naked ride and the
three new ones; thrill: adventure, crime, fire, fight; general: sleep and
swim. Engine's call: every other action in general, `custom_together`
included.)*

### 4.2 Water

Wet kinds: `swimming_together`, `hot_spring_together`, `sauna_together`
(general); `showering_together`, `bathing_together`,
`skinny_dipping_together`, `naked_hot_spring_together` (naturism). No thrill
kind is wet. *(Owner's rulings: water is a property of the general and the
naturism groups; shower and bath are naturism and wet. Engine's call: sauna wet,
keeping it on the two lists it is on today.)*

### 4.3 Counterparts

Each naturism kind below names its clothed counterpart:

| naturism | clothed |
|---|---|
| `streaking_together` | `jogging_together` |
| `naked_hiking_together` | `hiking_together` |
| `skinny_dipping_together` | `swimming_together` |
| `naked_hot_spring_together` | `hot_spring_together` |
| `naked_yoga_together` | `yoga_together` |
| `naked_horse_riding_together` | `horse_riding_together` |

`showering_together`, `bathing_together` and `having_sex_together` have
none. *(Owner's rulings: jog/streak, hike/naked hike, swim/skinny dip, hot
spring/naked hot spring, yoga/naked yoga. Engine's call: horse riding/the
naked ride, the pair the owner's naked ride implies.)*

## 5. What the properties decide

### 5.1 Naturism

A naturism action or kind:

- **no romance gate.** Naturism is not a romantic act, so romance
  compatibility does not gate it: `UserAction::romance_gated` goes and
  nothing is refused before the judge for compatibility. `together_ruled_out` keeps
  only its machine rule (`toilet_together`). *(Owner's ruling; it
  overturns the gate on `shower_together`, `bath_together`, `sex_together`,
  `streak_together`, `skinny_dip_together` and
  `ride_horse_naked_together`.)*
- **answered in the `emotional` tone** (`speaks_emotionally`), accepted or
  refused. *(Owner's ruling.)*
- **ended by a chemistry drop**: `EventKind::ENDS_ON_CHEMISTRY_DROP` is the
  naturism kinds. *(Owner's ruling.)*
- **kept out of the story director**: every naturism kind is in `life_events`'
  kept-out list, beside the non-naturism kinds already there for their own
  reasons (`using_toilet_together`, `asleep_together`, `custom_together`,
  `on_a_date`). *(Owner's ruling.)*
- **pictured with nothing on**: §7.

### 5.2 Water

- **The period roll** (`pde::period_refuses`) rolls an action whose kind is
  wet, and `sex_together`. *(Owner's ruling.)*
- **The held asks** (`EventKind::HOLDS_BACK_INTIMATE_ASKS`: she does not
  ask the user to bathe or to bed while it is under way) are the wet kinds
  and `having_sex_together`. *(Owner's ruling.)*

### 5.3 Refusal lines

`refusal_line` becomes `refusal_lines`, the set of lines a refusal drops,
each by the one-tier drop of today:

| action | lines |
|---|---|
| `give`, `gift`, every thrill action | bond |
| `jog_together`, `hike_together`, `fish_together`, `cycle_together`, `ride_horse_together` | bond |
| a naturism action whose counterpart (§4.3) drops bond | bond and chemistry |
| every other physical or custom action | chemistry |
| `tip`, `bluff`, `threaten`, `trick`, `propose`, `break_up`, `challenge`, `bet`, `date` | none |

So a refused `streak_together`, `hike_naked_together` or
`ride_horse_naked_together` drops both lines; a refused
`skinny_dip_together`, `yoga_naked_together`, `hot_spring_naked_together`,
`shower_together`, `bath_together` or `sex_together` drops chemistry.
*(Owner's rulings: thrill drops bond; a naturism counterpart of a bond-dropping
outdoor action drops both. The outdoor five stay a named list: "outdoor"
is not a group.)* The rules that make a refusal free (the world rule, the
period roll, `refusal_free`) are unchanged.

The audit's `context.tier_drop` is `bond`, `chemistry` or, for a two-line
drop, `both`. *(Engine's call.)*

## 6. Three naturism actions

| action `type` | marker | places | label | category | lasts | fits |
|---|---|---|---|---|---|---|
| `yoga_naked_together` | 想和你一起裸体做瑜伽 | `naked_yoga_together` | 和对方一起裸体做瑜伽 | `short` | 1 h | both |
| `hike_naked_together` | 想和你一起裸体爬山 | `naked_hiking_together` | 和对方一起裸体爬山 | `mid` | 2–3 h | both |
| `hot_spring_naked_together` | 想和你一起裸体泡温泉 | `naked_hot_spring_together` | 和对方一起裸体泡温泉 | `short` | 1 h | both |

Each is naturism (§5.1), takes its counterpart's length, and otherwise behaves
as the other together actions: no payload; an accept places the kind and
ends the incident under way (no event inside a date); a picture turn;
`SPECIAL`, `Neutral` place, never told, not offline, shared.
*(Owner's rulings: the three actions. Engine's calls: names, markers and
labels — spelled out with 裸体, as `ride_horse_naked_together` is —
lengths and worlds.)*

## 7. Pictures

The engine writes the composer's `[此刻穿着]` from the picture's
foreground kind:

| foreground | `[此刻穿着]` |
|---|---|
| any naturism kind | 什么都没穿 |
| `swimming_together`, `hot_spring_together` | the swim ladder (spec 2026-11-27-swim-outfit-ladder §4) |
| `hot_spring_together`, a gender other than female or male | the sauna's two lines below |
| `sauna_together`, romance-compatible | 身上只松松地裹着一条浴巾 |
| `sauna_together`, not compatible | 身上只裹着一条浴巾 |
| any other | the judge's clothing, as today |

The ladder writes nothing for a gender other than female or male. A swim
keeps that; a hot spring does not, since a hot spring left to the composer
is what came back in street clothes (2026-11-26 §5): there she wears the
sauna's towel, loose for a compatible pair. *(Owner's ruling.)*

*(Owner's rulings: every naturism kind nothing on — which takes
`showering_together`, `bathing_together` and `having_sex_together` off "the
state wins"; the general hot spring wears the swim ladder; the sauna a
towel, a loose one for a compatible pair. Engine's call: the loose-towel
wording.)* Which pictures, and "only the composer's slot", are as before.
The swim ladder's virginity read applies to a hot spring as to a swim.

## 8. What stays

`strip`'s compatibility refusal; the compatibility her own romantic asks
need; her solo `bathing` / `showering`; the swim ladder's rows; the world
rules (`fight_together` fantasy-only, `cycle_together` modern-only); the
outdoor bond refusals of the previous spec.

## 9. What changes for a live user

- A pair that is not romance-compatible can now have a shower, a bath,
  sex, a streak, a skinny dip or a naked ride accepted, if the judge says
  yes.
- `shower_together` is rolled on her period; `hot_spring_naked_together`
  is too.
- She holds her bathe/sleep asks during a swim, a skinny dip and a naked
  hot spring, as well as before.
- Shower, bath and sex pictures show nothing on. A hot spring picture
  shows the swim ladder instead of nothing on / a towel (a towel, loose for
  a compatible pair, when her gender is neither female nor male). A sauna
  picture for a compatible pair shows a loose towel instead of nothing on.
- A refused adventure, fire or fight drops bond, not chemistry. A refused
  streak, naked hike or naked ride drops bond and chemistry.

## 10. Tests

- Core, table-driven over every together action: its group, its kind's
  wet flag and counterpart, `speaks_emotionally`, `refusal_lines`, and
  that no action is romance-gated any more.
- `ENDS_ON_CHEMISTRY_DROP`, `HOLDS_BACK_INTIMATE_ASKS` and
  `period_refuses` equal what §5 derives, pinned as explicit lists so a
  drift shows.
- The three new kinds: placed-kind properties as the earlier specs test
  them.
- The clothing table of §7 for every row, every wearer.
- Server: a refused `streak_together` drops both lines and its audit says
  `both`; an incompatible pair's `sex_together` reaches the judge; a
  shower picture and a hot spring picture carry their §7 lines.
- Story: every naturism kind kept out.

## 11. Order of work

One code PR, on top of the merged swim-ladder change. Then one
documentation PR that covers this spec and the swim-ladder spec together:
neither has been released, and this one overturns parts of that one. *(Engine's call.)*

## 12. Migrating notes to write

The three new actions and kinds; the gate removal and what it lets through
(§9); the picture, period, held-ask and refusal changes of §9;
`context.tier_drop` can be `both`. The notes are written against v2.8.3,
the last release, as one guide with the swim-ladder spec's: a reader never
saw the state in between. No migration; rolling back is redeploying the
previous image.

## 13. Left to the builder

- The names of the group type and of the three property functions, and
  whether the counterpart is stored naturism→general only.
- How `refusal_lines` is typed (a slice, a small struct) and how the
  post-processing drop loops over it.
- Whether `ENDS_ON_CHEMISTRY_DROP` / `HOLDS_BACK_INTIMATE_ASKS` stay
  consts (pinned by a test against the properties) or become functions.

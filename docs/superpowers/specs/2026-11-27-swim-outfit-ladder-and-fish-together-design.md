# What she wears swimming, skinny-dipping clothing, and fishing together — Design

- **Date:** 2026-11-27
- **Status:** Approved
- **Type:** Engine change. No migration. Additive wire change: one new action
  type and one new event kind (placed, never drawn). Two behaviour changes:
  the composer's `[此刻穿着]` is written by the engine while she swims or
  skinny-dips with the user; refusing five outdoor together actions drops
  bond instead of chemistry.
- **Owner:** enriquephl (sole dev)
- **Builds on:**
  `2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-custom-together-design.md`
  (§4, the together actions and `skinny_dip_together`; §5, what she wears in
  a hot spring or a sauna),
  `2026-11-24-focused-tone-four-together-actions-and-user-gender-design.md`
  (§6, `swim_together` and `hike_together`),
  `2026-11-11-stocks-lost-habit-events-and-just-have-sex-design.md` §4 and
  `2026-11-23-twelve-together-actions-and-propose-design.md` §6 (what ends
  her virginity).

## 1. Intent

Four changes:

1. A picture drawn while she swims with the user shows a swimsuit that
   follows how far the romance has gone, chosen by the engine from a fixed
   ladder, instead of whatever the composer picks.
2. A picture drawn while she skinny-dips with the user shows her with
   nothing on.
3. The user can ask her to go fishing together.
4. Turning down a jog, a hike, a fishing trip, a bike ride or a horse ride
   costs bond, not chemistry: she may simply not care for the outdoors,
   which says nothing about how she feels about the user.

## 2. Goals, non-goals, anti-goals

**Goals.** The clothing table of §4 for `swimming_together`; the fixed
clothing of §5 for `skinny_dipping_together`; the action and kind of §6;
the refusal line of §7.

**Non-goals.** Swimwear per persona or per deployment (no config key, no
card field); a virginity variant for a male persona; any change to the hot
spring, sauna, bathing or showering rules; any change to what the reply
prompt says she wears; deciding whether a fishing trip caught anything.

**Anti-goals.** The words `school` or `sukumizu` in any clothing line; a
stored copy of her virginity or of the chemistry at the time of the
picture.

## 3. Approaches

**Who picks the swimsuit.** Three ways: a fixed table in the engine, keyed
on the persona's gender, romance compatibility, her virginity and the
chemistry score, in the same function that already writes the hot spring
and sauna clothing; hand the composer the chemistry tier and her virginity
and let it choose; swimwear set per persona in `art_metadata`. The first is
taken. The owner named the garment for each tier, so a model choosing would
drift from it, and one ladder for every persona was asked for, which a card
field would split and would make web work for no stated need.

## 4. What she wears swimming

**Rule.** When the `[此刻状态]` of a picture names `swimming_together`, the
engine writes the composer's `[此刻穿着]` from the table below, over the
judge's `clothing` and over an outfit the caller named, exactly as §5 of
the 2026-11-26 spec does for a hot spring. Which pictures, and "only the
composer's slot", are as that section says.

**Female persona** (`art_metadata.gender` is `female`), romance-compatible:

| chemistry | not a virgin | a virgin |
|---|---|---|
| tier 1 (< 0.15) | `Tankini top` or `Bralette bikini`, one of the two at random per picture | `navy one-piece swimsuit with name patch and a front drain flap` |
| tier 2 (0.15–0.35) | `Triangle bikini` | `navy one-piece swimsuit with name patch and a front drain flap` |
| tier 3 (0.35–0.62) | `Bandeau bikini` | `navy one-piece swimsuit with name patch and a front drain flap` |
| tier 4 (0.62–0.90) | `Halter bikini` | `Halter bikini` |
| tier 5, below 0.95 | `String bikini` | `String bikini` |
| 0.95 and above | 什么都没穿 | 什么都没穿 |

**Female persona**, not romance-compatible: `plain one-piece swimsuit`.

**Male persona** (`art_metadata.gender` is `male`), romance-compatible:

| chemistry | `[此刻穿着]` |
|---|---|
| tier 1 | `rash guard with board shorts` |
| tier 2 | `board shorts` |
| tier 3 | `swim trunks` |
| tier 4 | `square-cut swim trunks` |
| tier 5, below 0.95 | `swim briefs` |
| 0.95 and above | 什么都没穿 |

**Male persona**, not romance-compatible: `board shorts`.

**Any other gender**, or none: the engine writes nothing and the composer
reads the judge's clothing, as today.

*(Owner's rulings: every garment in both ladders and both non-compatible
lines; Tankini and Bralette at random on tier 1; the virgin variant for
tiers 1–3 only; the virgin wording with the drain flap and without
`school`; nothing on at 0.95 and above; no override for a gender other than
`female` or `male`; no virginity variant for a male persona.)*

**The inputs.**

- **Tier** is `affinity::tier_index` of the chemistry score, the cuts every
  chemistry label uses. **0.95** is a new constant, the line inside tier 5
  above which she has nothing on.
- **Chemistry** is the affinity the picture's path already holds when it
  composes: on a chat turn, the turn's affinity after decay and before this
  turn's deltas; in a group, the speaker's seat; for an event picture, the
  decayed affinity the proactive path loads. *(Engine's call.)*
- **Romance-compatible** is read as for a hot spring (2026-11-26 §5).
- **A virgin** is `art_metadata.is_virgin` and no `having_sex` or
  `just_have_sex` row ever (`EventRepo::ever(instance_id, EventKind::SEX)`),
  the reading the event draw already uses; the first time with the user ends
  it. *(Engine's call.)* The read is made only when its answer can change
  the line: a female persona, romance-compatible, `is_virgin` set, tier 3 or
  below. A failed read is warned and reads as no such row, so the card
  stands; it never blocks the picture. *(Engine's call.)*
- **The random pick** on tier 1 is a fair coin per picture, not stored.
  *(Engine's call.)*
- Garment names go into the slot in English, verbatim; the naked line
  reuses the hot spring's 什么都没穿. *(Engine's call.)*

## 5. What she wears skinny-dipping

When the `[此刻状态]` of a picture names `skinny_dipping_together`, the
engine writes 什么都没穿 into `[此刻穿着]`, for any gender and either way
on romance compatibility (the action is ruled out before the judge for a
pair that is not compatible, so the event only starts for one that is).
Which pictures, and "only the composer's slot", as in §4. *(Owner's
ruling.)*

## 6. `fish_together`

| action `type` | marker | places | label | category | lasts | fits |
|---|---|---|---|---|---|---|
| `fish_together` | 想和你一起钓鱼 | `fishing_together` | 和对方一起钓鱼 | `mid` | 2–3 h | both |

It behaves as `hike_together` and `adventure_together` do: no payload; the
judge decides; an accept places the kind in the write that stores the
answer (no event inside a date) and ends the incident under way; the
accepted turn is a picture turn when the image executor is available; a
refusal drops bond (§7); no romance gate, no species gate, no world rule,
no period roll; not answered in the `emotional` tone. The kind is
`SPECIAL`: weight 0, no cooldown, `Neutral` place, never told, not offline,
shared with the user (`[对方]`), and it reaches the story director as the
other outings do. No clothing rule.

*(Owner's rulings: the action, `mid` 2–3 hours, and the bond refusal.
Engine's calls: the marker, the label, both worlds, and every other rule
that follows `hike_together`.)*

## 7. Refusals that drop bond

`UserAction::refusal_line` returns `Bond` for `jog_together`,
`hike_together`, `fish_together`, `cycle_together` and
`ride_horse_together`, beside `give`, `gift` and `commit_crime_together`.
Nothing else about a refusal changes: the drop is the same one-tier drop,
on the other line. *(Owner's ruling, the five actions and the reason in
§1.)* `adventure_together` and `walk_together` stay on chemistry: neither
has to happen outdoors. *(Owner's ruling.)*

What follows from it: a refusal of one of the five while an intimate event
is under way (2026-11-26 §6) no longer ends that event, since only a
chemistry drop does.

## 8. Tests

- `prompt.rs`: the clothing function, table-driven over every row of §4 and
  §5, both genders, another gender, compatible or not, virgin or not, the
  tier cuts and the 0.95 line from either side; tier 1 returns each of the
  two lines for the two coin values.
- `stream.rs`: an accepted `swim_together` turn's composer payload carries
  the tier's line; a virgin persona at tier 2 carries the navy line, and the
  same persona after a `having_sex` row carries `Triangle bikini`;
  `skinny_dip_together` carries 什么都没穿.
- `state_for_picture`, which group and event pictures share: the swim line
  read from a seeded `swimming_together`, as the sauna test reads its towel;
  a group picture's composer payload carries the speaker's swim line, as
  the group sauna test does.
- `fish_together`: the action round-trips on the wire, maps to
  `fishing_together`, is `mid` 2–3 h, `SPECIAL`, shared, fits both worlds,
  and reaches the story director's `life_events`.
- `types.rs`: `refusal_line` is `Bond` for the five actions of §7 and
  stays `Chemistry` for the other together actions.

What a picture looks like is judged by the owner on real output; no test
asserts on the image.

## 9. Order of work

One code PR: §4 to §7 together. Documentation in a PR of its own after the
code PR is merged.

## 10. Migrating notes to write

One new action type, `fish_together`, and one new event kind,
`fishing_together`, both additive; a client that lists together actions adds
it. Refusing `jog_together`, `hike_together`, `cycle_together` or
`ride_horse_together` now moves the bond line instead of the chemistry line,
which a client showing tier changes will see. No migration. These changes
reach every user the moment the image is deployed; rolling back is
redeploying the previous image.

## 11. Left to the builder

- The function's signature and how a path hands it gender, virginity,
  chemistry and the coin.
- Where the 0.95 constant lives.
- The exact wording of doc comments and test names.

# `outdoorsy`, `missed_ride` and an accept that ends the incident — Design

- **Date:** 2026-11-11
- **Status:** Approved
- **Type:** Engine change. No migration. Additive HTTP change: the events
  route's kind enum gains `missed_ride`. The character card 1.1 gains the
  `outdoorsy` (string) key. `PersonaTraits` gains one field. One behaviour
  change: accepting `sleep_together` or `propose` ends the incident under
  way.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-11-eight-short-events-and-hygienic-design.md`
  (`hygienic_factor`, `employed_factor`, `bathe_together`) and
  `2026-11-11-stocks-lost-habit-events-and-just-have-sex-design.md`
  (`needs_background`, `got_lost`). Everything there stands except where
  this document says otherwise.

## 1. Intent

When the user says yes to her, what she was busy with a moment ago gives
way. On a trip she can miss her ride. And a persona can be someone who
loves the outdoors or someone who stays in: each foreground kind is
outdoor, neutral or indoor, and the persona's `outdoorsy` key tilts her
day toward one side.

## 2. An accept ends the incident under way

Today an accept that places an event (`ask_for_space`, `travel_together`,
`bathe_together`) writes it with `insert_preempting_in_tx`: every
foreground under way ends at the moment of the accept, a background keeps
running.

From this change an accept of `sleep_together` or `propose` ends the
foreground under way the same way, without placing anything: its
`ends_at` becomes the accept's `now`, in the accept's transaction. The row
stays; the events list shows it as ended. A background keeps running. A
decline, an ignore and a lapse change nothing.

The cap is the statement `insert_preempting_in_tx` already runs (under the
instance lock), split out so both callers share it; one definition of
"ends the foreground".

## 3. `missed_ride`

| | |
|---|---|
| kind | `missed_ride` |
| category | `short` |
| weight | 3.0 |
| cooldown | 48 h |
| lasts | 1 h |
| label | 没赶上车 |
| cue | （你刚没赶上车。） |
| cue during `traveling_together` | （你们一起出来玩，刚没赶上车。） |
| told | yes |
| offline | no |
| setting | both (a carriage is a ride too) |
| place (§4) | outdoor |

`needs_background`: drawn only while one of `business_trip`, `short_trip`,
`long_trip`, `hometown_visit`, `traveling_together` is under way. Unlike
`got_lost`, a business trip counts. No gate, no follow-on.

## 4. Outdoor, neutral, indoor

`EventKind::place(self) -> Place { Outdoor, Neutral, Indoor }` labels every
drawable foreground kind, in `short` and `mid`. Backgrounds and the placed
kinds (`bathing_together`, `taking_space`, `fell_out`) are `Neutral`; no
factor reads them. The sex kinds, `streaking` included, are neutral.

Outdoor means out in the open air; a shop, a class, an office or a gallery
is indoor or neutral, not outdoor.

| place | `short` | `mid` |
|---|---|---|
| outdoor | `police_encounter`, `animal_encounter`, `on_the_way`, `got_bumped`, `bumped_someone`, `admiring_scenery`, `bird_encounter`, `want_to_see_sea`, `want_hot_spring`, `got_lost`, `missed_ride` | `on_the_road`, `want_to_go_out` |
| indoor | `showering`, `bathing`, `in_class`, `scolded_by_superior`, `scolded_superior`, `admiring_art`, `want_sauna`, `watching_stocks` | `reading`, `gaming`, `want_to_go_home` |
| neutral | every other drawable `short` kind | every other drawable `mid` kind |

Every kind that can take her offline (`may_be_offline`: `asleep`,
`in_battle`, `busy`, `away_from_phone`, `out_shopping`, the gatherings,
`having_sex`) is neutral, whatever its setting: how often she is out of
reach stays with those kinds' own rules, as for `employed`.

## 5. The `outdoorsy` key

`art_metadata.outdoorsy`: `"outdoor"`, `"neutral"` or `"indoor"`. Absent,
or any other value, reads as `neutral`, so every existing persona keeps
today's draw. `PersonaTraits.outdoorsy: Outdoorsy { Outdoor, #[default]
Neutral, Indoor }`, parsed as `Employment` is.

`outdoorsy_factor` scales a kind within its category by its place:

| `outdoorsy` | outdoor kinds | indoor kinds | neutral kinds |
|---|---|---|---|
| `outdoor` | ×3 | ×⅓ | ×1 |
| `neutral` (default) | ×1 | ×1 | ×1 |
| `indoor` | ×⅓ | ×3 | ×1 |

×3 is `PREFERENCE_FACTOR`. Its own factor, so it multiplies with
`needs_factor`, `hygienic_factor` and the rest.

It is a `PersonaTraits` field, not a `Habits` key: no indulgence drift, no
`give` item, not in the judge's brief. Read by life events only.

## 6. Goals, non-goals, anti-goals

- **Goal:** a persona with no `outdoorsy` key draws exactly as today apart
  from `missed_ride` joining the `short` pool during trips.
- **Non-goal:** `place` replacing the hospital rule-out list. The two
  overlap but answer different questions (open air vs. able to leave the
  ward); both stay.
- **Non-goal:** `outdoorsy` touching cooldowns, durations, gates or the
  category draw.
- **Non-goal:** ending a background on accept.
- **Anti-goal:** an accept deleting the incident's row: the history keeps
  it.

## 7. Character card 1.1

`outdoorsy` (string) joins 1.1 beside `hygienic` and `employed`, read by
life events. Its *Versions* line gains it.

## 8. Documentation

English and Chinese where both exist: `api-reference` (events kind table,
persona settings, the life-choice accept effects), `life-events-needs`
(the kind, the trip-only rule, the place table, persona settings, the
accept), `eros-character-v1.1.md`. The v2.6.5 migrating note gains
`missed_ride`, `outdoorsy` and the accept behaviour.

## 9. Left to the builder

- The function name for the shared cap and where `missed_ride` sits in
  `ALL`.
- Test names; the label and cue pin tests gain one row each.

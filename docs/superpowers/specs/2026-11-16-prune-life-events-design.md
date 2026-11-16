# Prune the life-event catalog — Design

- **Date:** 2026-11-16
- **Status:** Draft
- **Type:** Engine change. No migration. Breaking for a caller that names a
  deleted kind in `kinds`: the draw answers `422`.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-15-life-events-v2-design.md`,
  `2026-11-08-strip-and-world-setting-design.md`,
  `2026-11-11-stocks-lost-habit-events-and-just-have-sex-design.md`,
  `2026-11-11-outdoorsy-missed-ride-and-accept-ends-incident-design.md`.

## 1. Intent

A life event earns its place when it can happen at any moment and moves the
user: sudden desire fits, because it needs no lead-up and stirs a reaction.
Grocery shopping does not: it arrives with no context, so she may break off
whatever she was doing to go buy vegetables. In a production deployment the
most frequent kinds were of this everyday-errand sort.

The catalog keeps the kinds that are general, carry feeling, or give the user
something to answer, and drops or narrows the rest.

## 2. Goals, non-goals, anti-goals

- **Goals:**
  - delete seven kinds (§4.1);
  - draw `on_the_way` and `on_the_road` only during a trip (§4.2);
  - make `temp_work` and `scolded_superior` modern only (§4.3).
- **Non-goals:**
  - No compensation for the shift in frequency. How often an event happens
    is the caller's per-turn roll; a draw always picks a kind, so the share
    the deleted kinds held goes to the rest of their category (roughly a
    quarter more for each remaining `short` kind and a third more for each
    remaining `mid` kind). A caller that wants fewer events lowers its roll.
  - `temp_work` stays online: she announces the job and keeps replying. It
    reads as an excuse, not as being truly busy; `busy` is the kind that
    takes her offline.
  - No change to any kind not named here.
  - No rewrite of stored rows.
- **Anti-goal:** keeping a deleted kind as a name that is never drawn.
- **Kept on purpose, never to be deleted:**
  - `temp_work`: people in real chats brush someone off with work as the
    excuse, and the companion should be able to do the same;
  - `want_sauna`: it sets up a scene for a picture.

## 3. Approaches

- **A — delete the kinds end to end (chosen).**
- **B — keep the names, never draw them.** Dead names in a public enum, and
  `kinds` keeps accepting kinds that can never come up.
- **C — delete the kinds and let `kinds` ignore unknown names instead of
  answering `422`.** Every caller loses the check that catches a misspelled
  kind.

## 4. Design

### 4.1 Deleted kinds

| Kind | Category | What it was |
|---|---|---|
| `grocery_shopping` | `short` | out buying groceries |
| `in_class` | `short` | in class |
| `watching_stocks` | `short` | watching stocks |
| `trading` | `short` | making a deal with someone |
| `bumped_someone` | `short` | bumped into someone |
| `weather_change` | `mid` | the weather turned |
| `watching_match` | `mid` | watching a match |

Each leaves `EventKind`, `EventKind::ALL` (70 → 63 drawable kinds), every
rule that names it (category, place, setting, weight, the hospital
rule-out, `weather_change` → `unwell` as a follow-on), the reply prompt's
event label, the event message's cue, and `EventKindDto`;
`openapi.json` is regenerated.

What already holds for a kind this build does not know, and now applies to
these seven:
- a stored row of one reads as no event, so one under way at the upgrade
  drops out of `…/effects` and the draw, while the message it sent stays in
  the chat;
- `prefer_events` naming one ignores it.

`kinds` naming one answers `422`.

### 4.2 Commute kinds only on a trip

`on_the_way` and `on_the_road` are drawn only while `business_trip`,
`short_trip`, `long_trip`, `hometown_visit` or `traveling_together` is under
way: the set `missed_ride` already needs. Their follow-on weight from a trip
stays.

### 4.3 Modern only

`temp_work` and `scolded_superior` join the kinds a `fantasy` persona never
draws, whatever `kinds` or `prefer_events` say. A fantasy persona has
`hit_someone` for a clash with someone above her. `kinds` still accepts both.

### 4.4 Docs

- `api-reference` (both languages): the kind table and its count, the
  hospital rule-out list, the trip-only kinds, and the `setting` list.
- `life-events-needs` (both languages): the same facts where it states them.
- A migration guide under `docs/migrating/`.
- Older specs and migration guides stay as written.

### 4.5 Release

Ships in v2.7.1, together with the four needs. That release is not out yet
(builder's call; the user may move it).

## 5. Downstream

A client that keeps its own catalog of kinds (labels, icons, categories)
drops the seven. Either deploy order is safe: a client that drops kinds it
does not know shows no event for one the old engine still draws, and an
unused catalog entry is harmless.

## 6. Left to the builder

- test rewrites;
- doc wording;
- whether to drop now-redundant entries, such as the commute kinds in the
  hospital rule-out list.

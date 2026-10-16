# Life events: run-ins, quarrels and kind filters — Design

- **Date:** 2026-10-17
- **Status:** Approved
- **Type:** Engine change. No migration. Additive API changes (five kinds,
  an object form of `kinds`). `eros-engine-core::events` gains five
  `EventKind` variants (breaks exhaustive matches).
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — five new `short` kinds; `kinds` on the events
  route accepts an `{allow, disallow}` object beside the list.
- **Builds on:** `2026-10-15-life-events-v2-design.md` (*the v2 spec*) and
  `2026-10-16-life-events-offline-and-scares-design.md` (*the offline spec*).
  This spec extends the v2 kind table (§3.2), its exclusions (§3.3), its
  needs factors (§3.5) and the `kinds` field of the route. Everything else
  stands.

## 1. Motivation

A day has small frictions the 33 kinds do not cover: a knock on the street,
a row with someone, a scolding at work, or her turning on whoever is above
her. And a caller needs to rule kinds out without listing every other kind:
a persona's world has no superior, or a free tier is not served adult
themes. An allowlist cannot do that without freezing the set it lists, so a
kind added later would never be drawn.

## 2. Kinds

| kind | label | category | weight | cooldown | lasts |
|---|---|---|---|---|---|
| `got_bumped` | 被撞了 | `short` | 1 | 72 h | 30 min |
| `bumped_someone` | 撞人了 | `short` | 0.5 | 7 d | 30 min |
| `quarrel` | 吵架了 | `short` | 1 | 48 h | 1 h |
| `scolded_by_superior` | 被上级骂了 | `short` | 1 | 72 h | 1 h |
| `scolded_superior` | 骂上级了 | `short` | 0.5 | 14 d | 1 h |

That makes 38 kinds, 16 of them `short`. None has a gate, none is offline,
and all are told. The two-stage draw keeps every other category's share: the
five only dilute the `short` kinds already there.

### 2.1 Cues

| kind | cue |
|---|---|
| `got_bumped` | （你刚走在路上，被擦撞了一下。） |
| `bumped_someone` | （你刚不小心撞到了人。） |
| `quarrel` | （你刚跟人吵了一架。） |
| `scolded_by_superior` | （你刚被上级骂了一顿。） |
| `scolded_superior` | （你刚把上级骂了一顿。） |

Each cue states the fact and leaves the rest to the persona. `got_bumped` is
a person or a vehicle brushing into her on foot; `bumped_someone` is her
knocking into someone on foot or in whatever she rides or drives, a car or a
tank in a modern world, a horse or a flight in a historical or fantasy one.
Who she quarrelled with is hers to say. 「上级」 is read in her world: a boss,
a manager, a master, a sovereign. The examples stay out of the cue, so the
model does not pick one of them off a list.

### 2.2 Exclusions

`got_bumped` and `bumped_someone` join the kinds ruled out during
`hospitalized` and `post_surgery` (v2 spec §3.3): she is not out and about.
The other three can happen anywhere.

### 2.3 Needs

`quarrel` weighs ×2 when `emotional` is `high`, beside `animal_encounter`
and `irritable`.

## 3. `kinds`

| request | kinds that may be drawn |
|---|---|
| absent or `null` | all |
| `[]` | none: the response is `event: null`, nothing is written |
| `["a", "b"]` | the listed ones |
| `{"allow": […], "disallow": […]}` | those `allow` permits, minus those `disallow` lists |

In the object, `allow` is what is allowed and `disallow` what is not:

- `allow` absent or `null` allows every kind; `[]` allows none; a list
  allows those.
- `disallow` absent, `null` or `[]` removes nothing.
- A kind in both is not drawn.
- `{}` is the same as absent.

An unknown key in the object or an unknown kind name anywhere rejects the
request, as an unknown kind does today. A misspelt `disallow` must not
silently serve a kind the caller meant to exclude; the cost is that a
caller lists a new kind only once the engine knows it.

The offline gates (offline spec §4.1) still run after the filter.

## 4. Adult themes

The engine has no built-in group of adult kinds. The docs name the kinds
with adult themes so a caller can list them under `disallow`:
`sudden_desire`, and `pregnancy_scare`, which implies sex.

## 5. API

- `EventKindDto` gains the five kinds; absent `kinds` means all 38.
- `kinds` is an array of kinds or an object `{allow?, disallow?}`.
- OpenAPI regenerated.

## 6. Rollback and deploy

- **No migration.** A previous image reads an event of a kind it does not
  know as absent, as for the offline spec's kinds, and rejects a request
  with the object form of `kinds`.
- **Deploying.** With `kinds` absent, the five kinds are drawn from the
  first draw.

## 7. Testing

- **core.** `ALL` holds 38 distinct kinds, 16 `short`; the five durations;
  `got_bumped` and `bumped_someone` ruled out in hospital and after surgery,
  the other three not; `quarrel` ×2 at high `emotional`. Tests pinned to the
  weights of the 28 v2 kinds keep drawing from those.
- **route.** Each form of `kinds` resolves to its set: absent, `null`, `[]`,
  a list, `{}`, `allow` only, `disallow` only, a kind in both; an unknown key
  and an unknown kind are rejected.
- **proactive / prompt.** Each cue and each label.
- **Regression.** Every other edit to an existing test is a call form or a
  count. `openapi.json` regenerated.

## 8. Documentation

- `docs/api-reference.md` / `.zh.md`: the kind table, the exclusions, the
  `kinds` forms.
- `docs/life-events-needs.md` / `.zh.md`: the kinds, the needs factor, the
  `kinds` forms, the adult themes.
- Every count of kinds (33 → 38) wherever it appears.

## 9. Non-goals

- A built-in adult or work group of kinds.
- Gating the superior kinds on the persona's occupation; the caller's
  `disallow` covers a persona without a superior.

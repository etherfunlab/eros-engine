# Affinity model

[English](affinity-model.md) · [中文](affinity-model.zh.md) · [日本語](affinity-model.ja.md)

## Overview

Affinity is a six-axis vector, one per relationship (one per session). Every
axis is bounded to `[0, 1]` and clamped on every update. It moves only on
text-channel, non-`product_qa` chat turns; voice turns and `product_qa`
turns never write an affinity event, and neither do group turns (a group
reads each member's affinity and writes none). Two moves are not turns: the
tier drop of an answered life choice ([below](#life-choice-drops-and-the-cold-war-halving)),
and the one verdict on a settled date ([below](#dates)).

On top of the six axes each session stores a [relationship status](#relationship-status):
what the pair has agreed to be, moved only by life choices and two user actions,
with a cold-war flag over it.

The judge (the LLM evaluator) only ever outputs ordinal buckets: a grade
or a level, never a continuous number. The engine owns every score, every
label, and every tier transition it derives from those buckets; it is the
single source of truth.

The six axes fall into three groups, and this doc is organized around them:
the base pair (`warmth`, `patience`), the Bond line (`trust`, `intrigue`),
and the Chemistry line (`intimacy`, `tension`).

## The three groups

### The base pair — warmth & patience

`warmth` and `patience` are not accumulated state. Each judged turn the
evaluator reports one absolute **level** per endpoint — `1` cold/impatient,
`2` baseline (the overwhelmingly common verdict), `3` clearly warm/invested
— and the engine derives the continuous value from it:

```
base(level)  = (level − 1) / 3                          ∈ {0, 1/3, 2/3}
B(x)         = 1 + λ·(x − 0.35)                         λ = (1.5−1)/(1−0.35) = 10/13
decay(Δt)    = max(FLOOR, 1 − RATE·days)                Δt since updated_at

warmth   = clamp01( max(base(w_level)·B(chemistry), φ·chemistry) × decay )
patience = clamp01( max(base(p_level)·B(bond),      φ·bond)      × decay )
```

The coupling direction is **amplification, not correlation**: deeper
Chemistry warms expression, and deeper Bond funds patience.

Low bond × high chemistry natively produces the tsundere register
(impatient but warm). High bond × low chemistry produces the old-friend
register (patient but cool). Neither needs prompt special-casing; both fall
out of the formula.

A fresh session defaults both endpoints to ≈ `0.244`, the level-2 damped
base. A stranger starts with limited patience; that is deliberate.

The base pair is what the user *feels* turn to turn. It feeds directly
into:

- the `[relationship]` prompt section, when the pair has no status, no
  composed relation and no cold war ([`[relationship]`](#relationship)): a
  warmth × patience quadrant, each axis split at `0.5`. The four cells are
  the actual prompt strings, never translated:

  | | patience ≥ 0.5 | patience < 0.5 |
  |---|---|---|
  | **warmth ≥ 0.5** | 好朋友 | 快被磨光耐心的朋友 |
  | **warmth < 0.5** | 没什么交情的人 | 死对头 |

  Independent of the Bond/Chemistry tiers; omitted when either axis is
  outside the request's scope, or no affinity row exists yet.
- `[mood]` cold gates (`warmth ≤ 0.2` fires a cold-tone directive,
  `patience < 0.35` an impatient one), plus the per-turn dice vetoes behind
  the same floors.
- the patience band shown to the PDE judge: low `[0, 0.35)`, mid
  `[0.35, 0.65)`, high `[0.65, 1]`.

In code these two are the **derived endpoints**. The stored
`warmth`/`patience` columns are a materialized cache of the derivation,
refreshed wherever time decay runs. The authoritative facts on the row are
the four line axes, the two judge levels (`warmth_grade` / `patience_grade`,
`1..=3`), and `updated_at`.

### The Bond line — trust & intrigue

`trust` tracks topic depth and willingness to disclose self. `intrigue`
tracks curiosity and follow-up questions; it is the anti-ghost driver.

```
bond = (trust + intrigue) / 2   ∈ [0, 1]
```

Bond is friendship: trust plus continued interest.

Bond has five tiers, serialized snake_case: `acquaintance`, `friend`,
`close_friend`, `confidant`, `soulmate`.

### The Chemistry line — intimacy & tension

`intimacy` tracks inside jokes, nicknames, callbacks to earlier details.
`tension` tracks push-pull and playful friction, the tsundere affordance.

```
chemistry = (intimacy + tension) / 2   ∈ [0, 1]
```

Chemistry is romance: closeness plus charge.

Chemistry has five tiers: `spark`, `flirtation`, `crush`, `lover`,
`beloved`.

Both lines exclude the base pair by construction: the lines are the base
pair's *inputs*, never the other way around. As of 4.0 the two lines share
nothing.

A fresh session seeds all four line axes at `0`, so bond = chemistry = 0 and
both lines start at tier 1. There is no separate "stranger" state: tier 1
reads `acquaintance` + `spark`.

## How values move

### Judge protocol (fully ordinal)

For each of the four line axes the judge reports an integer **grade**
`0`–`4` plus a **direction** (`"up"`/`"down"`). For each base-pair endpoint
it reports an absolute **level** `1`–`3`, a state read for this turn, not a
delta.

Grade rubric: `0` = nothing happened (the overwhelmingly common verdict);
`1` = small but real; `2` = a clear push or a clear hurt; `3` = a rare
significant moment; `4` = a milestone (extremely rare). Negative moments are
prompted to fire readily.

Example verdict:

```json
{
  "warmth":   2,
  "trust":    {"grade": 1, "direction": "up"},
  "intrigue": {"grade": 0, "direction": "up"},
  "intimacy": {"grade": 0, "direction": "up"},
  "tension":  {"grade": 2, "direction": "down"},
  "patience": 2,
  "reason": "…"
}
```

Why ordinal: models are reliable ordinal raters and unreliable calibrated
arithmetic. The continuous distribution users see is folded out of these
discrete buckets by engine math. 4.0 removed the last continuous judge
output, a 0.1-step patience read that ceiling-packed in production.

Malformed verdicts reject wholesale, in `parse_affinity_eval`: unparseable
JSON, an out-of-range grade, an unknown direction, or a malformed level all
reject the whole verdict: all-zero grades, no level reads, an empty reason.
The turn's rule deltas still persist. Salvage rules: an omitted or `null`
axis reads as grade 0; an omitted or `null` level holds the stored level;
quoted integers (`"2"`) are salvaged.

Banded input: the judge sees the four line-axis reads as coarse bands
(低/中/高, cut at `0.35`/`0.65`), never floats. Current `warmth`/`patience`
are deliberately not injected: a stateless absolute read avoids anchoring
inflation.

Register hygiene: the evaluator prompt is first-person, in-character,
engine-owned, and not configurable. `reason` forbids system vocabulary,
because it is persisted and re-injected later as `[emotional_context]`.

### Write pipeline

Line axes only; the base pair never enters this pipeline:

```
grade → raw score → tier decay → cross-line penalty → threshold gate → clamp
                                                    → endpoint derivation
```

All four stages run inside `grade_turn`, computed against the pre-turn
snapshot and applied under the row lock.

**1. Conversion.** Signed grade × line unit: `AFFINITY_GRADE_UNIT_BOND`
`0.0786` (trust/intrigue), `AFFINITY_GRADE_UNIT_CHEM` `0.0266`
(intimacy/tension). Negative raw is further multiplied by
`AFFINITY_NEG_FACTOR` `1.5` (slow up, fast down). Demo sessions
(`metadata.is_demo`) multiply positive judge raw by `AFFINITY_DEMO_BOOST`
`1.4`. The ~3× spread between the two units is the judge's measured grading
asymmetry (tension reaches grade ≥2 on roughly half of turns; trust is
graded 0 on ~80%), written down where it can be argued with. PDE rule
nudges (e.g. intrigue `+0.02` on a long user message) join the raw score
pre-decay.

**2. Tier decay, positive only.** Positive raw is multiplied by the axis's
own line's tier factor, `AFFINITY_TIER_DECAY` = `1.0, 0.70, 0.45, 0.25,
0.10` for tiers 1–5. Negative raw never decays: losses stay full price at
any tier.

**3. Cross-line penalty.** The other line's height taxes the move, in
proportion to the grade actually applied:

```
penalty = κ_line × ((y − y₀)⁺ / (1 − y₀))² × (|g| / 4)
  y      = the OTHER line's score
  κ_line = AFFINITY_CROSS_PENALTY_RATIO × u_line   (ratio default 5/6)
  y₀     = AFFINITY_CROSS_PENALTY_START            (default 0.35)
```

Grade `0` charges nothing: the pipeline charges events, not rent. Ignoring
rule nudges, the term factorises so that neither bracket carries `g` or `u`:
the outcome cannot change sign between grades at a fixed position, and the
break-even position is unit-invariant. At defaults only own tier 5 has a
real break-even (counterpart ≈ `0.800`); past it every grade nets negative,
uniformly.

**4. Threshold gate.** Each axis keeps a signed accumulator; it commits once
`|accumulated| ≥ AFFINITY_DELTA_THRESHOLD` (default `0`, so every turn
commits), otherwise the turn's real score buffers in `pending_deltas`.
Committed deltas apply 1:1 and clamp to `[0,1]`; while a cold war is on, the
positive ones are first halved
([below](#life-choice-drops-and-the-cold-war-halving)). The judge's levels, when
read this turn, then overwrite the stored levels, and both endpoints
re-derive from the post-turn lines.

### Time

Line-axis drift is lazy, computed on load from `updated_at`: `intrigue`
drifts `−0.01`/day, `tension` `−0.005`/day. `trust` and `intimacy` never
decay; they are the deep dimensions.

Base-pair absence decay is multiplicative, inside the derivation itself:
`AFFINITY_TIME_DECAY_RATE` `0.02`/day, floored at `AFFINITY_TIME_DECAY_FLOOR`
`0.5` (7 days → ×0.86; 25+ days → ×0.5). Absence cools but never zeroes:
an old relationship keeps a floor through the boost (bond `0.9` at full
decay still yields patience ≈ `0.47`). The pre-4.0 patience up-drift is
retired.

### Turns that do not move

A skipped eval (`eval_skip_reason`) or a failed one (non-empty
`llm_attempts` / `gateway_errors`) holds the stored levels; the endpoints
simply re-derive with the current lines and decay. The old rule-delta
fallback is retired. The turns of a [date](#dates), and the turn whose
invitation opened it, are skipped evals with `eval_skip_reason = "activity"`,
and their rule nudges are zeroed as well.

A ghost the PDE decides never reaches `persist_with_event`: only
`ghost_streak` / `total_ghosts` / `last_ghost_at` and `updated_at` move.
`record_ghost` writes an all-zero `effective_deltas`; no axis moves at all, but
the reset `updated_at` restarts the decay clock of [Time](#time). The ghost the engine imposes
during an offline stretch moves none of them (see
[ghost-mechanics.md](ghost-mechanics.md#ghosting-the-engine-imposes)).

### Refused actions

A chat turn can carry a user action (see the `action` field in
[api-reference.md](api-reference.md)). The PDE judge decides before the reply
whether the persona accepts it; without a definite refusal the engine accepts,
except for a give of tobacco, alcohol, weed, coffee or a dish her persona does not take, or the
user's ask to smoke, smoke weed or drink with her when she does not, which a
roll can refuse whatever the judge says
([persona settings](life-events-needs.md#persona-settings)).

- **Accepted:** nothing special. The affinity judge sees the action in the
  user text and grades the turn as usual.
- **Refused:** the line the action touches (`give` and `gift` → Bond, every physical
  action → Chemistry) drops one tier, landing at the same relative position
  inside the lower tier; a tier-1 line goes to `0`. Both axes of the line are
  scaled by the same factor, so their ratio is kept. The drop is computed from
  the pre-turn snapshot inside the row lock and replaces whatever the judge's
  grades did to that line; the other line and the base-pair levels follow the
  judge. That line's `pending_deltas` are cleared, and the event row's
  `context.tier_drop` names the line. The drop applies even when the affinity
  eval is skipped or fails.
- **Refused tip:** a `tip` drops no line. The persona's reply follows the
  verdict; the turn is graded like any other.
- **Social actions:** a refused `bluff`, `threaten` or `trick` means she saw
  through it, and a refused `challenge` or `bet` that she turned it down. Neither
  drops a line; the turn is graded like any other.
- **Strip:** a `strip` from a pair whose `affinity_scope` is not romance-compatible is
  refused whatever the judge says, and drops Chemistry like any refused
  physical action. So is a `shower_together`, `bath_together`, `sex_together` or
  `streak_together` from such a pair, and a `toilet_together` to a machine persona. A
  `fight_together` to a persona whose `setting` is not `fantasy` is refused the same way and drops
  no line. The other together actions have no such rule. A `smoke_together`, `smoke_weed_together` or
  `drink_together` the roll refuses drops Chemistry too, not Bond.
- **Proposal and break-up:** a refused `propose` or `break_up` drops no line; the turn is
  graded like any other. Before the judge, a `propose` from a status other than `dating`,
  `fwb` or `situationship`, and a `break_up` from a status that is not breakable, are refused
  the same way; an answered one moves the [relationship status](#relationship-status).
- **Gift:** accepted, the gift also joins the persona's items or wardrobe;
  refused, it joins nothing and drops Bond like a refused `give`.
- **Date:** a refused `date` invitation drops no line; the turn is graded like
  any other. An accepted one is not graded at all: it opens the date, which is
  judged once when it is settled ([below](#dates)).

Example: chemistry `0.80` (64% of tier 4) lands at 64% of tier 3, `≈ 0.524`.

### Life-choice drops and the cold-war halving

A life choice she puts to the user costs the relationship on some answers
([life choices](life-events-needs.md#life-choices)). Nothing here is
a chat turn: no judge runs.

- **The drop.** Only a decline drops a line, and which line depends on the kind:

  | kind | accept | decline |
  |---|---|---|
  | `confess` | no drop | chemistry |
  | `partner_up` | no drop | no drop |
  | `propose` | no drop | bond and chemistry |
  | `break_up` | no drop | no drop |
  | `sleep_together` | no drop | chemistry |
  | `fall_asleep_together` | no drop | chemistry |
  | `bathe_together` | no drop | chemistry |

  Each line named falls one tier by the landing rule of a refused action: the
  same relative position one tier down, both axes of the line scaled by the
  same factor, a tier-1 line to `0`. Its `pending_deltas` are cleared; a line
  not named keeps its axes and its balance. It is computed from the snapshot
  after time decay and the endpoint refresh (with the deployment's affinity
  tuning), inside the row lock and in the transaction that resolves the
  choice, so a rolled-back resolution leaves no drop. An answer typed in words
  (`ignored`) and an expiry drop no line, for any kind.
- **Its event row.** One row of its own non-turn `event_type`, `life_choice`,
  for every drop. `deltas` and
  `effective_deltas` carry the same figures, because the drop is the whole raw
  change; `label_changes`, `effective_line_deltas` and the two state snapshots
  record the tier move. `context` holds `life_choice` (`accept` | `decline`)
  and `choice_id`. No accept drops a line, so a row carries `decline`; `accept`
  appears only on stored rows of the retired `ask_for_space`. The row has no
  `user_message_id`. The delta route
  `GET /bff/v1/comp/affinity/{session_id}/event` never returns it, like
  `time_decay`, and it is not a judged turn either. A status the answer moves
  writes a `relationship` row of its own ([Event rows](#event-rows)).
- **The cold-war halving.** While the session's [cold war](#the-cold-war) is
  on, every positive committed delta on the four line axes is halved before it
  is applied, on a judged turn and on a date's settlement. Negative deltas
  apply in full, and `warmth` and `patience` re-derive from the post-turn lines
  as always. The cut part is gone: it is not parked in the gate's balance.
  Nothing drops when a cold war starts. The flag is read as stored on the
  session's affinity row; a read that fails commits at full speed. A halved
  turn's event row carries `context.growth_scale` (`0.5`), absent at full
  speed, and its `effective_deltas` show the halved values.

### Dates

A [date](activities.md) is judged once, when it is settled, and its verdict
reaches affinity as one write on the session the invitation was sent in. A date
turn is written on that session's row too, as a skipped eval
([above](#turns-that-do-not-move)); a refused action inside the date still drops
its line.

- **The weighted verdict.** The activity judge returns one verdict in the
  per-turn shape: a grade per line axis and a level per endpoint. It goes
  through the write pipeline once (`grade_weighted`), from the stored state
  after time decay and with no rule nudges, worth `weight` turns:

  ```
  weight = min(turns, cap) × AFFINITY_ACTIVITY_TURN_SHARE
  ```

  `turns` counts the date's substantive turns, and `cap` is
  `AFFINITY_ACTIVITY_TURN_CAP_SHORT` or `AFFINITY_ACTIVITY_TURN_CAP_MID` by the
  date's length ([Tuning knobs](#tuning-knobs)). Each axis's raw score, its
  real score and the assessed cross-line penalty scale by `weight` ahead of the
  threshold gate; tier decay, `AFFINITY_NEG_FACTOR`, the demo boost, the clamp
  and the cold-war halving apply as on a turn. The levels, when the
  verdict parsed, overwrite the stored ones, and both endpoints re-derive.
- **The one-tier cap.** Each line then lands no further than one tier from
  where it stood before the write: up to the same relative position in the
  tier above, the mirror of the refused-action landing rule, and down to the
  same relative position in the tier below. A line past the cap has its two
  axes pulled back along their own deltas by one ratio, so it lands on the
  cap. A tier-1 line can fall to `0`, and a tier-5 line is bounded by the
  clamp. What the cap cuts off is gone: the capped line's `pending_deltas` are
  cleared.
- **Its event row.** One row of the non-turn `event_type` `activity`, with no
  `user_message_id`. `context` carries `activity_id`, `turns`, `weight`, the
  judge's `reason` and `capped` (the lines the cap held, absent when none)
  beside the usual audit keys. The delta route never returns it, it is not a
  judged turn for the life-choice triggers, and its `reason` does not feed
  `[emotional_context]`.

## Base-pair derivation details

Every constant in the endpoint derivation is anchored, not invented:

- **Pivot `0.35`** is the tier-2 upper bound, the same constant: the boost
  turns positive the moment the counterpart line enters tier 3. `0.35`/`0.65`
  are also the judge-input band cuts and the patience bands.
- **`B(1) = 1.5`** makes `⅔ × 1.5 = 1.0`: a full level times a full
  counterpart lands exactly at the ceiling. A structural commitment, so a
  code constant rather than a knob.
- **Floor `φ = 0.2`** (`AFFINITY_FLOOR_RATIO`): a level-1 verdict reads
  `φ·counterpart`, not `0`. A deep relationship going cold one turn keeps
  an ember (`0.18` at counterpart `0.9`); a stranger reads ~0. Since
  `φ·x ≤ 0.2 < 0.244 = ⅓·B(0)`, the floor only ever acts on level 1.

Reachable values at `decay = 1`: level 1 → `[0.0, 0.2]` (continuous in the
counterpart), level 2 → `[0.244, 0.5]`, level 3 → `[0.487, 1.0]`. The level
picks the band; the counterpart picks the position inside it.

Per-turn deltas still exist: `effective_deltas.warmth` / `.patience` are
the derivation's `after − before` across the turn, measured against the
post-decay snapshot, so the absence gap is never attributed to the turn.

## Tiers and labels

Each line has five tiers, with widening gaps then a narrow apex:

| Tier | Score range | Gap |
|------|-----------|-----|
| 1 | [0.00, 0.15) | 0.15 |
| 2 | [0.15, 0.35) | 0.20 |
| 3 | [0.35, 0.62) | 0.27 |
| 4 | [0.62, 0.90) | 0.28 |
| 5 | [0.90, 1.00] | 0.10 |

Scores are served as-is, with no display curve. The easy-early / grind-at-the-top
pacing is real, because write-side tier decay damps positive gains by the
line's own tier.

Two independent sets of five labels, serialized snake_case:

| Line | Tier 1 | Tier 2 | Tier 3 | Tier 4 | Tier 5 |
|------|--------|--------|--------|--------|--------|
| Bond | `acquaintance` | `friend` | `close_friend` | `confidant` | `soulmate` |
| Chemistry | `spark` | `flirtation` | `crush` | `lover` | `beloved` |

Tier numbers are persisted (`bond_tier` / `chem_tier` columns), so a SQL
consumer gets the authoritative tier without re-deriving it. Thresholds live
in exactly one function, `tier_index`; adding a tier means changing that
function plus a backfill.

Per-turn transitions are recorded in `label_changes` JSONB on the event
row: `{bond: {from, to}, chemistry: {from, to}}`, `NULL` when neither line
moved (format in [Event rows](#event-rows)).

## Relationship status

The two lines say how close a pair is, not what they are to each other. Two
things sit on top of them, and neither feeds back into the axes, the tiers or
the write pipeline:

- the **relationship status**: what the pair has agreed to be, stored per
  session and moved only by an answered ask, with a **cold war** that can sit
  on top of any status;
- the **composed relation**: a name for the pair's bond tier × chemistry tier,
  derived, never stored, and read by her prompt only.

### The statuses

Eight statuses, one per session. The Chinese wording is what the judge's
`[关系]` line reads for every status but `none`, which it never states; the
prompt's status line uses it too, except for the two gendered nouns
([`[relationship]`](#relationship)).

| Status | Wording | Breakable |
|---|---|---|
| `none` | 未确定 | no |
| `dating` | 伴侣 | yes |
| `partner` | 伙伴 | yes |
| `peer` | 同事 | no |
| `fwb` | 伙伴，也是情人 | yes |
| `situationship` | 同事，私下在交往 | yes |
| `engaged` | 已订婚 | yes |
| `ex` | 前任 | no |

Every session starts at `none` and never returns to it. `engaged` is the top
status. A *breakable* status is one a break-up can end.

### Transitions

Four asks move the status, and nothing else does: her `confess` and
`partner_up`, and `propose` and `break_up`, which she can ask and the user can
too ([life choices](life-events-needs.md#the-romantic-choices),
[the user's actions](api-reference.md#post-compchatsession_idmessagestream)).

| From | What happens | To |
|---|---|---|
| `none`, `ex` | `confess` accepted | `dating` |
| `partner` | `confess` accepted | `fwb` |
| `peer` | `confess` accepted | `situationship` |
| `none`, `ex`, `peer` | `partner_up` accepted | `partner` |
| `none`, `ex`, `peer` | `partner_up` declined | `peer` |
| `dating`, `fwb`, `situationship` | `propose` accepted | `engaged` |
| any breakable status | `break_up` accepted | `ex`, and the cold war ends |
| any breakable status | `break_up` refused | unchanged, and a cold war starts unless one is on |

A declined `confess` or `propose` keeps the status. Each ask is made only from
the statuses in its rows: she does not ask it from any other, and the user's
`propose` or `break_up` from any other is refused before the judge, with no
effect. A status already reached stays when the caller's scopes change.

The move is written in the transaction that settles the ask, under the affinity
row's lock: her ask's answer at the life-choice route, which `409`s and writes
nothing when the status has moved to one the kind is no longer asked from; the
user's action when its answer is stored, on the turn's session (on a date, the
session the invitation was sent in). Every change of status or cold war writes
one `relationship` event row ([Event rows](#event-rows)).

### The cold war

A cold war is a flag over the status (`cold_war_since`); ending it uncovers the
status underneath unchanged.

- **It starts** after a judged 1:1 text turn when, among the session's judged
  turns since the streak anchor, the newest three all carry the judge's lowest
  patience level, or at least five of the newest seven do; and when a break-up
  is refused. No tier gap, compatibility or status is read.
- **It ends** when the newest three judged turns since the anchor all carry a
  patience level above the lowest, and when a break-up is accepted.
- **The levels** are the judge's own (`patience_grade`, `1..=3`, from each
  judged turn's `state_after`; a turn whose snapshot lacks it reads as `2`),
  not the derived `patience`: that is the level times the bond line's boost, so
  with a low bond even the top level stays under `0.5`, and a threshold on it
  could hold a new relationship in a cold war for good.
- **The anchor** is the later of the session's newest life choice (its ask, its
  answer) and its newest `relationship` event, so every start and end of a cold
  war, and every status change, begins a fresh streak.
- **While it lasts** she keeps replying, coldly: `[relationship]` says so. Every
  positive committed delta on the four line axes is halved
  ([above](#life-choice-drops-and-the-cold-war-halving)). She asks nothing warm
  (`confess`, `partner_up`, `propose`, her sleep ask, `bathe_together`,
  `travel_together`), and from a breakable status she can ask to break up.
- **She says so** when a streak starts it, in one proactive message generated
  under the engine's default scopes, with no choice row,
  `metadata.proactive = 'cold_war'`. The flag is committed first: a
  failed or blank message leaves it on, and she says nothing. A cold war started
  by a refused break-up has no message of its own, and ending one writes none.

Only a 1:1 text session of a genome persona has one; a pending ask or a date
under way does not hold it back. The rules and the cue are in
[life-events-needs.md](life-events-needs.md#the-cold-war).

### BYOC sessions

A [BYOC](byoc.md) session stays outside the status: it is always `none`, with no
cold war. Her asks never fire there, the user's `propose` goes to the judge as
any action does and moves nothing, and the user's `break_up` is refused before
the judge with no effect.

### The composed relation

A name for the pair's two tiers, one per cell, `null` at `(1, 1)`. The names
are the actual prompt strings, never translated; the keys are what the API
serves:

| bond \ chemistry | 1 | 2 | 3 | 4 | 5 |
|---|---|---|---|---|---|
| **1** | none | 初识心动对象 `intriguing_stranger` | 让你上头的陌生人 `heady_stranger` | 露水情人 `fling` | 一见倾心的人 `love_at_first_sight` |
| **2** | 普通朋友 `casual_friend` | 暧昧对象 `budding_flame` | 迷恋对象 `friend_and_crush` | 热恋对象 `passionate_flame` | 热烈爱人 `ardent_beloved` |
| **3** | 亲近好友 `trusted_friend` | 友达以上 `more_than_friends` | 恋人未满 `almost_lovers` | 相依恋人 `tender_lover` | 难分难舍的人 `inseparable` |
| **4** | 知音 `kindred_spirit` | 心照不宣的知己 `knowing_confidant` | 暗生情愫的知己 `smitten_confidant` | 知心爱人 `heart_lover` | 挚爱 `beloved` |
| **5** | 莫逆之交 `lifelong_kindred_spirit` | 动了心的灵魂之交 `stirred_kindred` | 柏拉图式的爱人 `platonic_love` | 灵犀恋人 `destined_lover` | 命中注定的人 `destined` |

No name uses a status word (伴侣, 伙伴, 同事, 前任, 未婚). A composed relation
gates nothing: no event, life choice or action reads it.

- **In the prompt**, a pair whose turn `affinity_scope` is not
  romance-compatible takes its name from the chemistry-1 column (普通朋友,
  亲近好友, 知音, 莫逆之交; none at bond tier 1), whatever its chemistry.
- **In the API**, `composed_relation` is the key of the real tiers' cell; no
  compatibility is applied. A snapshot carries no scope, and the caller knows
  whether it marked the pair compatible.

### `[relationship]`

The text-chat prompt's `[relationship]` block says who the user is to her, in up
to three lines, each present only when it has something to say:

```
[relationship]
你是用户的未婚妻
论感情，你们现在是知心爱人
你们正在冷战
```

- **The status line**, when the status is not `none`: `你是用户的{noun}`.
  `dating` reads 女朋友 or 男朋友 and `engaged` 未婚妻 or 未婚夫, by the persona's
  `art_metadata.gender` (`female` / `male`; 伴侣 and 订婚对象 for any other
  value or none); every other status reads its wording above.
- **The composed line**, when there is a composed relation (for an incompatible
  pair, the chemistry-1 name): `论感情，你们现在是{name}` after a status line,
  and `你是用户的{name}` without one.
- **The cold-war line**, `你们正在冷战`, while it lasts.

With none of the three the block is the base pair's quadrant noun
([above](#the-base-pair--warmth--patience)), under the quadrant's scope gate.
The three lines are settled conclusions about the pair, not an axis steering
the reply, so no single axis of `affinity_scope` gates them; the block is
omitted only with no affinity row or an empty scope. A group speaker's prompt
carries the same block, read from that member's 1:1 session. The voice prompt
and the group director read no status: they keep their tier-label lines.

## What reads affinity

- `[relationship]` — the status line, the composed relation of the two tiers
  and the cold-war line, and the base-pair quadrant when none of the three has
  anything to say (see [`[relationship]`](#relationship)).
- `[mood]` — per-axis threshold gates: cold bans and warm unlocks.
- `[feelings]` — the LLM-written feeling clause stored on the affinity row
  (`feeling_clause`), rewritten on movement turns and on a refused action's tier drop.
- `[reply_length]` — three fixed ceilings selected by the scope composite
  `length_score`, thresholds `0.25` / `0.55`.
- Per-turn dice (`TurnNudges`) vetoes — the same cold floors as `[mood]`:
  `warmth ≤ 0.2` / `trust < 0.3` / `intrigue < 0.3`.
- PDE judge context — the intimacy rung (`1..=3`, an image gate over
  `max(bond, chemistry)`; rung 3 opens at `0.76`, inside tier 4 by design),
  the patience band, and a `[关系]` line, so its accept and refuse decisions
  agree with the relationship: `[关系] 状态={wording}；冷战中={是|否}`
  (`engaged` reads 已订婚). At `none` the line is `[关系] 冷战中=是` during a
  cold war and absent otherwise. It carries no composed relation. The judge's
  audit `inputs` record `relationship_status` and `cold_war`.
- The group director — the Bond and Chemistry labels and the intimacy rung of
  each member, read from the member's 1:1 session and never written.
- The life-event draw — the intimacy rung gates `ex_message`, `breakup` and
  `period_scare` (not drawn on rung 1), `sudden_desire` and `pregnancy_scare`
  (rung 3 only), and whether `busy`, `in_battle` and a female persona's
  `out_shopping` are announced (from rung 2); from rung 2 she also says she wants
  what the shopping finds (`saw_bag`, `saw_clothes`) put in front of her
  (see [the events route](api-reference.md#post-v2compsessionsession_idevents)).
- The cold war — the judge's patience levels of the newest judged turns start
  and end it, and ground her ask to break up
  ([the cold war](life-events-needs.md#the-cold-war)).
- The relationship and romantic choices — the status, the leading line and its
  tier, and the derived `warmth` and `patience` of the newest judged turns
  decide whether she confesses, offers a partnership, proposes, or asks to
  sleep together or fall asleep together; all but the partnership only for a
  pair the caller's scopes mark as romantic
  ([the choices](life-events-needs.md#the-romantic-choices)).
- The trip — the bond tier and the derived `warmth` and `patience` of the
  newest judged turns decide whether she suggests going away together
  ([going away together](life-events-needs.md#going-away-together)).
- The recall gate — the Bond and Chemistry tiers of the time-decayed row
  decide which intimate 基础画像 fields and domain-tagged profile memories
  reach the prompt, and how many memories each layer recalls; no row is tier 1
  on both lines. Text, group, voice and the image-edit text half all read it
  decayed ([memory-layers.md](memory-layers.md#affinity-gate)).
- Ghost scoring —
  `score = (1−intrigue)·0.4 + (1−patience)·0.4 + tension·0.2`, with hard
  vetoes (first 10 messages, streak ≥ 2, 1 h cooldown) and threshold `0.65`
  (`0.85` once the session has ghosted).
- The story director — the relationship status and the cold-war flag, beside
  the six axes and the two labels, as an engine-recorded fact that its
  `user_relationship` must not contradict
  ([world-system.md](world-system.md#the-story-round)).

## `affinity.rs` vs `scope.rs`

**What each is.** `crates/eros-engine-core/src/affinity.rs` is the model
itself: the state struct, the write pipeline (`grade_turn`), the base-pair
derivation, time decay, the Bond/Chemistry scores, tiers, labels.
`crates/eros-engine-core/src/scope.rs` is the per-request injection gate:
`AffinityScope` — six booleans, which axes may influence *this* request's
prompt — plus `MemoryScope`. It gates prompt injection and `length_score`,
and the `affinity_scope` a request records is what `romance_compatible` reads to decide
whether a pair is eligible for the romantic life choices (and, per turn, which
column of the [composed relation](#the-composed-relation) the prompt reads); post-process writes
(insight extraction, memory writes, the six-axis eval) are unaffected.

**What they share.** Both live in core. Both group the six axes into two
named halves. The scope's veto/gating decisions and the model's cold
directives read the same floors, so a vetoed axis and a cold axis tell one
story.

**Where they differ, deliberately:**

1. **Write vs read.** `affinity.rs` owns state and how it moves; `scope.rs`
   never writes anything. 3.1's write-side scope steering is retired in
   4.0: `affinity_scope` is read-side only. The endpoint derivation must
   never read the scope either: `B(x)` already transmits every line
   change to the endpoints, so a derivation that also read the scope would
   land the same request twice. (The crossed names below are the second
   reason.)
2. **The groupings differ.** `affinity.rs` (2.0+ lines): `bond =
   trust+intrigue`, `chemistry = intimacy+tension`, base pair outside both.
   `scope.rs` (1.0-era split): `AffinityScope::bond()` =
   `warmth+intimacy+tension` (朋友感), `AffinityScope::chemistry()` =
   `trust+intrigue+patience` (暧昧感). `length_score` averages each active
   triad by 3, and both halves when both are active.
3. **The scope's two names are crossed** relative to the 2.0+ lines: the
   triad *called* bond contains the Chemistry line's axes, and vice versa.
   Structurally the 1.0 split grouped each endpoint with the line that
   today amplifies it (warmth with intimacy/tension, patience with
   trust/intrigue: the same families 4.0's coupling makes explicit), but
   the names landed crosswise.
4. **This is a known, deliberately kept wart.** Renaming or regrouping the
   scope would change `length_score` inputs and regress reply lengths for
   existing callers. Default scope is `bond()`, the warmth/intimacy/tension
   triad. Do not "fix" it; do not let the scope steer the derivation or the
   write path.

## Persistence and API

### Generated columns

`bond` and `chemistry` are Postgres `GENERATED ALWAYS … STORED` columns over
the line axes:

```sql
bond      GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (trust    + intrigue) / 2))) STORED
chemistry GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (intimacy + tension)  / 2))) STORED
```

The DB recomputes on every row insert or update, so they cannot drift. The
formula is mirrored in code (`bond_score`/`chemistry_score`); keep the two
in sync.

### Endpoint levels

`warmth_grade`/`patience_grade` are `SMALLINT NOT NULL DEFAULT 2`, checked
`1..=3`: these are the authoritative judge levels. The
`warmth`/`patience` cache columns are backfilled with the level-2
derivation over each row's lines.

### Pending deltas

`pending_deltas` JSONB on `engine.companion_affinity` holds the line axes
only, as of 4.0; a stale `warmth` key from an older row is ignored and
drains naturally. `NULL` reads as all-zero.

### Relationship columns

Two columns on `engine.companion_affinity` (migration `0088`) carry the
[relationship status](#relationship-status):

- `relationship_status text NOT NULL DEFAULT 'none'`, checked against the eight
  keys;
- `cold_war_since timestamptz`, when the cold war began; `NULL` means none is
  on, and is the one carrier of the flag.

They are the authority: the status is never re-derived from life choices or
chat messages. Writing them leaves the row's `updated_at` alone, since that is
the time-decay clock.

### Event rows

Each delta turn appends one row to `engine.companion_affinity_events`:

- `deltas` — raw scores: grade conversion plus rule nudges, pre-decay; on an
  `activity` row, times its `weight`.
  `warmth`/`patience` are always `0.0` here, except on a `life_choice` row,
  whose `deltas` equal its `effective_deltas` ([above](#life-choice-drops-and-the-cold-war-halving)).
- `effective_deltas` — the applied change, `after − before`; for the base
  pair, the per-turn derivation delta.
- `context` — `affinity_reason`, `eval_skip_reason`, the verbatim signed
  `grades`, `pending_after`, and the endpoint audit: `warmth_grade`/
  `patience_grade` when read, `boost_warmth`/`boost_patience`,
  `decay_factor`, `units`. `cross_penalty_assessed` joins them whenever a
  turn was taxed; `tier_drop` (`bond` | `chemistry`) joins them on a turn
  whose refused action dropped that line; `growth_scale` (`0.5`) joins them
  on a turn whose positive growth was halved by the cold war. A life choice's
  own `life_choice` row has a `context` of `life_choice` and `choice_id`
  instead (see [above](#life-choice-drops-and-the-cold-war-halving)). A date's
  `activity` row adds `activity_id`, `turns`, `weight`, the judge's `reason`
  and, when the cap held a line, `capped` ([above](#dates)). A `relationship`
  row's `context` is `from` and `to` (status keys), `cold_war` (`entered` |
  `ended`) when the flag switched, and `choice_id` when her answered ask moved
  it (below).
- `user_message_id` — a real FK to `chat_messages`,
  `ON DELETE SET NULL`. `NULL` on `proactive`/`time_decay`/`life_choice`/`activity`
  rows and on pre-migration rows; never backfilled. A `relationship` row carries
  it when the user's own `propose` or `break_up` moved the status, and is `NULL`
  when her ask or a patience streak did. Assistant rows point at the same
  message through `chat_messages.user_message_id`, so the replies for an
  event are a join.
- `label_changes` — the engine-authoritative tier transition for the turn:

  ```
  label_changes = {
    bond:      { from: "<tier_key>", to: "<tier_key>" }  // if bond tier changed
    chemistry: { from: "<tier_key>", to: "<tier_key>" }  // if chemistry tier changed
  }
  // NULL when neither tier moved this turn
  ```

- `effective_line_deltas` — the exact per-turn bond/chemistry delta, served
  as `effective_deltas_computed`.
- `state_after` — the whole post-turn vector.
  `state_before` exists but is not served; replay is a direct query
  against `engine.companion_affinity_events`.

`event_type` is `message`, `ghost`, `gift`, `proactive`, `time_decay`,
`life_choice`, `activity` or `relationship`. The last four are not turns: the
per-turn delta route reads `message`, `gift`, `proactive` and `ghost` only.

A `relationship` row is written for every change of the status or the cold-war
flag, in the transaction that makes it, and only then: an answer that leaves
both as they were (a declined `partner_up` from `peer`) writes none. Its
`deltas` keep their default and it takes no state snapshot. It is not a judged
turn, but it moves the life choices' streak anchor.

### API surfaces

`GET /bff/v1/comp/affinity/{session_id}` and, per item,
`GET /bff/v1/comp/affinities/{user_id}` return an `AffinitySnapshot`,
refreshed at read time (`apply_time_decay` + `refresh_endpoints`):

```json
{
  "warmth": 0.52,
  "trust": 0.08,
  "intrigue": 0.12,
  "intimacy": 0.05,
  "patience": 0.27,
  "tension": 0.04,
  "bond": 0.10,
  "chemistry": 0.045,
  "bond_tier": 1,
  "chem_tier": 1,
  "bond_label": "acquaintance",
  "chemistry_label": "spark",
  "relationship_status": "none",
  "cold_war": false,
  "composed_relation": null,
  "ghost_streak": 0,
  "total_ghosts": 0,
  "updated_at": "2026-08-16T12:00:00.000000Z"
}
```

`bond`/`chemistry` are the real stored composites, `0..1`, no display curve.
`bond_tier`/`chem_tier` are `tier_index`'s own result; clients read one of
the two rather than re-deriving it from the score. `bond_label`/
`chemistry_label` are always one of the five tier keys for that line.
`relationship_status` is one of the eight status keys and `cold_war` whether a
cold war is on. `composed_relation` is the key of the real tiers' cell, or
`null` at `(1, 1)`; the pair's compatibility is not applied
([the composed relation](#the-composed-relation)). The life-choice answer and
the date's end route return the same shape.

`GET /bff/v1/comp/affinity/{session_id}/event` returns the per-turn delta:

```json
{
  "session_id": "…",
  "event": {
    "event_id": "…",
    "event_type": "message",
    "effective_deltas": {
      "warmth": 0.06, "trust": 0.02, "intrigue": 0.0,
      "intimacy": 0.0, "patience": 0.01, "tension": -0.02
    },
    "effective_deltas_computed": {
      "bond": 0.01,
      "chemistry": -0.01
    },
    "label_changes": {
      "bond": { "from": "acquaintance", "to": "friend" }
    },
    "state_after": {
      "warmth": 0.58, "trust": 0.21, "intrigue": 0.09,
      "intimacy": 0.04, "patience": 0.44, "tension": 0.02,
      "bond": 0.15, "chemistry": 0.03,
      "bond_tier": 2, "chem_tier": 1,
      "warmth_grade": 2, "patience_grade": 2,
      "ghost_streak": 0, "total_ghosts": 0,
      "updated_at": "…"
    },
    "created_at": "…"
  }
}
```

`effective_deltas_computed`, `label_changes`, and `state_after` are all
stored directly on the event row; each is `null`/absent on rows written
before its migration.

## Tuning knobs

Server-side env vars, each falling back per-knob to a default:

| Env var | Default | Meaning |
|---------|---------|---------|
| `AFFINITY_GRADE_UNIT_BOND` | `0.0786` | Raw score per grade step, trust/intrigue |
| `AFFINITY_GRADE_UNIT_CHEM` | `0.0266` | Raw score per grade step, intimacy/tension |
| `AFFINITY_NEG_FACTOR` | `1.5` | Extra multiplier on negative raw, keeping "slow up, fast down" |
| `AFFINITY_TIER_DECAY` | `1.0,0.70,0.45,0.25,0.10` | Positive-delta damping per tier 1–5 (comma-separated; anything but exactly 5 finite non-negative values keeps the whole default table) |
| `AFFINITY_CROSS_PENALTY_RATIO` | `0.8333` | κ_line = ratio × u_line, keeping the break-even unit-invariant |
| `AFFINITY_CROSS_PENALTY_START` | `0.35` | Counterpart score where the penalty ramp starts (y₀) |
| `AFFINITY_DELTA_THRESHOLD` | `0.0` | Commit threshold θ; `0` commits every turn |
| `AFFINITY_DEMO_BOOST` | `1.4` | Multiplier on the judge's positive raw for `metadata.is_demo` sessions |
| `AFFINITY_FLOOR_RATIO` | `0.2` | Endpoint floor φ; domain-capped at `0.24` so it can never override a non-cold verdict |
| `AFFINITY_TIME_DECAY_RATE` | `0.02` | Endpoint absence decay per day |
| `AFFINITY_TIME_DECAY_FLOOR` | `0.5` | Endpoint absence decay floor |
| `AFFINITY_ACTIVITY_TURN_SHARE` | `0.25` | A date's weight per substantive turn ([Dates](#dates)); `0` or more |
| `AFFINITY_ACTIVITY_TURN_CAP_SHORT` | `12` | Most turns that count on a `short` date; a non-negative integer, `0` = the date moves nothing |
| `AFFINITY_ACTIVITY_TURN_CAP_MID` | `32` | Most turns that count on a `mid` date; same domain |

Every scalar is domain-checked at boot: a non-finite or out-of-domain value
keeps its default and logs a warning, so an env typo degrades to defaults
instead of reaching the pipeline. The `0.35` pivot (= tier-2 upper bound)
and `B_MAX = 1.5` are code constants, not knobs, structural commitments an
env override could break.

## Source map

- `crates/eros-engine-core/src/affinity.rs` — types, `grade_turn` write
  pipeline, endpoint derivation, time decay, bond/chemistry scores, tiers,
  labels; a date's `grade_weighted`, `cap_line_shift` and
  `AffinityTuning::activity_weight`
- `crates/eros-engine-core/src/relationship.rs` — the statuses and their
  wording, `settle` (the transition table), `composed_relation`,
  `leading_line`, the cold-war streak rules, `sleep_ask_is_sex`
- `crates/eros-engine-core/src/scope.rs` — `AffinityScope` / `MemoryScope`,
  `length_score`, `romance_compatible` (see
  [`affinity.rs` vs `scope.rs`](#affinityrs-vs-scopers))
- `crates/eros-engine-store/src/affinity.rs` — `AffinityRepo`
  (persist_with_event, record_ghost, persist_life_choice_drop_in_tx,
  persist_activity_in_tx, last_judged_endpoints, write_relationship_in_tx,
  set_cold_war); its tables are in `migrations/0000_baseline.sql`, the
  `activity` event type in `migrations/0085_activities.sql`, and the
  relationship columns and event type in `migrations/0088_relationship_status.sql`
- `crates/eros-engine-server/src/pipeline/activity.rs` — a date's settlement
  and the activity judge call
- `crates/eros-engine-store/src/life_choices.rs` — `LifeChoiceRepo`
  (the choice rows, the streak anchor)
- `crates/eros-engine-server/src/pipeline/life_choice.rs` — the cold war's
  start and end, her asks, `growth_scale_for` (the halving)
- `crates/eros-engine-server/src/pipeline/post_process.rs` — LLM
  evaluation, grade/level parsing
- `crates/eros-engine-server/src/prompt.rs` — affinity → attitude directive
  + eval prompt; `relationship_ground` (the `[relationship]` block)
- `crates/eros-engine-server/src/pipeline/stream.rs` — `build_pde_ctx` (the
  judge's `[关系]` line); the status gate on the user's `propose` and `break_up`
- `crates/eros-engine-server/src/routes/dto.rs` — `AffinitySnapshot`
  (composite scores, labels, status, cold war, composed relation)
- `crates/eros-engine-server/src/routes/bff/affinity.rs` — BFF affinity
  surface (value + event)
- Design spec: `docs/superpowers/specs/2026-08-16-affinity-40-design.md` —
  the line math, endpoint derivation, tiers
- Design spec: `docs/superpowers/specs/2026-08-17-affinity-41-design.md` —
  stored tier columns, event state snapshots, the value endpoint
- Design spec: `docs/superpowers/specs/2026-10-05-chat-user-actions-design.md` —
  user actions, the judge's response, the tier drop
- Design spec: `docs/superpowers/specs/2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md` —
  the life-choice mechanism and the tier drop of an answer (its ask for space
  is retired)
- Design spec: `docs/superpowers/specs/2026-11-12-activity-sub-session-design.md` —
  a date as a sub-session, its weighted verdict and the one-tier cap
- Design spec: `docs/superpowers/specs/2026-11-23-affinity-50-design.md` —
  the relationship status, the cold war and its halving, the composed relation

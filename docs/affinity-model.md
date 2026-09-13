# Affinity model

[English](affinity-model.md) · [中文](affinity-model.zh.md) · [日本語](affinity-model.ja.md)

## Overview

Affinity is a six-axis vector, one per relationship (one per session). Every
axis is bounded to `[0, 1]` and clamped on every update. It moves only on
text-channel, non-`product_qa` chat turns; voice turns and `product_qa`
turns never write an affinity event.

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

- the `[relationship]` prompt section: a warmth × patience quadrant, each
  axis split at `0.5`. The four cells are the actual prompt strings, never
  translated:

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
Committed deltas apply 1:1 and clamp to `[0,1]`. The judge's levels, when
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
fallback is retired.

Ghost turns never reach `persist_with_event`: only `ghost_streak` /
`total_ghosts` / `last_ghost_at` move. `record_ghost` writes an all-zero
`effective_deltas`; no axis moves at all.

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

## What reads affinity

- `[relationship]` — the base-pair quadrant (see
  [the base pair](#the-base-pair--warmth--patience)).
- `[mood]` — per-axis threshold gates: cold bans and warm unlocks.
- `[feelings]` — the LLM-written feeling clause stored on the affinity row
  (`feeling_clause`), rewritten on movement turns.
- `[reply_length]` — three fixed ceilings selected by the scope composite
  `length_score`, thresholds `0.25` / `0.55`.
- Per-turn dice (`TurnNudges`) vetoes — the same cold floors as `[mood]`:
  `warmth ≤ 0.2` / `trust < 0.3` / `intrigue < 0.3`.
- PDE judge context — the intimacy rung (`1..=3`, an image gate over
  `max(bond, chemistry)`; rung 3 opens at `0.76`, inside tier 4 by design)
  and the patience band.
- Ghost scoring —
  `score = (1−intrigue)·0.4 + (1−patience)·0.4 + tension·0.2`, with hard
  vetoes (first 10 messages, streak ≥ 2, 1 h cooldown) and threshold `0.65`
  (`0.85` once the session has ghosted).

## `affinity.rs` vs `scope.rs`

**What each is.** `crates/eros-engine-core/src/affinity.rs` is the model
itself: the state struct, the write pipeline (`grade_turn`), the base-pair
derivation, time decay, the Bond/Chemistry scores, tiers, labels.
`crates/eros-engine-core/src/scope.rs` is the per-request injection gate:
`AffinityScope` — six booleans, which axes may influence *this* request's
prompt — plus `MemoryScope`. It gates prompt injection and `length_score`
only; post-process writes (insight extraction, memory writes, the six-axis
eval) are unaffected.

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

Migration `0048` redefines `bond` and `chemistry` as Postgres `GENERATED
ALWAYS … STORED` columns over the line axes (drop + re-add: Postgres cannot
alter a generation expression in place):

```sql
bond      GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (trust    + intrigue) / 2))) STORED
chemistry GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (intimacy + tension)  / 2))) STORED
```

The DB recomputes on every row insert or update, so they cannot drift. The
formula is mirrored in code (`bond_score`/`chemistry_score`); keep the two
in sync.

### Endpoint levels

`warmth_grade`/`patience_grade` are `SMALLINT NOT NULL DEFAULT 2`, checked
`1..=3` (migration `0048`): these are the authoritative judge levels. The
`warmth`/`patience` cache columns are backfilled with the level-2
derivation over each row's lines.

### Pending deltas

`pending_deltas` JSONB on `engine.companion_affinity` holds the line axes
only, as of 4.0; a stale `warmth` key from an older row is ignored and
drains naturally. `NULL` reads as all-zero.

### Event rows

Each delta turn appends one row to `engine.companion_affinity_events`:

- `deltas` — raw scores: grade conversion plus rule nudges, pre-decay.
  `warmth`/`patience` are always `0.0` here.
- `effective_deltas` — the applied change, `after − before`; for the base
  pair, the per-turn derivation delta.
- `context` — `affinity_reason`, `eval_skip_reason`, the verbatim signed
  `grades`, `pending_after`, and the endpoint audit: `warmth_grade`/
  `patience_grade` when read, `boost_warmth`/`boost_patience`,
  `decay_factor`, `units`. `cross_penalty_assessed` joins them whenever a
  turn was taxed.
- `user_message_id` (migration `0056`) — a real FK to `chat_messages`,
  `ON DELETE SET NULL`. `NULL` on `proactive`/`time_decay` rows and on
  pre-migration rows; never backfilled. Assistant rows point at the same
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
- `state_after` — the whole post-turn vector (migration `0049`).
  `state_before` exists but is not served; replay is a direct query
  against `engine.companion_affinity_events`.

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
  "ghost_streak": 0,
  "total_ghosts": 0,
  "updated_at": "2026-08-16T12:00:00.000000Z"
}
```

`bond`/`chemistry` are the real stored composites, `0..1`, no display curve.
`bond_tier`/`chem_tier` are `tier_index`'s own result; clients read one of
the two rather than re-deriving it from the score. `bond_label`/
`chemistry_label` are always one of the five tier keys for that line.

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

Every scalar is domain-checked at boot: a non-finite or out-of-domain value
keeps its default and logs a warning, so an env typo degrades to defaults
instead of reaching the pipeline. The `0.35` pivot (= tier-2 upper bound)
and `B_MAX = 1.5` are code constants, not knobs, structural commitments an
env override could break.

## Source map

- `crates/eros-engine-core/src/affinity.rs` — types, `grade_turn` write
  pipeline, endpoint derivation, time decay, bond/chemistry scores, tiers,
  labels
- `crates/eros-engine-core/src/scope.rs` — `AffinityScope` / `MemoryScope`,
  `length_score` (see [`affinity.rs` vs `scope.rs`](#affinityrs-vs-scopers))
- `crates/eros-engine-store/src/affinity.rs` — `AffinityRepo`
  (persist_with_event, record_ghost), migrations 0048–0049
- `crates/eros-engine-server/src/pipeline/post_process.rs` — LLM
  evaluation, grade/level parsing
- `crates/eros-engine-server/src/prompt.rs` — affinity → attitude directive
  + eval prompt
- `crates/eros-engine-server/src/routes/dto.rs` — `AffinitySnapshot`
  (composite scores + labels)
- `crates/eros-engine-server/src/routes/bff/affinity.rs` — BFF affinity
  surface (value + event)
- Design spec: `docs/superpowers/specs/2026-08-16-affinity-40-design.md` —
  the line math, endpoint derivation, tiers
- Design spec: `docs/superpowers/specs/2026-08-17-affinity-41-design.md` —
  stored tier columns, event state snapshots, the value endpoint

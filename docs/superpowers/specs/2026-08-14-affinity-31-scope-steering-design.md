# Affinity 3.1 — `affinity_scope` steers the write side — Design

- **Date:** 2026-08-14
- **Status:** Implemented
- **Type:** Engine change — write-side scoring, additive; no schema change, no
  API surface change
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` 1.2.1 (branched off `dev` @ 1.2.1-dev)
- **Extends:** [2026-08-13-affinity-30-grade-pipeline-design.md](2026-08-13-affinity-30-grade-pipeline-design.md).
  3.0 is unchanged and remains the behaviour under a neutral scope.

## 1. Motivation

3.0 models the bond/chemistry interlock as *symmetric*: the same tier decay and
the same cross-line penalty apply whatever the relationship is supposed to be.
But the product already knows something 3.0 ignores — whether the user and the
companion are a plausible romantic pair. `affinity_scope` carries that verdict
and, until now, only chose what to inject into the prompt.

Two things follow from letting it steer scoring as well:

1. **The asymmetry the field was added for finally reaches the score.** A pair
   with no romantic prospect should earn friendship faster; a pair with one
   should not get romance cheaply.
2. **A structural damper on evaluator generosity.** The affinity judge's prompt
   is engine-owned and its model rotates. "Generous about chemistry" is a
   failure mode of LLM judges as a class, not of one model. A ladder that cuts
   the bottom grade re-anchors the floor whatever the model calls it — a
   multiplier only scales whatever the model emits.

Measured on 213 turns from a reference deployment's first day on 3.0: chemistry's three
axes drew **2.06×** the grade mass of bond's (`tension` mean 1.249 and `+2` on
46% of turns; `trust` **0** on 84%). 91.5% of those turns carried a single
`affinity_scope` value, so the imbalance is global, not scope-conditional —
which is exactly why the correction is worth applying on the dominant value.

## 2. Steering

`ScopeMode` is a total function over the resolved six bools, so an axes array
steers as predictably as a named value:

| Resolved scope | Mode | Effect |
|---|---|---|
| empty (`none`) | `neutral` | 3.0 verbatim |
| any chemistry-half axis — `chemistry`, `full`, mixed arrays | `suppress_chemistry` | `intimacy` / `tension` positive grades pass the ladder |
| everything else — `bond`, bond-half arrays | `boost_bond` | `trust` / `intrigue` positive raw × the constant |

```
scope → ScopeMode → grade steering → [3.0: raw → decay → penalty → gate]
```

The field is **borrowed for the bond/chemistry mental model its named values
carry, not for the axis triads behind them**. Those triads (`scope.rs`, 1.0-era)
*partition* all six axes; the score composites (2.0+) *overlap* on warmth and
drop patience. Two different algebras, both correct for their own consumer —
`scope.rs` is deliberately not touched (see §5).

Three invariants:

- **Shared `warmth` is exempt from both directions.** It feeds both composites,
  so scaling it would leak the correction onto the other line — the same reason
  the cross-line penalty exempts it. Only line-exclusive axes steer.
- **Losses are never steered.** Negative raw keeps paying `neg_factor` and
  nothing else. The correction raises the bar; it does not soften damage.
- **A laddered-out grade charges no cross-line penalty.** Steering runs *before*
  the pipeline, so a grade mapped to 0 reads as "the judge did not touch this
  axis" and the penalty — charged only on touched axes — never fires.

## 3. Why a ladder for chemistry and a constant for bond

Break-even for one exclusive-axis push is `y* = y₀ + (1−y₀)·√(m·D_k·g)`, the
counterpart-line score at which the push nets zero. Reading the table for the
three candidates:

- **Ladder `0,0,1,3,4`** — its `g3`/`g4` rows are *identical* to 3.0's, and its
  `g2` row lands exactly on 3.0's `g1` row. It introduces **no break-even case
  3.0 did not already have**: milestones keep the wall geometry they had, only
  the two cheap grades get harder.
- **Constant ×0.5** — grows a row 3.0 never had. 3.0 guarantees a `g1` push at
  own-tier 1 can never be out-paid (`m·D·g ≥ 1`); ×0.5 turns that into
  `y* = 0.810`, so a fresh session starts losing ground once the counterpart
  passes 0.81. It also suppresses milestones *harder* than noise — the opposite
  of the intent.
- **Constant on bond** is the right shape for the other direction: boosting
  bond means "the same moment counts for more," which is a scale, not a floor.

## 4. Calibration

Replaying those 213 grade vectors (bootstrapped, 150 turns, 400 runs,
median) through the exact pipeline. `ratio` = chemistry ÷ bond at turn 150;
3.0 baseline sits at 2.36.

| Setting | ratio | wall |
|---|---|---|
| bond ×1.5 | 1.64 | none |
| chem ×0.7 | **2.43** | **bond wall @ turn 71** |
| chem ×0.5 | 1.34 | none |
| chem ladder, filter `g1` only | **2.42** | none |
| chem ladder `0,0,1,3,4` | 1.09 | none |

Two dead zones, both *worse than doing nothing*: the curve is **not monotonic in
strength**, so "start with a mild setting" is not available. The judge's
positive mass on the chemistry axes sits at `g2`, so any scheme that leaves `g2`
alone misses the centre of gravity.

The strength dial is **how many axes the ladder covers**, not how deep it cuts
(one axis lands at 1.36–1.58). Both axes is the setting: covering only
`tension` fits the current judge's particular skew and would leak under a model
that inflates `intimacy` instead — which forfeits the damper in §1.2.

Landing near parity is the intended outcome, not an overshoot: the `g2` mass
being cut is substantially explicit sexual requests that the judge reads as
flirtation. Not cutting it makes the product flatter the user rather than model
a relationship.

## 5. Non-goals

- **`core/scope.rs` is not touched.** Its `bond()`/`chemistry()` triads keep the
  1.0 grouping. They partition the six axes because `length_score`'s
  both-halves branch averages them — flipping them to the 2.0+ grouping would
  double-count warmth, drop patience out of the reply-length composite
  entirely, and change reply lengths product-wide. It would also break
  `voice.rs`'s half detection and `ScopeMode` itself (`bond()` would light the
  chemistry half, collapsing the mode to "everything but `none` suppresses").
  Settled in the 2.0 spec §4.1; 3.0 §7 inherits it; this spec inherits it again.
- **The judge rubric is untouched.** Two known biases — `trust` graded 0 on 84%
  of turns, and explicit requests scored as flirtation — are rubric work. The
  ladder compensates for the second on the write side; neither is fixed here.
  Note that `bond ×1.5` therefore rides almost entirely on `intrigue`: a
  multiplier cannot lift an axis the judge never moves.
- **No schema migration, no API surface change.** `openapi.json` is unchanged.

## 6. Audit

`companion_affinity_events.context` gains:

- `scope_mode` — every turn, always written;
- `effective_grades` — only when steering actually changed a grade, so its
  presence alone flags a corrected turn.

`grades` keeps meaning "the judge's verdict, verbatim". The pair is what keeps a
committed `0` attributable: the judge said nothing, or the ladder filtered it.
Without it, the drift-watching that `grades` exists for would read the engine's
own corrections as model movement.

## 7. Configuration and rollback

| Env var | Default | Identity (3.1 off) |
|---|---|---|
| `AFFINITY_SCOPE_BOND_BOOST` | `1.5` | `1.0` |
| `AFFINITY_SCOPE_CHEM_LADDER` | `0,0,1,3,4` | `0,1,2,3,4` |

The ladder parses like `AFFINITY_TIER_DECAY`: positional, exactly five values,
rejected whole on any malformed field so a typo cannot shift later grades. Slot
0 is additionally pinned to 0 — minting a delta from a grade of 0 would charge
rent on silence and make the axis pay a penalty it never earned.

Coverage note for whoever deploys this: the two paths are only as asymmetric as
the caller's scope distribution. A deployment that sends one value for most of
its traffic gets a fleet-wide retune wearing a per-scope switch — check which
values you actually send before rolling this out, and expect it to land on
everyone at once, since config ships with the image. Set both knobs to their
identities to revert without a code change.

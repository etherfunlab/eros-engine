# Cross-line penalty charged by grade, not by "touched at all" — Design

- **Date:** 2026-08-14
- **Status:** Implemented
- **Type:** Engine change — write-side scoring, additive; no schema change, no
  API surface change
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` 1.2.1
- **Amends:** [2026-08-13-affinity-30-grade-pipeline-design.md](2026-08-13-affinity-30-grade-pipeline-design.md)
  §2 "Cross penalty". Composes with
  [2026-08-14-affinity-31-scope-steering-design.md](2026-08-14-affinity-31-scope-steering-design.md).

## 1. Motivation

3.0 charged the cross-line penalty as a flat toll: any non-zero grade paid the
full `κ·φ(y)`, a grade of `0` paid nothing. The tax therefore did not depend on
how far the axis actually moved, which produces a specific pathology at high
line scores — **the judge returns an all-positive verdict and the meter goes
down.**

A production session made it concrete: eleven consecutive turns, thirty-nine
positive grades, zero negative, and ten of those eleven turns moved at least one
line downward. Both `warmth` and `intimacy` were pinned at `1.0`, so the only
penalty-exempt source of growth was exhausted and every remaining push was an
exclusive axis paying full toll.

The flat toll also makes the sign of the outcome depend on the grade: with
`ρ = D_k·g·u − κ·φ(y)`, gains grow linearly in `g` while the tax is constant, so
a small honest push loses ground where a larger one gains it. That is the
mechanism behind "the judge said up and the number fell", and it is not a
tuning accident — it is what a step function minus a constant does.

## 2. The change

```
penalty = κ · ((y − y₀)⁺ / (1 − y₀))² · (|g| / 4)
```

`g` is the grade **actually applied** — after any 3.1 scope steering, so the
ladder's "filtered to 0" case is now simply the zero endpoint of a continuous
rule rather than a separate branch. Magnitude, not sign: a negative grade moves
the axis *away* from the double-high position the penalty exists to discourage,
so it pays in proportion too rather than at the old flat rate.

Nothing else moves. `κ`, `y₀`, the quadratic ramp, warmth's exemption, tier
decay, the threshold gate and the 3.1 ladder are all unchanged.

## 3. Why this is the right shape

Ignoring rule nudges, the whole exclusive-axis term becomes

```
ρ = g · (D_k·u − κ·φ(y)/4)
```

The bracket does not contain `g`. Three consequences:

- **The sign of the outcome matches the sign of the verdict.** The grade sets
  magnitude, not direction. "Judge says up, score falls" is structurally gone
  wherever the bracket is positive.
- **The break-even is a property of the position, not of the grade.** It solves
  to `φ(y*) = 4·D_k·u/κ`, which at defaults exceeds 1 for tiers 1–4 — those tiers
  can never be out-taxed at any grade. Only own tier 5 has a real break-even, at
  `y* ≈ 0.761`.
- **The double-high lock survives exactly where it was aimed.** At own tier 5
  against a counterpart past `0.761` every grade nets negative, uniformly. "You
  cannot be both a confidant and a lover" still holds at the apex; it simply
  stopped firing on ordinary mid-relationship turns.

This is also a generalisation rather than a new concept: 3.1 already established
"a grade laddered to 0 charges nothing". Proportional charging extends the same
principle from binary to continuous.

## 4. Calibration

Replaying the 213 recorded grade vectors (bootstrapped, 150 turns, 400 runs,
median). The second block starts from the six-axis state of the production
session above rather than from zero — the high-position regime the earlier
bootstrap could not reach.

| Pipeline | from zero: bond / chem | from the high-position case: bond / chem |
|---|---|---|
| 3.0 (flat toll) | 0.356 / 0.839 | **0.278** / 0.890 |
| 3.1 ladder + flat toll | 0.625 / 0.673 | **0.304** / 0.895 |
| 3.1 ladder + proportional | 0.652 / 0.820 | **0.724** / 0.876 |

The bond line collapsing at high positions is caused by the **flat toll**, not
by the 3.1 ladder: the ladder alone barely moves it (0.278 → 0.304), while
proportional charging recovers it (→ 0.724). The judge touches the bond axes
rarely and at low grades, and under a flat toll a low grade at that position is
unconditionally a net loss — so the friendship line was being ground down by the
mechanism meant to keep the two lines balanced.

**This re-calibrates 3.1.** Chemistry was being suppressed partly by paying full
tax on halved gains; charging in proportion returns some of that (0.673 → 0.820
from zero), so the chem/bond ratio lands at 1.26 rather than 1.08. Still far from
3.0's 2.36, and now both lines rise instead of one being held down.

## 5. Audit

`companion_affinity_events.context` gains `cross_penalty_charged` — per
line-exclusive axis, written only when a turn actually paid. With the penalty
scaling by grade, how much was charged is no longer recoverable from the grades
and the stored scores, so it is recorded rather than reconstructed. warmth is
penalty-exempt and never appears.

## 6. Non-goals

- **No new knob.** `κ` remains the dial and `AFFINITY_CROSS_PENALTY_START` the
  second one; adding a flag to preserve a rule we have concluded is wrong would
  only double the behaviour space to test.
- **Tier decay stays a five-step table.** The staircase is a separate concern
  from the toll's shape; making it continuous is not required for the sign
  property above and is not attempted here.
- **No hysteresis at the tier endpoints.** A line crossing `0.90` back and forth
  still flips its label each time. Real, observed in the same session, and out
  of scope.

## 7. Compatibility

Purely a scoring change: no migration, no API surface change, `openapi.json`
regenerates byte-identical. Rolling back means pinning the previous image. Four
3.0 unit tests changed their expected values because the rule they pinned is the
rule being replaced; each now states the new arithmetic and what it was before.

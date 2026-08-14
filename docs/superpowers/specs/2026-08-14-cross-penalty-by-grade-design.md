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

More precisely, it factorises twice — the negative part is never decayed, so it
has a bracket of its own:

```
g > 0:  ρ = g · (D_k·u − κ·φ(y)/4)
g < 0:  ρ = g · (u·λ⁻ + κ·φ(y)/4)
```

Neither bracket contains `g`. Three consequences:

- **The outcome cannot change sign between grades at a fixed position.** The
  grade sets magnitude, not direction. This is the flat toll's failure mode
  removed — it is *not* a promise that every positive verdict is a gain, which
  remains a property of the position (below). The negative bracket is always
  positive, so a negative verdict always lowers the axis.
- **The break-even is a property of the position, not of the grade.** It solves
  to `φ(y*) = 4·D_k·u/κ`, which at defaults exceeds 1 for tiers 1–4 — those tiers
  can never be out-taxed at any grade. Only own tier 5 has a real break-even, at
  `y* ≈ 0.761`.
- **The double-high lock survives exactly where it was aimed.** At own tier 5
  against a counterpart past `0.761` every grade nets negative, uniformly. "You
  cannot be both a confidant and a lover" still holds at the apex; it simply
  stopped firing on ordinary mid-relationship turns.

Rule nudges sit outside the factorisation: they join the raw score before decay
but are not part of the penalty's grade, so a large enough opposing nudge could
in principle invert the sign. None can today — the only rule deltas reaching a
graded axis are `intrigue +0.02` and `tension +0.03`, both positive; every
negative nudge lands on `patience`, which bypasses the pipeline.

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

`companion_affinity_events.context` gains `cross_penalty_assessed` — per
line-exclusive axis, written only when non-zero. With the penalty scaling by
grade, the amount is no longer recoverable from the grades and the stored scores,
so it is recorded rather than reconstructed. warmth is penalty-exempt and never
appears.

**Assessed, not applied**, and the name says so: the penalty is subtracted
inside `ρ`, *before* the threshold gate, so on a gated turn nothing has reached
the axis yet — the amount rides in `pending_deltas` rather than being lost (the
gate re-times commits, it never rescales them), and the axis clamp can swallow
part of a later commit besides.

## 6. Non-goals

- **No new knob.** `κ` remains the dial and `AFFINITY_CROSS_PENALTY_START` the
  second one; adding a flag to preserve a rule we have concluded is wrong would
  only double the behaviour space to test.
- **Tier decay stays a five-step table.** The staircase is a separate concern
  from the toll's shape; making it continuous is not required for the sign
  property above and is not attempted here.
- **No hysteresis at the tier endpoints.** Evaluated against recorded
  `label_changes` and rejected — see below. A line crossing an endpoint back and
  forth still flips its label each time.

### Why not hysteresis

Label whiplash is real and current, not a legacy artifact: **34.3% of tier
transitions recorded under 3.0 are reversed** (12 of 35; the pre-3.0 corpus sits
at 22.7%, 108 of 475 — so it did not improve). Every endpoint is affected, led by
`beloved ↔ lover` and `flirtation ↔ spark`. Three findings still argue against a
deadband:

1. **The reversals are large real moves, not boundary jitter.** Under 3.0 the
   reversal-causing `|Δline|` has median `0.0285` and max `0.0603`; only 2 of 12
   fall under `0.01`. A deadband large enough to matter would need to be `≥0.03`
   — roughly a third of the entire tier-5 span (`0.10`). That is not hysteresis,
   it is redefining the ladder.
2. **The cause is the rule this spec replaces.** Of the 3.0-era down-legs, three
   of five landed on turns the judge scored *positive* (the other two were
   genuine negative verdicts; none were silent). Adding a deadband now would mask
   the symptom of a cause that has just been removed.
3. **Real hysteresis costs state.** One-step memory demonstrably does not hold —
   a score parked just below the endpoint flips on the following turn regardless
   — so it needs a persisted per-line tier. That means an additive migration,
   `tier_index` ceasing to be a pure function, and the label being allowed to
   disagree with the score: the two-bookkeeping-spaces problem 3.0 deleted along
   with `bar()`.

The measurement to repeat once this ships is cheap: `context` now carries
`grades`, `effective_grades`, `cross_penalty_assessed` and `label_changes`
together.

A separate boundary is worth stating: `label_changes` is a faithful record of
what the score did and should stay faithful. Whether a given transition deserves
a "you levelled up" notification — and whether that notification should be
debounced — is a consumer-side product decision, not a reason to change what the
engine scores.

## 7. Compatibility

Purely a scoring change: no migration, no API surface change, `openapi.json`
regenerates byte-identical. Rolling back means pinning the previous image. Four
3.0 unit tests changed their expected values because the rule they pinned is the
rule being replaced; each now states the new arithmetic and what it was before.

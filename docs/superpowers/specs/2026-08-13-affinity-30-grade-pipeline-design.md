# Affinity 3.0 — graded judge, tier decay, cross-line penalty, threshold gate — Design

- **Date:** 2026-08-13
- **Status:** Implemented
- **Type:** Engine change — write-side scoring pipeline rework + read-side
  projection removal, spanning core / store / server, two additive migrations
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` 1.2.0 (branched off `dev` @ 1.1.1-dev)
- **Supersedes:** the display-layer half of
  [2026-06-30-affinity-bond-chemistry-tiers-design.md](2026-06-30-affinity-bond-chemistry-tiers-design.md)
  (the `bar()` projection); tier endpoints, labels, rung and patience bands from
  that spec and [2026-07-09-affinity-fifth-tier-design.md](2026-07-09-affinity-fifth-tier-design.md)
  are unchanged
- **Resolves:** [#254](https://github.com/etherfunlab/eros-engine/issues/254)
  (decision rows now persist the affinity state the judge was shown)

## 1. Motivation

Affinity 2.0 (v0.7.0–v0.7.2) drew its "easy early, grind late" pacing with a
display curve: the five evaluated axes accumulate linearly, and `bar()`
compressed the top tiers only on screen. Three problems survived it:

1. **Growth was monotone and fast.** Linear accumulation × a judge model that
   skews generous × deliberately widened delta ranges = users maxed the lines
   in a couple dozen good turns. A display curve cannot slow the underlying
   number.
2. **Two bookkeeping spaces.** The store kept raw composites while the debug
   surface showed bar-space values; the BFF per-turn deltas were raw-space.
   Every consumer (PDE gating, display, tuning discussions) had to first answer
   "which space?".
3. **The judge was asked to do arithmetic.** LLMs are reliable ordinal raters
   and unreliable calibrated arithmetic. Asking for continuous deltas in
   [−0.6, +0.4] produced systematic inflation that a clamp can only truncate.
   The `patience` channel moved to 0.1-step buckets in July and has been stable
   since — the in-repo evidence for the bucket approach.

3.0 moves the nonlinearity from the display layer into the actual score
dynamics, and moves the judge from numbers to buckets. The projection is
deleted; what the API returns is what is stored.

## 2. The pipeline

One path, message events only (`affinity_evaluation`); ghost / time_decay /
gift / patience-absolute channels are untouched. Per evaluated axis
*a* ∈ {warmth, trust, intrigue, intimacy, tension}:

```
judge grade g ∈ {0..4} + direction        (patience: absolute 0~1, 0.1 steps)
   │
   ▼  conversion      r = σ·g·u          (σ<0: additionally ×λ⁻)  + rule nudges
   ▼  tier decay      positive part × D[tier(own line)]
   ▼  cross penalty   − κ·((y−y₀)⁺/(1−y₀))²   (judge-touched exclusive axes)
   ▼  real score ρ = D·max(r,0) + min(r,0) − P
   ▼  threshold gate  acc += ρ; |acc| ≥ θ → commit acc, else commit 0
   ▼  apply + clamp   (no EMA — deleted on this path)
```

- **Conversion.** `u = AFFINITY_GRADE_UNIT` (0.05), `λ⁻ = AFFINITY_NEG_FACTOR`
  (1.5). Endpoints reproduce 2.0's *effective* single-turn envelope exactly:
  grade +4 → +0.20, grade −4 → −0.30, the same caps the old ±0.4/−0.6 clamps
  yielded after EMA 0.5. The ceiling is unchanged; only the interior shape and
  the protocol changed. A 2.0 delta of +0.1 ≈ a 3.0 grade 1 — the buckets are
  the old scale discretised at 0.1 steps.
- **Tier decay.** `D = AFFINITY_TIER_DECAY` (1.0, 0.70, 0.45, 0.25, 0.10),
  indexed by the own line's tier on the *pre-turn* snapshot (one lookup per
  turn — no intra-turn order dependence). trust/intrigue read bond's tier,
  intimacy/tension read chemistry's, and shared warmth reads
  `max(tier_bond, tier_chem)` — the same max rule as the intimacy rung: deep
  relationships stop running on small talk; the exclusive axes must carry
  growth. Negative raw is never decayed — depth does not discount damage.
- **Cross penalty.** `κ = AFFINITY_CROSS_PENALTY` (0.05, exactly one grade
  unit), `y₀ = AFFINITY_CROSS_PENALTY_START` (0.35 = the tier-3 lower
  endpoint), `y` = the counterpart line's score. Charged only when the judge
  touched the axis (`g ≠ 0`) — a tax on events, not rent — and only on
  line-exclusive axes (warmth exempt, else a double-high pair would lock
  itself). A positive push can net negative; a negative push loses more. This
  is the friend-zone given a coordinate: a g2 push on an exclusive axis nets
  zero where `κ·φ(y) = D_k·2u`, i.e. counterpart break-evens y* ≈ 0.97 (own
  tier 3), 0.81 (tier 4), 0.64 (tier 5). Against a counterpart at 0.90 a g2
  exclusive push nets +0.009 at own tier 3 but −0.011 at tier 4, so a lagging
  line stalls at the tier-3/4 boundary (0.62) on ordinary clear moments —
  crossing it requires g ≥ 3.
- **Threshold gate.** Signed per-axis accumulator persisted in
  `companion_affinity.pending_deltas` (JSONB, migration 0043; NULL = zero).
  `θ = AFFINITY_DELTA_THRESHOLD`, default 0 = commit every turn. Conservation:
  the gate re-times commits, never rescales them (Σ committed = Σ ρ − pending,
  up to the axis clamps). Engine ships the mechanism; whether a deployment
  enables it is a downstream product decision.
- **Patience** rides through untouched: the rule delta passes 1:1 (no decay,
  penalty, or gating) so the eval-failure fallback still lands, and the
  absolute target overrides as before.
- **Demo boost.** `AFFINITY_DEMO_BOOST` (1.4) multiplies the judge's positive
  raw component on `is_demo` sessions (rule nudges unaffected) — the
  replacement for the retired `DEMO_EMA_INERTIA` (blend 0.7 vs 0.5 ≈ ×1.4).

**EMA removal.** `EMA_INERTIA` / `DEMO_EMA_INERTIA` are gone. EMA's damping
job is now the decay table's; its noise-smoothing job is done by grade
quantisation + the gate. Keeping it would make "grade × unit" a lie (a g4
nominally worth 0.2 would land 0.1).

### Difficulty economics

Defaults; "standard good turn" = warmth g1 + one exclusive axis g2 = +0.05
raw on a line; single line advancing alone with the counterpart below y₀, so
the cross penalty is zero and the table isolates the decay term.

| tier | span | D | net/turn | turns in tier | cumulative |
|---|---|---|---|---|---|
| 1 | 0.15 | 1.00 | 0.050 | 3 | 3 |
| 2 | 0.20 | 0.70 | 0.035 | 6 | 9 |
| 3 | 0.27 | 0.45 | 0.0225 | 12 | 21 |
| 4 | 0.28 | 0.25 | 0.0125 | 22 | 43 |
| 5 | 0.10 | 0.10 | 0.005 | 20 | 63 |

vs. 2.0's ≈ 20 turns to cap at the same judge generosity. Early game is
unchanged (D₁ = 1, penalty dormant below y₀); judge inflation is now damped
multiplicatively instead of clamp-truncated. No tier can be skipped in one
turn; the cap is reachable in finite turns (D₅ > 0). Balanced dual-line
growth: the standard-turn line delta `(0.15·D_k − κ·φ(S))/3` stays positive
through tier 4 (its zero, φ = 3·D₄, sits at S ≈ 0.91, outside the tier), so
both lines can reach 0.90/0.90 together — but inside dual tier 5 every
exclusive-axis push nets negative at ANY grade (even g4: 0.2·0.10 − κ·φ(0.9)
< 0) and only penalty-exempt warmth still lifts both lines, which caps at
1.0 while the exclusive axes cannot climb: dual 1.00 is asymptotic by design.

## 3. Judge protocol

Per axis the evaluator returns `{"grade": 0..4, "direction": "up"|"down"}`
(grade 0 = nothing happened — the overwhelmingly common verdict; rubric
anchors: 1 minor-but-real, 2 clear advance/harm, 3 rare significant moment,
4 milestone). `patience` stays an absolute 0~1 in 0.1 steps; `reason` stays a
one-line in-character sentence (all 2026-08-02 hygiene rules retained).

Parsing is strict where it must be and lenient where it can be: quoted
integer grades, missing direction (up) and missing grade (0) are salvaged;
a fractional / out-of-range grade or unknown direction rejects the whole
verdict — grades, the patience read and `reason` alike go down with it (the
engine refuses to trust any part of a malformed reply). Rule deltas persist
either way. Grades are folded to a signed integer −4..+4 at the parse
boundary (`AxisGrades`); every number downstream is engine-computed.

## 4. Read side

`bar()` is deleted. `AffinitySnapshot.bond/chemistry` return the stored
composites; the per-turn BFF deltas were already raw-space, so the two-space
mixing ends. Tier endpoints (0.15/0.35/0.62/0.90), both label ladders, the
legacy relationship label, the intimacy rung (0.76) and patience bands are all
unchanged — the endpoints simply gained a second job: indexing the decay
table. Existing users' displayed fill drops one-time (bar inflated the middle:
raw 0.35 displayed as 0.50); tier labels do not move.

## 5. PDE payload and decision audit (#254)

`build_pde_ctx` no longer hands the judge any affinity number: the
`[关系状态]` six-axis line and the `（bond=… chemistry=…）` parenthetical are
gone; the judge sees `[亲密度] 当前档位=第 N 档` and `[耐心] 当前档位=高/中/低`
only. In exchange, the decision row now freezes what the judge was shown:
`companion_decision_events.inputs` (JSONB, migration 0044, nullable,
fail-open) carries `{v, intimacy_rung, patience_band, bond, chemistry, axes}`.
`payload` remains "what the model returned"; `inputs` is "what the engine
supplied". Historical rows stay NULL — the values are not recoverable, which
is the point of #254. Net effect: the numbers moved out of the prompt and into
the audit row.

## 6. Observability

Three read-outs ship with the change, enough for the first tuning round with
no extra instrumentation:

- `companion_affinity_events.context.grades` — the judge's folded signed
  grades (distribution drift watch when swapping evaluator models);
- `companion_affinity_events.context.pending_after` + `deltas` (raw) vs
  `effective_deltas` (applied) — gate behaviour;
- `companion_decision_events.inputs` — gate-input replay for the image rung.

## 7. Non-goals

`scope.rs`'s 1.0-era groupings (`length_score`, injection scope), the
patience machinery, time_decay / ghost / gift paths, and the three-way
bond/chemistry definition cleanup are all untouched — separate tracks.

## 8. Compatibility

Both migrations are additive; an older engine reads the new schema unchanged
(unknown columns ignored), so rollback = pin the previous image. Config is a
breaking change in name only: the two EMA vars stop being read; the seven
`AFFINITY_*` vars all default sensibly when unset.

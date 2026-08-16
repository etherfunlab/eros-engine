# Affinity 4.0 — independent lines, derived endpoints — Design

- **Date:** 2026-08-16
- **Status:** Approved
- **Type:** Engine change — judge contract, write-side scoring, one schema
  migration (composite generation expression + two grade columns); axis set
  unchanged
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` 1.3.0
- **Supersedes:** [2026-08-14-affinity-31-scope-steering-design.md](2026-08-14-affinity-31-scope-steering-design.md)
  (write-side steering retired). Amends
  [2026-08-13-affinity-30-grade-pipeline-design.md](2026-08-13-affinity-30-grade-pipeline-design.md)
  (composites, warmth/patience channels). Keeps
  [2026-08-14-cross-penalty-by-grade-design.md](2026-08-14-cross-penalty-by-grade-design.md)
  (penalty-by-grade survives, with κ re-parameterised).

## 1. Motivation

Three defects, one root cause: the two relationship lines share an axis, and
the two per-turn axes ask an LLM for things LLMs cannot reliably produce.

1. **The shared term hides both lines.** `bond` and `chemistry` each take a
   third of `max(warmth, 0)`. On a reference deployment (477 judged turns,
   506 live affinity rows, 2026-08-15 snapshot) `warmth` was graded `+1` on
   78.8% of turns and reached `|grade| ≥ 2` on 3.6% — a near-constant that
   dilutes both composites and was also the only cross-penalty-exempt way up.
2. **`patience` asks the judge for a continuous reading.** Median 0.924
   (ceiling-packed), moving on 59.4% of turns with median step 0.100 and p90
   0.220. An LLM without reasoning or tools produces ordinals, not readings;
   a 0.1-step absolute scale is a reading with extra steps.
3. **`warmth` accumulates forever.** Graded-delta accumulation with a ±1
   clamp is a ratchet: 20 rows sat pinned at +1.0 with no mechanism to come
   back down.

4.0 fixes all three with one structural move: the lines stop sharing axes,
and the two per-turn axes become **derived quantities** — the judge grades a
coarse absolute level, and the engine folds it into a continuous value using
the line scores themselves.

## 2. Judge contract — fully ordinal

| Axis | Output | Change |
|---|---|---|
| `trust` / `intrigue` / `intimacy` / `tension` | grade 0–4 + direction (delta) | unchanged (3.0 contract) |
| `warmth` | **absolute level 1 / 2 / 3** | was graded delta; range becomes 0..1, negatives removed |
| `patience` | **absolute level 1 / 2 / 3** | was continuous 0..1 read + rule deltas |

Level semantics (rubric wording): **1** = cold / impatient — this turn was
visibly frosty, dismissive, or offended; **2** = baseline — the overwhelmingly
common verdict; **3** = clearly warm / invested.

JSON contract (warmth drops the `{grade, direction}` object form; both
endpoints are bare integers):

```json
{"warmth": 2, "trust": {"grade": 0, "direction": "up"}, "intrigue": {...},
 "intimacy": {...}, "tension": {...}, "patience": 2, "reason": "..."}
```

After 4.0 no LLM output anywhere in the affinity system is a continuous
number. Continuity is produced by engine arithmetic (§4), which is the 3.0
"grades in, arithmetic out" premise finally covering every output.

**Judge input:** the current `warmth` / `patience` values are **no longer
injected** into the judge prompt. An absolute-level verdict is valuable
precisely because it is stateless; injecting the previous value would anchor
the judge and reproduce the inflation path. The four line axes keep their
banded injection unchanged.

**Skipped turns** (`eval_skip_reason`, 15.1% of turns): hold the last stored
level; the real value is still recomputed with current line scores and decay.
The `patience` rule-delta fallback retires with the rule deltas (§8).

## 3. Composites — the lines stop sharing

```
bond      = clamp01( (trust    + intrigue) / 2 )     -- was (max(warmth,0)+trust+intrigue)/3
chemistry = clamp01( (intimacy + tension ) / 2 )     -- was (max(warmth,0)+intimacy+tension)/3
```

`bond` is friendship — trust plus continued interest. `chemistry` is romance —
closeness plus charge. The mental model users already hold; the write side
catches up. Tier boundaries, both label sets, the legacy label and
`INTIMACY_RUNG3_LO` do not move.

**No data migration.** Axis columns keep the values the judge produced; the
composites are redefined and recomputed. On the reference snapshot this moves
labels on 123 of 506 rows (74 demotions, 56 promotions), median 2 turns to
undo, worst case 11; no top-tier bond label is lost. Accepted — repairing it
would mean permanently injecting score the judge never granted, and the four
axes are read raw by tone directives and reply-length scoring, so the
injection would leak into visible behaviour.

## 4. Endpoint derivation — warmth and patience become outputs

```
base(g)   = (g − 1) / 3                          -- {0, 1/3, 2/3}
B(x)      = 1 + λ·(x − PIVOT)                    -- λ = (B_MAX − 1)/(1 − PIVOT) = 10/13
decay(Δt) = max(FLOOR, 1 − RATE·days)            -- Δt since updated_at

warmth    = clamp01( max( base(g_w)·B(chem), φ·chem ) · decay )
patience  = clamp01( max( base(g_p)·B(bond), φ·bond ) · decay )

Δwarmth   = warmth_t − warmth_{t−1}              -- deltas still computed and
Δpatience = patience_t − patience_{t−1}          -- emitted every turn
```

Constants and their anchors — no new magic numbers:

- **`PIVOT = 0.35 = TIER2_HI`** (referenced, not redefined). The boost turns
  positive the moment the counterpart line enters tier 3; below it the real
  value is damped under the base. 0.35/0.65 are also the existing
  `patience_band` and judge-input band cuts, so production and consumption
  share one scale.
- **`B_MAX = 1.5`**, giving `⅔ × 1.5 = 1.0`: a full judge level times a full
  counterpart line lands exactly at the ceiling. `clamp01` is float
  insurance, not mechanism. Key values:
  `B(0)=0.731 · B(0.15)=0.846 · B(0.35)=1 · B(0.62)=1.208 · B(0.90)=1.423 · B(1)=1.5`.
- **`φ = 0.2` (floor ratio, ratified 2026-08-16).** Level 1 becomes a
  relationship-scaled ember `φ·x ∈ (0, 0.2]` instead of an absolute zero: a
  deep relationship going cold one turn reads 0.18, a stranger reads ~0.
  `φ·x ≤ 0.2 < 0.244 = ⅓·B(0)` guarantees the floor only ever acts on
  level 1 — it can never overwrite a non-cold verdict. This closes the
  `(0, 0.244)` reachability hole down to a 0.044 sliver that sits entirely
  inside the low band (engineering continuity: few breakpoints, no visible
  cliff — not mathematical density). Two alternatives were evaluated and
  rejected: raising the base ladder (shrinks the cliff, keeps the hole) and
  asymmetric fall smoothing (deferred; re-evaluate on one week of level
  distribution data before adding a second mechanism).
- **`RATE = 0.02/day`, `FLOOR = 0.5`**: linear, matching the engine's
  existing per-day drift style. 7 days → 0.86, 25+ days → 0.5. Absence
  cools, but never to zero — and an old relationship keeps a floor through
  the boost: bond 0.92 at full decay still yields patience 0.48 (inside the
  mid band), a stranger 0.24.

Reachable values at `decay = 1`: level 1 → `(0, 0.2]` (continuous in x),
level 2 → `[0.244, 0.500]`, level 3 → `[0.487, 1.0]`. The level picks the
band, the counterpart line picks the position inside it; levels 2/3 overlap
by only 1.3%, so a judge demotion is never recoverable by boost alone.

**Delta reporting:** snapshot after decay (same convention as
`effective_deltas` today), so the delta attributes the turn's judgment and
coupling change, never the absence gap.

## 5. Coupling semantics

`chem` boosts `warmth`; `bond` boosts `patience`. The semantics is
**amplification**, not correlation: deeper chemistry warms expression, deeper
bond funds patience. This yields a complete quadrant vocabulary with no
prompt special-casing:

| | chem low | chem high |
|---|---|---|
| **bond high** | patient but cool — the old friend | both high |
| **bond low** | stranger (default start) | impatient but warm — the tsundere |

The pairing is the 1.0 structural inheritance: `scope.rs`'s original triads
already grouped `warmth` with `intimacy`/`tension` and `patience` with
`trust`/`intrigue`. 4.0 upgrades that implicit grouping to an explicit
formula. Only the *names* crossed: 1.0 called the warmth-family triad
"bond()" and the patience-family triad "chemistry()"; 4.0 keeps the 2.0 line
names (already in generated columns, labels, and docs). `scope.rs`'s triads
keep serving injection gating and `length_score` — the two algebras remain
deliberately distinct.

## 6. AffinityScope stays out of the derivation

Ratified: `affinity_scope` does not touch the endpoint derivation, ever.

1. **Double-boosting is structural, not hypothetical.** B(x) exists to
   transmit every line change — including scope-steered pace — to the
   endpoints: `BoostBond` speeds trust/intrigue → bond rises → `B(bond)`
   lifts patience. The scope signal *already arrives*. A derivation layer
   that also read scope would land the same request on the same endpoint
   twice. Trigger surface is 100%: on the reference deployment 161/164 turns
   resolved to `SuppressChemistry` (every request sends `full`), and
   `AffinityScope::default()` is `bond()`, so even scope-less requests steer.
2. **Array scopes are ambiguous.** `affinity_scope` accepts arrays; there is
   no non-arbitrary rule assigning an array to one line's half.
3. **Wrong semantics.** B(x) is a relationship-depth amplifier; scope is a
   request-side attention switch. They must not share an entry point.

With the 3.1 ladder retired (§8) this completes the retreat of
`AffinityScope` from the write side; it keeps its read-side injection role
only.

## 7. Pacing — per-line units, κ tied to the unit

Inherited from the 4.0 composite analysis, restated here as the operative
config:

```
AFFINITY_GRADE_UNIT_BOND = 0.0786
AFFINITY_GRADE_UNIT_CHEM = 0.0266
κ_line = AFFINITY_CROSS_PENALTY_RATIO · u_line     -- ratio default 5/6 ≈ 0.8333
```

The per-line units reproduce the shipped pace (tier-5 in ~99/98 turns) after
the shared term and the 3.1 ladder are both gone; the 2.96× spread is the
judge's measured grading asymmetry written down where it can be argued with.
Tying κ to the unit makes the cross-line break-even independent of the unit —
without it, dropping `u_chem` to 0.0266 would freeze chemistry at tier 3
against a high bond. Both units were derived on a 1.79-day window;
**re-derive after one week of 4.0 data**, and again if the beta grade unit
override changes.

## 8. Retirements

- **3.1 chemistry ladder** (`ScopeMode` grade remap): fired on 98.2% of turns
  regardless of intent, collapsed g1/g2 into one event, purpose superseded by
  penalty-by-grade. `effective_grades` stops being emitted — its absence is
  the cleanest verification the retirement landed.
- **`patience` rule deltas** (long/short message ±0.02, stale >24h −0.05):
  noise-level; the stale rule is absorbed by `decay`. Ghost handling already
  bypasses patience and is unchanged.
- **`patience` +0.005/day upward drift**: inverted by design. Absence should
  cool; the old-friend resilience is now carried by `B(bond)` (§4), not by a
  drift patch.
- **`warmth` negatives and the ±1 clamp**: range is 0..1; the accumulator,
  tier-decay damping, threshold gate and cross-penalty exemption for warmth
  all become dead paths and are removed with it.
- **v2's EMA draft** (`AFFINITY_WARMTH_INERTIA`): never shipped, dead on
  arrival.

After this, `grade_turn` and the whole accumulation pipeline (tier decay,
cross penalty, threshold gate) govern exactly four axes.

## 9. Storage and migration

Authoritative facts per row: the four line axes, the two judge levels, and
`updated_at`. One migration:

- `companion_affinity` gains `warmth_grade smallint NOT NULL DEFAULT 2` and
  `patience_grade smallint NOT NULL DEFAULT 2` (backfill default 2 =
  baseline; existing warmth/patience values are not consulted — they no
  longer feed anything).
- `bond_score` / `chemistry_score` generated columns are dropped and
  re-added with the `/2` expression (Postgres cannot alter a generation
  expression in place).
- `warmth` / `patience` columns remain as a **materialized cache** of the
  derived values, refreshed at the same three call sites where `time_decay`
  applies today (row-locked persist, prompt-injection read, debug snapshot).
  The derivation is a pure function of stored facts, so every value stays
  reproducible from the event log.

New-row behaviour: defaults level 2/2 with bond = chem = 0 give
warmth = patience ≈ 0.244 (floor contributes nothing at x = 0). Patience at
conversation start drops from 0.5 to 0.244 — a deliberate change (strangers
have limited patience; the 1.0 spirit). The knob for softening this, if ever
wanted, is the default level, not φ.

Accepted consequences (ratified 2026-08-16): the label churn of §3, and ten
shallow rows (3–10 lifetime turns) dropping an image-gate rung
(`intimacy_rung` 2→1, fifteen rows gaining one) — the gate was independently
found too loose, so this is a correction, not a regression.

## 10. Engine touch points

- `eros-engine-core/src/affinity.rs` — composites to `/2`; warmth removed
  from `grade_turn` / `AxisGrades` (four axes, by construction); endpoint
  derivation (base / B / floor / decay) as pure functions; per-line units;
  κ ratio.
- `eros-engine-core/src/scope.rs` — **one line**: `length_score`'s
  `warm01 = (warmth + 1)/2` becomes `warmth` (range change; the shift would
  otherwise inflate the bond half by 0.25). Triads untouched.
- `eros-engine-server/src/prompt.rs` — judge system prompt: warmth/patience
  level rubric, endpoint injection removed, JSON contract update.
- `eros-engine-server/src/pipeline/post_process.rs` — parse bare-integer
  levels (lenient like patience today: quoted integers salvaged, NaN/inf
  refused, malformed verdict voids the whole eval); skip-turn hold; wire the
  derivation.
- `eros-engine-store/src/affinity.rs` — persist levels + refresh cache;
  event context fields (§11); ladder fields removed.
- `eros-engine-server/src/state.rs` + `.env.example` — knobs:
  `AFFINITY_FLOOR_RATIO` (0.2), `AFFINITY_TIME_DECAY_RATE` (0.02),
  `AFFINITY_TIME_DECAY_FLOOR` (0.5), `AFFINITY_GRADE_UNIT_BOND`,
  `AFFINITY_GRADE_UNIT_CHEM`, `AFFINITY_CROSS_PENALTY_RATIO` (5/6).
  `PIVOT` and `B_MAX` are **code constants** (ratified): they are structural
  commitments — env-tuning them could break the exact-ceiling property.
  Out-of-range values fall back to defaults with a warning, as today.

## 11. Observability

Per turn, on `companion_affinity_events.context`:

- `warmth_grade` / `patience_grade` — the judge's raw levels (rubric
  distribution is unauditable without them).
- `boost_warmth` / `boost_patience` — the B values in force that turn.
- `decay_factor` — that turn's decay.
- `units` — the per-line units in force, `{"bond": u, "chem": u}`
  (window-boundary safety).
- The endpoint deltas are **not** duplicated into `context`: the existing
  `effective_deltas` event column already carries `warmth`/`patience`
  after−before under the post-decay snapshot convention, and one fact gets
  one carrier.

First-week dashboard: level distribution for g_w/g_p (level-1 share decides
whether the deferred smoothing is needed; level-2 share validates the
rubric's "overwhelmingly common" claim); warmth band-landing distribution vs
today; whether patience ceiling-packing (median 0.924) clears; floor-hit
share, which must equal the level-1 share — any divergence is a bug.

## 12. Non-goals

- **The trust ceiling** (bond hard-capped at 0.5 while the judge grades
  trust 0 on 80.7% of turns): rubric fix queued as its **own release** so
  results stay attributable.
- **Endpoint hysteresis** at tier cuts: pre-existing, explicitly out of
  scope here as it was for penalty-by-grade.
- **Asymmetric fall smoothing** (deferred plan C): revisit on week-one data.
- Anything downstream: clients render the new 0..1 warmth range; this is a
  breaking API-shape note for the release notes, not an engine concern.

## 13. Testing

- Unit: derivation pure functions (base/B/floor/decay identities: exact
  ceiling, floor-only-on-level-1, level-2/3 overlap bounds); composite
  regeneration; κ/unit independence of the break-even.
- Regression locks: existing four-axis pipeline tests updated for the
  narrowed `AxisGrades`; store test asserting Rust/SQL composite parity
  updated to `/2`; new lock: `effective_grades` absent from new events.
- Migration: on a seeded fixture, assert the `/2` redefinition recomputes
  labels and rungs correctly across the tier boundaries (including a
  demotion, a promotion, and an unchanged row); the §3/§9 production counts
  are evidence for the decision, not test expectations.
- Judge contract: parse tests for bare-integer levels including salvage and
  refusal paths.

## 14. Rollout

Single PR, atomic (ratified): composites, derivation, units, ladder
retirement and κ ratio ship together — a split would leave warmth neither
feeding lines nor derived, a state that would need its own transitional
code. Rollback = revert image + re-run the down migration (generation
expression back to `/3`; grade columns are additive and inert to old code).
Constants re-derived after one week of production data.

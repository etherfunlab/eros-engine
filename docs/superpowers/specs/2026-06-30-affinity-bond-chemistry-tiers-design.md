# Affinity: Bond / Chemistry lines, tiered labels, and a sparser eval distribution

**Date:** 2026-06-30
**Status:** Design (approved for spec review)
**Related:** issue #130 (deferred `filter_prompt` override for `affinity_evaluation`),
`docs/superpowers/specs/2026-05-20-affinity-event-delta-design.md` (BFF event surface)

## 1. Motivation

The companion's relationship state is computed from a 6-axis affinity vector but
surfaced to users through two folded bars — **Bond** (friendship) and
**Chemistry** (romance). Three problems today:

1. **Labels are too coarse and tangled.** `Affinity::infer_label` produces only 5
   states (`stranger / romantic / friend / frenemy / slow_burn`) from ad-hoc
   multi-axis threshold conditions. It does not map cleanly onto the two bars.
2. **The bars climb too fast, with no sense of achievement.** The 6→2 folding was
   decided off-the-cuff and lives in the frontend; each bar rises quickly and
   linearly, so reaching "100%" feels unearned.
3. **The eval scoring band is too narrow and symmetric.** The evaluator emits
   tiny per-axis deltas (≈±0.15 raw, EMA-compressed to ≈±0.03/turn). Gains are
   easy and small, losses rare and slow. There is no "rare-but-big" gain and no
   readily-firing loss.

This change makes the engine the single source of truth for the two lines: it
computes Bond/Chemistry, derives a tiered label per line, exposes them, and
reshapes the eval distribution — **without changing the 6-axis base.**

## 2. Goals / non-goals

**Goals**

- Two derived lines, **Bond** and **Chemistry**, each with **4 tiered labels**
  (8 names) + a shared `stranger` → the "9 relationship states".
- Each line's label is a **pure monotonic function of that line's value**
  (kills the `infer_label` heuristic).
- An **exponential-feel climb**: early tiers easy, top tiers a grind.
- Engine **owns and exposes** Bond/Chemistry (value + label); the frontend only
  renders.
- Engine is the **single authority for per-turn label transitions** — computed
  deterministically from the LLM's 6-axis deltas, stored on the event row, and
  served to the frontend (which stops computing them itself).
- A **sparser, asymmetric eval distribution**: most turns score 0; positive
  moments are rare but can be large; negative moments fire more readily and can
  be larger.

**Non-goals (explicitly out of scope)**

- The 6-axis representation, EMA smoothing (`apply_deltas`), and time decay
  (`apply_time_decay`) are **unchanged**. (The new-row *default seed* values are
  lowered — §4.8 — but the axis mechanics and existing rows are untouched.)
- `AffinityScope` / `length_score` (prompt-injection scoping, issue #40) is
  **untouched** — even though its `bond()`/`chemistry()` axis-sets use a
  *different* grouping (see §4.1 naming note).
- The chat companion prompt is unchanged.
- The `filter_prompt` override for `affinity_evaluation` is **deferred** (#130);
  the evaluator prompt stays a single in-code string — only its scoring guidance
  changes (§4.5).
- Frontend rendering (bar widths, colors, copy) is the frontend's concern.

## 3. Current state (reference)

- 6 axes on `Affinity` (`crates/eros-engine-core/src/affinity.rs`): `warmth`
  (−1..1), `trust`/`intrigue`/`intimacy`/`patience`/`tension` (0..1).
- `apply_deltas(d, ema_inertia)`: `blend = 1 − ema_inertia`; prod
  `EMA_INERTIA=0.8` → blend 0.2.
- `infer_label` → 5-state heuristic, written on every persist via the store
  (`crates/eros-engine-store/src/affinity.rs` `persist_with_event`).
- `parse_affinity_eval` (`pipeline/post_process.rs`) clamps each emitted axis to
  `±LLM_AXIS_CAP` (0.15).
- Surfaced only via `AffinitySnapshot` (`routes/dto.rs`) — used by
  `/comp/affinity/{sid}` (debug) and `/bff/v1/comp/chat/start` — and the BFF
  per-turn delta `/bff/v1/comp/affinity/{sid}/event`
  (`routes/bff/affinity.rs`). The 2-axis (Bond/Chemistry) folding currently
  lives in the frontend.

## 4. Design

### 4.1 Folding (core)

Add two pure methods on `Affinity` (core). With `warm_pos = warmth.max(0.0)`
(floored at 0 — **not** `(warmth+1)/2`; see §4.8):

```
bond_score      = (warm_pos + trust   + intrigue) / 3    ∈ [0, 1]   // 友情
chemistry_score = (warm_pos + intimacy + tension) / 3    ∈ [0, 1]   // 爱情
```

- **`warmth` is shared** into both lines (it underlies both friendship and
  romance; cold replies tank both). Floored at 0, so a neutral/cold session
  contributes nothing — a fresh session sits near 0, not at a 0.5 baseline.
- **`patience` is excluded** from both — it is rule-owned (maintained by
  decay/rules, never scored by the evaluator) and stays an internal pacing axis.

> **Naming note (known wart, intentionally not fixed here):**
> `AffinityScope::bond()/chemistry()` (used for prompt-injection scoping and
> `length_score`) use a *different* grouping (`bond = warmth+intimacy+tension`,
> `chemistry = trust+intrigue+patience`). That is the older off-the-cuff split.
> We do **not** touch it to avoid reply-length regressions. The new
> `bond_score`/`chemistry_score` are independent; document the distinction in
> code comments. A future cleanup may unify them.

### 4.2 Tiers + bar curve (core)

Four tiers per line, with **widening raw-score gaps** (each step costs more):

| tier | raw range      | gap   |
|------|----------------|-------|
| 1    | `[0.00, 0.15)` | 0.15  |
| 2    | `[0.15, 0.35)` | 0.20  |
| 3    | `[0.35, 0.62)` | 0.27  |
| 4    | `[0.62, 1.00]` | 0.38  |

**Bar value (0..1, what the frontend renders):** each tier maps to an even **25%
band**, linear within:

```
bar(raw) = band_lo(tier) + (raw − tier_lo) / (tier_hi − tier_lo) * 0.25
  T1: 0.00 + (raw − 0.00)/0.15 * 0.25   → [0.00, 0.25)
  T2: 0.25 + (raw − 0.15)/0.20 * 0.25   → [0.25, 0.50)
  T3: 0.50 + (raw − 0.35)/0.27 * 0.25   → [0.50, 0.75)
  T4: 0.75 + (raw − 0.62)/0.38 * 0.25   → [0.75, 1.00]
clamp to [0, 1]
```

Because higher tiers span more raw affinity, the bar fills fast early and crawls
near 100% → the desired "前两级简单、后两级难" without a literal `exp()`. A fixed
per-turn raw delta also moves the bar **less** in higher tiers.

All thresholds and bands are **tunable constants** — flagged for review.

### 4.3 Tiered labels (core)

Two enums (serialized snake_case keys; Chinese display is a frontend concern,
suggestions given for reference):

| line          | tier 1                 | tier 2              | tier 3                  | tier 4            |
|---------------|------------------------|---------------------|-------------------------|-------------------|
| **Bond**      | `acquaintance` 点头之交 | `friend` 朋友        | `close_friend` 好友      | `confidant` 知己   |
| **Chemistry** | `spark` 来电            | `flirtation` 暧昧    | `crush` 心动             | `lover` 恋人       |

`bond_label()` / `chemistry_label()` return the tier of the respective score.
These two fields are **always one of their 4 values** (never `stranger`);
`stranger` is conveyed only by the legacy field (§4.4).

### 4.4 Legacy `relationship_label` (back-compat)

The legacy column + DTO field **keep the old name set**
(`stranger / friend / slow_burn / romantic`; `frenemy` retired from emission but
kept parseable in the enum). It is now a **pure function of the two raw scores**
(replacing `infer_label`):

```
legacy_relationship_label(bond, chem):
  if tier(bond) == 1 && tier(chem) == 1            -> stranger
  let higher = (chem > bond) ? Chemistry : Bond     // tie -> Bond
  match higher:
    Bond                                            -> friend
    Chemistry if tier(chem) in {1,2}                -> slow_burn
    Chemistry if tier(chem) in {3,4}                -> romantic
```

The store writes this value to `companion_affinity.relationship_label` (so DB
consumers stay valid); the DTO serves it. Mapping table is **tunable** — flagged
for review.

### 4.5 Eval distribution + asymmetric cap (post_process + prompt)

**Cap.** Replace the symmetric `LLM_AXIS_CAP = 0.15` with an asymmetric clamp in
`parse_affinity_eval`:

```
POS_CAP = +0.4   NEG_CAP = -0.6
delta_axis = raw.clamp(NEG_CAP, POS_CAP)
```

With EMA blend 0.2 this yields per-turn maxima of **+0.08 / −0.12** on a single
axis (vs. ±0.03 today). `patience` stays forced to 0 (rule-owned), unchanged.

**Prompt (scoring guidance only).** Rewrite the magnitude guidance in
`prompt::affinity_eval_prompt`. The structure (six current values shown, JSON
output schema for warmth/trust/intrigue/intimacy/tension + `reason`) is
**unchanged**; only the guidance sentence changes to express the new
distribution:

- Ordinary chitchat / acknowledgements → **exactly `0`** (not "≈0").
- Positive deltas only on a genuine relationship-advancing moment (real warmth,
  self-disclosure, vulnerability, flirtation that lands); these are **rare but
  may be large** (up to ≈ +0.4 per axis).
- Negative deltas should fire **readily** for coldness, perfunctory/repetitive
  replies, boredom, boundary-crossing, conflict, or being ignored; these may be
  **larger** (down to ≈ −0.6 per axis).

Net effect: gains are hard (most turns 0) but a real moment can jump a bar;
losses are more frequent and bite harder — all via the prompt + cap, with EMA and
decay untouched (per decision).

### 4.6 Persistence (store + migration)

New migration `0029_affinity_bond_chemistry.sql`:

```sql
-- raw composites kept in lockstep with the axes by the DB (Postgres 12+).
-- Mirror of eros_engine_core::affinity::{bond_score,chemistry_score}
-- (warmth floored at 0 via GREATEST) — keep in sync if the formula changes.
ALTER TABLE engine.companion_affinity
  ADD COLUMN bond DOUBLE PRECISION
    GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (GREATEST(warmth,0) + trust    + intrigue) / 3))) STORED,
  ADD COLUMN chemistry DOUBLE PRECISION
    GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (GREATEST(warmth,0) + intimacy + tension)  / 3))) STORED;

-- lower the new-row default seed so a fresh session reads as "stranger" with
-- near-empty bars (existing rows unaffected by an ALTER ... SET DEFAULT).
-- warmth kept slightly positive (0.1 → still "平淡"/neutral attitude, not 冷淡);
-- patience keeps its 0.5 pacing default (rule-owned, not a bar). See §4.8.
ALTER TABLE engine.companion_affinity
  ALTER COLUMN warmth   SET DEFAULT 0.1,
  ALTER COLUMN trust    SET DEFAULT 0.0,
  ALTER COLUMN intrigue SET DEFAULT 0.0,
  ALTER COLUMN tension  SET DEFAULT 0.0;

-- per-turn tier transition; NULL on turns where no tier changed
ALTER TABLE engine.companion_affinity_events
  ADD COLUMN label_changes JSONB;
```

The `companion_affinity` columns store the **raw composite** (0..1), not the bar
value, and are **`GENERATED ALWAYS ... STORED`** — the DB recomputes them from the
axes on every insert/update, so they can never drift per-row and existing rows
auto-populate at migration time (no backfill, no engine write path, no
`load_or_create`/`record_ghost` special-casing). The bar curve + tiers live only
in the core read layer. Trade-off: the raw-composite formula is duplicated in SQL
(cross-referenced by comment); the **read-facing** bar/labels always derive from
core, so the API is correct even if the two ever diverged.

**Per-turn label change (engine-computed, deterministic).** The LLM still only
evaluates the 6 raw axes; the engine derives bond/chemistry, their tiers, and the
tier *transition* for the turn. In `persist_with_event`, snapshot the tiers over
the **same delta-only span as `effective_deltas`** (post-decay `before` →
post-delta `after`):

```
before_bond = bond_tier(bond_score(before))   after_bond = bond_tier(current.bond_score())
before_chem = chem_tier(chemistry_score(before)) after_chem = chem_tier(current.chemistry_score())

label_changes = {
  bond:      { from, to } if before_bond != after_bond,
  chemistry: { from, to } if before_chem != after_chem,
}  // NULL when neither tier moved
```

Delta-scoped (excludes decay) so it always agrees with `effective_deltas`.
`from`/`to` are tier **keys**. The legacy `relationship_label` transition is
**not** included (deprecated, derivable). Decay-induced tier drift is not
recorded as a discrete event (the absolute label is always available via the
snapshot); a follow-up could add it if needed.

New core type (so the store serializes a typed value, mirroring how `effective`
reuses `AffinityDeltas`):

```rust
pub struct LabelTransition { pub from: String, pub to: String }
pub struct TurnLabelChanges {            // serde skips None fields
    pub bond: Option<LabelTransition>,
    pub chemistry: Option<LabelTransition>,
}
// core: diff_labels(before: &Affinity, after: &Affinity) -> Option<TurnLabelChanges>
```

Store changes (`crates/eros-engine-store/src/affinity.rs`) — note `bond`/
`chemistry` are DB-generated, so **no engine write code** for them:

- `AffinityEventRow` gains `label_changes: Option<serde_json::Value>`; the
  `list_events` / `latest_turn_event` `SELECT`s add `e.label_changes`.
  (`AffinityRow` / `SELECT *` is unaffected — it ignores the new generated
  columns, and the read layer derives bond/chemistry from the axes anyway.)
- `persist_with_event`: clone the post-decay/pre-delta `current` as the `before`
  baseline; after `apply_deltas`, write `relationship_label` (new
  `legacy_relationship_label()`) and the event's `label_changes` (NULL when
  empty). bond/chemistry update themselves.
- `record_ghost`, `load_or_create`: unchanged for bond/chemistry (DB-generated);
  ghost has no axis change → no `label_changes`.

### 4.7 DTO / API surfaces

**`AffinitySnapshot`** (`routes/dto.rs`) — add four fields; compute on the fly
from the axes (pure functions):

```rust
pub bond: f64,            // bar value 0..1 (curve-applied)
pub chemistry: f64,       // bar value 0..1 (curve-applied)
pub bond_label: String,   // one of the 4 bond keys
pub chemistry_label: String, // one of the 4 chemistry keys
// relationship_label: unchanged field, now the legacy-mapped value (old names)
```

**BFF `/bff/v1/comp/affinity/{sid}/event`** (`routes/bff/affinity.rs`) — add two
siblings to `effective_deltas`:

```rust
pub effective_deltas_computed: BondChemistryDeltas, // { bond: f64, chemistry: f64 }
pub label_changes: Option<TurnLabelChangesDto>,     // engine-authoritative tier transition
```

`effective_deltas_computed` is the existing **post-EMA per-axis**
`effective_deltas` **folded linearly** into the two lines:

```
Δbond      = (Δwarmth/2 + Δtrust   + Δintrigue) / 3
Δchemistry = (Δwarmth/2 + Δintimacy + Δtension)  / 3
```

Raw-composite increment (not bar-% units): zero-cost, good for a per-turn
"+X bond / +Y chemistry" pulse.

`label_changes` is read **straight from the stored event column** (§4.6) — the
engine is the single authority, so the frontend stops computing transitions
itself (today's drift risk). `None`/absent when no tier moved that turn.

Both fields are mirrored onto the debug `/comp/affinity/{sid}/event` entries for
consistency (`effective_deltas_computed` is `Option`, `None` for pre-0014 rows
with no `effective_deltas`).

### 4.8 Starting point (default seed)

With the floored composite, a fresh session must still seed low enough to read as
`stranger`. The current defaults (`warmth 0.3, trust 0.2, intrigue 0.5, …`) would
put a brand-new session at bond ≈ 0.33 (tier 2) — past the "easy early" climb. So
migration 0029 lowers the new-row default seed:

| axis | old default | new default | why |
|------|-------------|-------------|-----|
| `warmth` | 0.3 | **0.1** | keep a neutral "平淡" opening tone (0 → 冷淡); near-zero bar |
| `trust` | 0.2 | **0.0** | start with no earned trust |
| `intrigue` | 0.5 | **0.0** | start with no built-up interest |
| `tension` | 0.1 | **0.0** | no romantic tension yet |
| `intimacy` | 0.0 | 0.0 | already zero |
| `patience` | 0.5 | 0.5 | rule-owned pacing axis, not a bar — unchanged |

Result: a fresh session has bond ≈ chemistry ≈ 0.033 → both tier 1 → legacy
`stranger`; bars start ≈ 5–6%. Only **new** rows are affected (`ALTER COLUMN …
SET DEFAULT` leaves existing rows alone). Side effects to expect (all benign,
flagged for review): a brand-new session's reply-length tier and warmth attitude
shift toward the strictest/neutral end, matching a true "just met" state. The
seed values are **tunable**.

## 5. Data flow

```
chat turn → post_process::evaluate_affinity
  → eval LLM (new prompt) → per-axis raw deltas
  → parse_affinity_eval (asymmetric clamp -0.6..+0.4)
  → merge with rule deltas → apply_deltas (EMA 0.2, unchanged)
  → store.persist_with_event:
       before := post-decay/pre-delta snapshot
       write 6 axes (unchanged) → bond/chemistry recomputed by the DB (generated)
       write relationship_label = legacy_relationship_label()  [old names]
       write event row (deltas, effective_deltas,
                        label_changes = diff_labels(before, after) or NULL)

read:
  AffinitySnapshot.from(Affinity) → bar(bond_score), bar(chemistry_score),
                                     bond_label, chemistry_label, legacy label
  BFF /event → effective_deltas + effective_deltas_computed (folded)
             + label_changes (read from the stored column)
```

## 6. Files touched + tests

**Core** (`eros-engine-core/src/affinity.rs`)
- Add `bond_score`, `chemistry_score`, the tier thresholds + `bar()` helper,
  `BondLabel`/`ChemistryLabel` enums + `bond_label`/`chemistry_label`,
  `legacy_relationship_label` (replaces `infer_label`), `LabelTransition` /
  `TurnLabelChanges` + `diff_labels`.
- Tests: score formulas; tier-boundary cases; `bar()` boundaries (0/0.15/0.35/
  0.62/1.0 → 0/0.25/0.50/0.75/1.0); legacy mapping table incl. `stranger` and
  tie→bond; `diff_labels` (single-line change, both, none→None); remove/replace
  old `infer_label_*` tests.

**Store** (`eros-engine-store/src/affinity.rs`, `migrations/0029_*.sql`)
- Migration: `bond`/`chemistry` as `GENERATED ALWAYS ... STORED`; lowered
  default seed (§4.8); events `label_changes JSONB`. `AffinityEventRow` +
  `list_events`/`latest_turn_event` add `label_changes`; `persist_with_event`
  writes new legacy label + per-event `label_changes` (bond/chemistry are
  DB-generated, no write code).
- Tests: generated bond/chemistry match `*_score()`; relationship_label uses new
  mapping; a tier-crossing turn writes `label_changes`, a flat turn writes NULL;
  update `load_or_create_idempotent` (now seeds `warmth 0.1`, `intrigue 0`).

**post_process** (`pipeline/post_process.rs`)
- Asymmetric clamp; update `parse_affinity_eval_clamps_out_of_range` (now
  +0.4 / −0.6).

**prompt** (`prompt.rs`)
- Rewrite the magnitude-guidance sentence in `affinity_eval_prompt`; keep the
  asserted six-value lines + JSON schema substrings so existing assertions hold.

**DTO / routes** (`routes/dto.rs`, `routes/bff/affinity.rs`, `routes/debug.rs`)
- `AffinitySnapshot` four new fields + tier→key serialization.
- `BffAffinityDelta.effective_deltas_computed` + `.label_changes`; debug
  `AffinityEventEntry` mirror. Tests: snapshot includes new fields/labels; BFF
  folded delta correct; BFF `label_changes` present on a tier-crossing turn,
  absent on a flat turn.

**Docs**
- Full rewrite of `docs/affinity-model.md` + `docs/affinity-model.zh.md` (stale):
  6-axis base, the two derived lines + formulas, tiers + bar curve, labels,
  legacy mapping, eval distribution + caps, persistence, DTO/BFF surfaces.
- Light touch on `docs/api-reference*.md` where affinity fields are described.
  OpenAPI schema updates automatically from the DTO `ToSchema` derives.

## 7. Tunables (review these)

- Tier thresholds `0.15 / 0.35 / 0.62` and the even 25% bands.
- Tier name keys (8) — rename freely.
- Caps `+0.4 / −0.6`.
- Legacy mapping table (esp. retiring `frenemy`; chemistry tier 1–2 → `slow_burn`).
- Folding weights (currently equal; `warmth` shared, floored at 0).
- The new-row default seed (§4.8: `warmth 0.1`, `trust/intrigue/tension 0`).

## 8. Open items

- None blocking. The naming overlap with `AffinityScope::bond()/chemistry()`
  (§4.1) is deliberately left for a future unification.

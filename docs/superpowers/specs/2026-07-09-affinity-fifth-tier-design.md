# Design: 5th (max) tier for Bond & Chemistry lines

**Date:** 2026-07-09
**Status:** approved

## Motivation

Downstream feedback on the affinity tier model:

1. **Tier 4 spans too wide a raw range.** Today tier 4 is `[0.62, 1.00]` — a 0.38-wide
   raw band, by far the widest. A user at bond 0.63 and a user at bond 0.98 read as the
   same tier, which flattens the top of the relationship arc.
2. **Levels 1–5 fit the consumer mental model better than 1–4.** A five-step ladder
   (with a rare, hard-earned top) reads as a more familiar progression.

## Current state (4 tiers)

Both derived lines (`bond`, `chemistry`) fold the unchanged 6-axis base into a raw
`0..1` composite, then map to a tier and a `0..1` progress-bar fill. Today:

| Tier | raw range | bar band | width |
|------|-----------|----------|-------|
| 1 | `[0.00, 0.15)` | `[0.00, 0.25)` | even 25% |
| 2 | `[0.15, 0.35)` | `[0.25, 0.50)` | even 25% |
| 3 | `[0.35, 0.62)` | `[0.50, 0.75)` | even 25% |
| 4 | `[0.62, 1.00]` | `[0.75, 1.00]` | even 25% |

`bar()` currently hardcodes an even `×0.25` band per tier.

## Design

Add a 5th, apex tier by **splitting the old tier 4 at raw 0.90**. Tiers 1–3 are
untouched (raw ranges *and* bar bands identical). The result:

| Tier | raw range | bar band | note |
|------|-----------|----------|------|
| 1 | `[0.00, 0.15)` | `[0.00, 0.25)` | unchanged |
| 2 | `[0.15, 0.35)` | `[0.25, 0.50)` | unchanged |
| 3 | `[0.35, 0.62)` | `[0.50, 0.75)` | unchanged |
| 4 | `[0.62, 0.90)` | `[0.75, 0.95)` | raw upper 1.0→0.9; bar top 1.0→0.95 |
| **5** | `[0.90, 1.00]` | `[0.95, 1.00]` | **new — a 5% apex band** |

**The even-25%-band invariant is intentionally dropped.** Tier 5 occupies only the top
5% of the bar (`[0.95, 1.00]`), so reaching the true ceiling reads as rare and
aspirational; tier 4 keeps most of the old top band (`[0.75, 0.95)`). The 5% band (vs a
tighter 2%) also limits lv4→lv5 damping — tier 5 spans 0.10 of raw score, so too narrow
a bar band would leave the bar nearly frozen while the raw score still climbs. `bar()`
is rewritten to carry explicit per-tier `(raw_lo, raw_hi, bar_lo, bar_hi)` bounds and
interpolate linearly within, instead of assuming a fixed `×0.25` band.

### New apex labels (wire keys)

Snake_case wire keys only; Chinese display lives in the frontend. Chosen with codex
input to avoid platonic/romantic synonym collision across the two lines:

| Line | Tier 1 | Tier 2 | Tier 3 | Tier 4 | **Tier 5 (new)** |
|------|--------|--------|--------|--------|------------------|
| **Bond** | `acquaintance` | `friend` | `close_friend` | `confidant` | **`soulmate`** (灵魂挚友) |
| **Chemistry** | `spark` | `flirtation` | `crush` | `lover` | **`beloved`** (至爱) |

## What does NOT change

- **Raw composite formula** — `bond_score`/`chemistry_score` and the 6-axis fold are
  untouched. Tiering is a pure read-layer reinterpretation of the same raw score.
- **DB — no new migration.** The `bond`/`chemistry` `GENERATED ALWAYS … STORED`
  columns (migration 0029) store the *raw* composite, not the tier/bar/label. The raw
  formula is unchanged, so the generated columns are correct as-is.
- **`legacy_relationship_label`** — needs no code change. Its romance test is
  `tier_index(chem) >= 3`, which naturally absorbs the new tier 5 (still ≥3 → romantic).
  The doc prose that reads "tier ∈ {3,4}" is updated to "≥3" / "{3,4,5}".
- **EMA smoothing, time decay, asymmetric eval caps** — untouched.

## Blast radius

Engine-only. Files:

- `crates/eros-engine-core/src/affinity.rs`
  - new `const TIER4_HI: f64 = 0.90;`
  - `tier_index` returns `1..=5`
  - `bar()` rewritten to per-tier explicit band bounds
  - `BondLabel::Soulmate` + `ChemistryLabel::Beloved` variants + `as_key` arms
  - `bond_label`/`chemistry_label` match arms extended to tier 5
  - tests (below)
- `crates/eros-engine-server/src/routes/dto.rs` — 2 doc-comment strings listing keys
- `crates/eros-engine-server/openapi.json` — 2 description strings listing keys
- `docs/affinity-model.md` + `docs/affinity-model.zh.md` — tier tables, bar-formula
  block, label tables, legacy-label prose

## Testing

TDD, in `affinity.rs`:

- `tier_index` boundaries: add `0.89 → 4`, `0.90 → 5`, `1.0 → 5`; the existing
  `1.0 → 4` assertion flips to `→ 5`.
- `bar`: `bar(0.90) == 0.95`, `bar(1.0) == 1.0`, a tier-4 interior point lands in
  `[0.75, 0.95)`, tiers 1–3 checkpoints unchanged.
- labels: a tier-5 case (`chem == 1.0 → beloved`; `bond == 1.0 → soulmate`); existing
  tier-4 cases still resolve to `lover`/`confidant`.
- Verify `crates/eros-engine-store/src/affinity.rs` test
  `label_changes_recorded_on_tier_crossing…` still lands where it asserts (its big
  positive turn may now cross into tier 5 — adjust the expected `to` key if so).

## Downstream follow-up (out of scope for this OSS repo)

The frontend (`eros-engine-web`) must add Chinese display strings for the two new keys
`soulmate` / `beloved`. Engine ships the keys; display mapping is a downstream concern
and is not part of this change.

## Out of scope

- No change to raw axis semantics, seed defaults, or the DB.
- No renaming of existing tiers 1–4.
- No change to `AffinityScope::bond()/chemistry()` (a separate, unrelated axis grouping
  used for prompt-injection scoping / `length_score`).

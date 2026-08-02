# Voice relationship line → Bond/Chemistry — design

- Date: 2026-08-02
- Status: design agreed, implementation plan pending
- Repo: `eros-engine`
- Amends: the "one relationship line" section of
  `2026-07-07-voice-call-parts-design.md` (which predates this change and
  still describes the cached-label read)

## Background & problem

`voice.rs::relationship_line` still reflects the pre-#132 relationship model:

- It reads the **cached** `affinity.relationship_label` DB field. Since
  PR #132 (Bond/Chemistry lines) the label is a *derived* value —
  `legacy_relationship_label()` folds the two line scores — and read paths
  (`dto.rs`) compute it on the fly instead of trusting the cache.
- The cache is only refreshed when the text path's affinity eval writes the
  row. The voice path runs no affinity eval **by design**, so a voice-heavy
  session's cached label never updates; rows untouched since #132 may still
  carry old-heuristic labels, including `frenemy`, which is retired from
  emission.
- Core guidance (`eros-engine-core/src/affinity.rs`) says new consumers
  should read `bond_label()` / `chemistry_label()`; the legacy 5-name
  vocabulary is kept only for API back-compat.

## Decision

Rewrite `relationship_line` in terms of the Bond/Chemistry tier labels,
derived from the affinity row at read time. Drop the `RelationshipLabel`
import from `voice.rs` entirely.

## Behaviour

- No affinity row ⇒ no line (unchanged).
- With a row: always emit **one** line (thin-prompt intent unchanged) —
  a base phrase from `bond_label()`, plus a romance clause appended in the
  same line when `chemistry_label()` is **Crush or higher** (tier ≥ 3).

| bond tier | base phrase |
| --- | --- |
| Acquaintance | "You two are still getting to know each other; keep it light." |
| Friend | "You two are friends; be warm and natural." |
| CloseFriend | "You two are close friends; be warm and familiar." |
| Confidant | "You two trust each other deeply; speak openly and at ease." |
| Soulmate | "You two know each other inside out; total comfort and familiarity." |

| chemistry tier | appended clause |
| --- | --- |
| Spark / Flirtation | (none — deliberately more conservative than the legacy `SlowBurn` fold: early chemistry must not push the voice tone toward romance) |
| Crush | "There's a growing attraction between you; let a little flirtation through." |
| Lover | "You share a romantic bond; be affectionate." |
| Beloved | "You two are deeply in love; be openly affectionate." |

### Behaviour changes (accepted)

1. Old rows cached with `frenemy` / stale heuristic labels no longer affect
   voice tone.
2. Rows whose cached label is NULL now get a line (previously silently
   none); a fresh all-zero row derives Acquaintance, matching what the old
   `Stranger` phrase conveyed.
3. The `relationship_label` field is no longer consumed anywhere on the
   voice path. The core enum itself is untouched (dto back-compat still
   uses it).

## Code changes

Only `crates/eros-engine-server/src/pipeline/voice.rs`:

- Import `BondLabel, ChemistryLabel` instead of `RelationshipLabel`.
- `relationship_line(a: &Affinity) -> String`; call site becomes
  `affinity.map(relationship_line)`.
- Update the doc comments that still say "cached `relationship_label`".

`core` / `store` / `dto` are untouched.

## Testing

- Replace the `affinity_with(label)` test helper with an axis-value
  constructor that lands on target tiers.
- Cases: no affinity ⇒ no line; fresh (all-zero) row ⇒ Acquaintance line;
  high bond + low chemistry ⇒ no romance wording; high chemistry ⇒
  affectionate clause present; boundary: chemistry tier 2 appends nothing.

## Out of scope

`story.rs` feeds the cached `relationship_label` string into the story
context JSON (`StoryAffinity` selects the raw column) — the same class of
staleness. Not touched here; fix under its own issue if wanted.

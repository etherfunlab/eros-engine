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
  a base phrase from `bond_label()`, plus the `chemistry_label()` tier's
  clause appended in the same line (every tier appends; the low tiers'
  clause explicitly holds romance back).

| bond tier     | base phrase |
|---------------|-------------|
| Acquaintance  | "You two are still getting to know each other; keep it light and natural." |
| Friend        | "You two are friends; be warm, easy, and natural." |
| CloseFriend   | "You two are close friends; be warm, familiar, and comfortable." |
| Confidant     | "You two trust each other deeply; speak openly, at ease, and with quiet closeness." |
| Soulmate      | "You two know each other inside out; total comfort, familiarity, and unspoken understanding." |

| chemistry tier     | appended clause |
|--------------------|-----------------|
| Spark / Flirtation | "A faint, unspoken spark exists between you. Keep it subtle — light teasing is allowed, but do not lean into romance or seduction yet." |
| Crush              | "There's a clear and growing attraction between you. Let soft flirtation and quiet allure color your words. Be teasing, a little magnetic, but still restrained." |
| Lover              | "You share a romantic and physical bond. Be affectionate, intimate, and gently alluring. Your voice and manner should feel warm, close, and quietly seductive." |
| Beloved            | "You two are deeply in love and highly attuned to each other. Be openly affectionate, sensual, and alluring. Speak with natural intimacy, quiet desire, and magnetic ease — as if the other person is already yours." |

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
- Cases: no affinity ⇒ no line; fresh (all-zero) row ⇒ Acquaintance base +
  the subtle Spark/Flirtation clause; high bond + low chemistry ⇒ the
  restrained clause, no Crush-and-up wording; high chemistry ⇒ affectionate
  clause present; boundary: tier 2 (Flirtation) still uses the subtle
  clause, tier 3 (Crush) switches to the attraction clause.

## Out of scope

`story.rs` feeds the cached `relationship_label` string into the story
context JSON (`StoryAffinity` selects the raw column) — the same class of
staleness. Not touched here; fix under its own issue if wanted.

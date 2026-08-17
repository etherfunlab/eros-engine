-- SPDX-License-Identifier: AGPL-3.0-only

-- Affinity 4.1 (docs/superpowers/specs/2026-08-17-affinity-41-design.md).
-- Storage + read surface only; no scoring math moves.

-- ── Tier columns ────────────────────────────────────────────────────────────
-- Written by the engine, NOT generated. Postgres forbids a generation
-- expression from referencing another generated column, and bond/chemistry are
-- already generated (0048) — so a generated tier would have to inline the line
-- formula and put the tier thresholds in SQL alongside the Rust ones. Two
-- authorities for the rule that decides what the user sees is exactly the
-- defect 4.1 exists to remove. eros_engine_core::affinity::tier_index stays the
-- only one.
--
-- No CHECK: the value is produced by engine code from a value the engine
-- already holds, so a range check would assert against our own writer. Its
-- absence is also what keeps the ladder extensible by formula — adding a sixth
-- tier is a change to tier_index plus a backfill, with no change to the table's
-- shape. (Contrast warmth_grade/patience_grade in 0048, which do carry CHECKs:
-- those originate in an LLM response, where validation earns its keep.)
--
-- DEFAULT 1 is derived, not arbitrary: a fresh row has every line axis at 0, so
-- both lines score 0, which is tier 1.
ALTER TABLE engine.companion_affinity
    ADD COLUMN bond_tier SMALLINT NOT NULL DEFAULT 1,
    ADD COLUMN chem_tier SMALLINT NOT NULL DEFAULT 1;

-- One-shot backfill at the 4.1 thresholds, inlined so nothing outlives the
-- migration. Mirror of tier_index over bond_score/chemistry_score.
UPDATE engine.companion_affinity SET
    bond_tier = CASE WHEN bond      < 0.15 THEN 1 WHEN bond      < 0.35 THEN 2
                     WHEN bond      < 0.62 THEN 3 WHEN bond      < 0.90 THEN 4
                     ELSE 5 END,
    chem_tier = CASE WHEN chemistry < 0.15 THEN 1 WHEN chemistry < 0.35 THEN 2
                     WHEN chemistry < 0.62 THEN 3 WHEN chemistry < 0.90 THEN 4
                     ELSE 5 END;

-- ── Replayable event audit ──────────────────────────────────────────────────
-- Same shape in both:
--   {warmth,trust,intrigue,intimacy,patience,tension, bond,chemistry,
--    bond_tier,chem_tier, warmth_grade,patience_grade,
--    ghost_streak,total_ghosts,updated_at}
--
-- The last three are what keep a ghost row honest: a ghost moves no axis, so
-- without them its two snapshots would be identical and would read as "nothing
-- happened" — while the operation has reset the decay clock and forgiven
-- however much absence had accrued.
--
-- Closes the two holes that made the trail unreplayable: events carried no
-- absolute state at all, and the absence effect applied at the head of
-- persist_with_event (apply_time_decay on the line axes + refresh_endpoints)
-- landed BEFORE the baseline snapshot, so it never appeared in effective_deltas
-- and was invisible in the log. Now state_after − state_before ==
-- effective_deltas within a row, and the gap between one row's state_after and
-- the next row's state_before IS that absence effect, stated rather than
-- inferred.
--
-- The snapshots carry bond/chemistry/tiers, which are derivable from the axes.
-- Deliberate: an audit row is an immutable point-in-time record and must say
-- what the tier WAS under the thresholds in force at the time — a later
-- re-derivation cannot reproduce that once the thresholds move. Live state
-- stays single-sourced; companion_affinity remains the only authority for
-- "where is this relationship now".
--
-- Not backfilled: pre-migration values are not recoverable, and inventing them
-- would defeat the columns' purpose. Old rows read NULL.
ALTER TABLE engine.companion_affinity_events
    ADD COLUMN state_before JSONB,
    ADD COLUMN state_after  JSONB;

-- ── Legacy label retired ────────────────────────────────────────────────────
-- The engine already stopped consuming this column in 4.0: the snapshot DTO
-- derived its relationship_label on read from the two line scores, and
-- pipeline/voice.rs documents that the relationship line must come from the
-- bond/chemistry tiers and never from the cached value. Only persist_with_event
-- kept writing it. bond_tier/chem_tier say strictly more.
--
-- Deployment ordering is mandatory — see spec §8. This DROP breaks every reader
-- of the column the instant it runs, with no gradual rollout: downstream must
-- stop selecting it and deploy FIRST.
ALTER TABLE engine.companion_affinity DROP COLUMN relationship_label;

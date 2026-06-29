-- SPDX-License-Identifier: AGPL-3.0-only

-- Derived bond/chemistry composites, kept in lockstep with the 6 axes by the DB.
-- Mirror of eros_engine_core::affinity::{bond_score,chemistry_score}
-- (warmth floored at 0 via GREATEST). Keep in sync if the formula changes.
ALTER TABLE engine.companion_affinity
    ADD COLUMN bond DOUBLE PRECISION
        GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (GREATEST(warmth, 0) + trust + intrigue) / 3))) STORED,
    ADD COLUMN chemistry DOUBLE PRECISION
        GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (GREATEST(warmth, 0) + intimacy + tension) / 3))) STORED;

-- Lower the new-row default seed so a fresh session reads as "stranger" with
-- near-empty bars (existing rows unaffected by ALTER ... SET DEFAULT). warmth
-- stays slightly positive (neutral "平淡" tone, not 冷淡); patience keeps 0.5.
ALTER TABLE engine.companion_affinity
    ALTER COLUMN warmth   SET DEFAULT 0.1,
    ALTER COLUMN trust    SET DEFAULT 0.0,
    ALTER COLUMN intrigue SET DEFAULT 0.0,
    ALTER COLUMN tension  SET DEFAULT 0.0;

-- Per-turn tier transition (bond/chemistry); NULL when no tier moved this turn.
ALTER TABLE engine.companion_affinity_events
    ADD COLUMN label_changes JSONB;

-- Exact per-turn line deltas (floored before/after bond/chemistry scores),
-- computed at persist time so the per-turn pulse is exact even when warmth < 0
-- (the linear axis fold was wrong below the warmth floor). NULL on pre-migration rows.
ALTER TABLE engine.companion_affinity_events
    ADD COLUMN effective_line_deltas JSONB;

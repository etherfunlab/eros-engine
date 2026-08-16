-- SPDX-License-Identifier: AGPL-3.0-only

-- Affinity 4.0 (docs/superpowers/specs/2026-08-16-affinity-40-design.md):
-- the two lines stop sharing warmth, and warmth/patience become derived from
-- judge levels × the counterpart line. Axis values are NOT migrated — the
-- composites are redefined over what the judge actually granted; the label
-- churn this causes was measured and accepted in the spec.

-- Authoritative judge levels (1 cold / 2 baseline / 3 warm). Backfill = 2:
-- existing warmth/patience values are not consulted, they no longer feed
-- anything.
ALTER TABLE engine.companion_affinity
    ADD COLUMN warmth_grade   SMALLINT NOT NULL DEFAULT 2,
    ADD COLUMN patience_grade SMALLINT NOT NULL DEFAULT 2,
    ADD CONSTRAINT companion_affinity_warmth_grade_range   CHECK (warmth_grade   BETWEEN 1 AND 3),
    ADD CONSTRAINT companion_affinity_patience_grade_range CHECK (patience_grade BETWEEN 1 AND 3);

-- Postgres cannot alter a generation expression in place: drop + re-add.
-- Mirror of eros_engine_core::affinity::{bond_score,chemistry_score} (4.0).
ALTER TABLE engine.companion_affinity
    DROP COLUMN bond,
    DROP COLUMN chemistry;
ALTER TABLE engine.companion_affinity
    ADD COLUMN bond DOUBLE PRECISION
        GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (trust + intrigue) / 2))) STORED,
    ADD COLUMN chemistry DOUBLE PRECISION
        GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (intimacy + tension) / 2))) STORED;

-- warmth range is now 0..1 (derived cache). New-row defaults: level 2 over
-- empty lines ⇒ 1/3 · B(0) = 1/3 · (1 − 0.35·10/13) ≈ 0.2436 for both
-- endpoints (patience start drops from 0.5 by design: strangers have limited
-- patience). Exact expression, not a rounded literal, so the cache is
-- bit-reproducible from the derivation before any refresh path runs.
ALTER TABLE engine.companion_affinity
    ALTER COLUMN warmth   SET DEFAULT ((1.0/3.0) * (1 - 0.35 * 10.0/13.0)),
    ALTER COLUMN patience SET DEFAULT ((1.0/3.0) * (1 - 0.35 * 10.0/13.0));

-- One-time cache refresh for existing rows (levels are all 2, so the φ floor
-- cannot apply and decay is refreshed on next read anyway):
-- value = 1/3 · (1 + (10/13)·(counterpart − 0.35)).
UPDATE engine.companion_affinity SET
    warmth   = LEAST(1, (1.0/3.0) * (1 + (10.0/13.0) * (chemistry - 0.35))),
    patience = LEAST(1, (1.0/3.0) * (1 + (10.0/13.0) * (bond      - 0.35)));

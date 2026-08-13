-- Affinity 3.0: per-axis balance the threshold gate is still holding back
-- (AFFINITY_DELTA_THRESHOLD). Written by the graded message path only; NULL
-- (every pre-3.0 row, and rows never gated) reads as all-zero.
ALTER TABLE engine.companion_affinity
    ADD COLUMN pending_deltas JSONB;

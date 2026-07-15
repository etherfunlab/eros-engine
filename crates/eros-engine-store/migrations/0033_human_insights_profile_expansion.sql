-- SPDX-License-Identifier: AGPL-3.0-only
-- Six new profile slots (spec 2026-07-15-insight-memory-enrichment). Nullable
-- TEXT adds = metadata-only change. No backfill: existing companion_insights
-- JSONB lacks these keys, and the mirror is full-overwrite on every merge, so
-- columns populate on each user's next extracting turn.
ALTER TABLE engine.human_insights
    ADD COLUMN education            TEXT,
    ADD COLUMN family               TEXT,
    ADD COLUMN relationship_history TEXT,
    ADD COLUMN social_pattern       TEXT,
    ADD COLUMN future_plans         TEXT,
    ADD COLUMN finance_status       TEXT;

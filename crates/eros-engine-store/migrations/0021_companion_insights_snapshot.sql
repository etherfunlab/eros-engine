-- SPDX-License-Identifier: AGPL-3.0-only
-- engine.companion_insights_snapshot — append-only history of
-- companion_insights, one row per user per sweeper fire.
--
-- captured_at carries no DEFAULT: the sweeper passes the fire-instant
-- timestamp explicitly so every row of a fire shares one value, letting
-- downstream group cleanly by "fire" without bucketing.
--
-- Spec: docs/superpowers/specs/2026-05-29-engine-cleanup-and-snapshot-design.md §3

CREATE TABLE engine.companion_insights_snapshot (
    id              UUID             PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id         UUID             NOT NULL,
    insights        JSONB            NOT NULL,
    training_level  DOUBLE PRECISION NOT NULL,
    captured_at     TIMESTAMPTZ      NOT NULL
);

CREATE INDEX idx_companion_insights_snapshot_user_time
    ON engine.companion_insights_snapshot (user_id, captured_at DESC);

-- Supabase lockdown, mirroring migration 0013. REVOKEs are wrapped in
-- pg_roles existence checks so non-Supabase Postgres (where anon /
-- authenticated don't exist — including the sqlx test DB) skips them
-- silently. The RLS enable runs unconditionally; with no policy attached,
-- only owner (postgres) and service_role connections can touch the table.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.companion_insights_snapshot FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.companion_insights_snapshot FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.companion_insights_snapshot ENABLE ROW LEVEL SECURITY;

-- SPDX-License-Identifier: AGPL-3.0-only
-- engine.companion_insights_events — append-only, ONE row per OpenRouter call
-- of insight_extraction (cost + what-was-extracted audit). A run makes up to
-- two calls (facts → structured), tied by a shared run_id. Distinct from
-- companion_insights_snapshot (periodic STATE history, migration 0021).
--
-- Also adds the OpenRouter audit trio (model/usage/generation_id, mirroring
-- chat_messages from migration 0012) to companion_affinity_events.
--
-- Spec: docs/superpowers/specs/2026-06-03-insight-events-and-audit-columns-design.md

CREATE TABLE engine.companion_insights_events (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id        UUID NOT NULL,
    user_id       UUID NOT NULL,
    session_id    UUID,
    message_id    UUID,
    stage         TEXT NOT NULL CHECK (stage IN ('facts','structured')),
    status        TEXT NOT NULL CHECK (status IN ('ok','empty','parse_error')),
    payload       JSONB,
    model         TEXT,
    usage         JSONB,
    generation_id TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_companion_insights_events_user_time
    ON engine.companion_insights_events (user_id, created_at DESC);
CREATE INDEX idx_companion_insights_events_run
    ON engine.companion_insights_events (run_id);

-- Supabase lockdown, mirroring 0021_companion_insights_snapshot.sql. REVOKEs
-- are wrapped in pg_roles existence checks so non-Supabase Postgres (incl. the
-- sqlx test DB, where anon/authenticated don't exist) skips them silently. The
-- RLS enable runs unconditionally; with no policy attached, only owner
-- (postgres) and service_role connections can touch the table.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.companion_insights_events FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.companion_insights_events FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.companion_insights_events ENABLE ROW LEVEL SECURITY;

-- Audit trio on the existing affinity events table (nullable, no backfill).
ALTER TABLE engine.companion_affinity_events
    ADD COLUMN model         TEXT,
    ADD COLUMN usage         JSONB,
    ADD COLUMN generation_id TEXT;

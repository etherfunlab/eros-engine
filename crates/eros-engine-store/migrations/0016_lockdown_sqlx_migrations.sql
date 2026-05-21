-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Lock down public._sqlx_migrations. After the 2026-05-22 eros-app /
-- eros-gateway decommission this is the only table left in `public`, the
-- schema Supabase exposes over PostgREST at /rest/v1/. Same defense-in-depth
-- posture as 0013_supabase_lockdown for engine.* tables: revoke the anon /
-- authenticated roles and enable RLS so the migration history (schema
-- versions + descriptions) can't be read by holders of the publishable anon
-- key.
--
-- Safe for the migration runner: sqlx connects as the table owner, which
-- bypasses RLS, so recording this and every later migration is unaffected.
-- Unlike 0013 we do NOT touch schema-level USAGE on `public` — that schema
-- is the default search path and revoking it would break unrelated access.
--
-- The REVOKEs are wrapped in pg_roles existence checks so non-Supabase
-- Postgres deployments (no anon / authenticated roles) skip them silently.
-- ENABLE ROW LEVEL SECURITY runs unconditionally and is idempotent.

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON public._sqlx_migrations FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON public._sqlx_migrations FROM authenticated;
    END IF;
END
$$;

ALTER TABLE public._sqlx_migrations ENABLE ROW LEVEL SECURITY;

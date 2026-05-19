-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Supabase lockdown. Defense-in-depth migration for deployments that put
-- eros-engine's schema on a Supabase Postgres project. Fixes issue #23.
--
-- Why this exists. Supabase exposes any schema added to Studio's "Exposed
-- schemas" list over the PostgREST API at /rest/v1/. To let a co-deployed
-- web app (e.g. eros-engine-web) read engine.* through @supabase/supabase-js,
-- operators commonly add `engine` to that list. The hazard: if the
-- `anon` / `authenticated` roles ever picked up SELECT / INSERT / UPDATE /
-- DELETE grants on engine.* tables — either via the Studio "Permissions"
-- panel or a stock template — every holder of the publishable anon key
-- (which by design ships in every browser bundle) can hit the raw rows
-- through PostgREST, no auth required.
--
-- This migration's three steps neutralise that even if the operator
-- accidentally toggled the boxes:
--
--   1. REVOKE ALL on every engine.* table from anon, authenticated.
--   2. REVOKE USAGE on schema engine from anon, authenticated. PostgREST
--      needs schema USAGE to enumerate tables; pulling it makes the schema
--      effectively invisible to those roles regardless of object-level
--      grants picked up later.
--   3. ENABLE ROW LEVEL SECURITY on every engine.* table. Pure defense in
--      depth — there are NO policies attached, so the only access paths are
--      (a) `postgres` owner connections (sqlx migration / engine app), and
--      (b) `service_role` connections from the engine's server side. Both
--      bypass RLS.
--
-- The whole thing is wrapped in `pg_roles` existence checks so non-Supabase
-- Postgres deployments (where anon / authenticated don't exist) skip the
-- REVOKEs silently. The RLS enable runs unconditionally — it's safe on any
-- Postgres and only changes behaviour for clients that aren't the owner or
-- service_role.
--
-- Re-running this migration is a no-op: REVOKE on a non-existent grant
-- succeeds, and `ALTER TABLE ... ENABLE ROW LEVEL SECURITY` is idempotent.

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.chat_messages              FROM anon;
        REVOKE ALL ON engine.chat_sessions              FROM anon;
        REVOKE ALL ON engine.persona_instances          FROM anon;
        REVOKE ALL ON engine.companion_affinity         FROM anon;
        REVOKE ALL ON engine.companion_affinity_events  FROM anon;
        REVOKE ALL ON engine.companion_insights         FROM anon;
        REVOKE ALL ON engine.companion_memories         FROM anon;
        REVOKE ALL ON engine.persona_genomes            FROM anon;
        REVOKE ALL ON engine.persona_ownership          FROM anon;
        REVOKE ALL ON engine.sync_cursors               FROM anon;
        REVOKE ALL ON engine.wallet_links               FROM anon;
        REVOKE USAGE ON SCHEMA engine FROM anon;
    END IF;

    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.chat_messages              FROM authenticated;
        REVOKE ALL ON engine.chat_sessions              FROM authenticated;
        REVOKE ALL ON engine.persona_instances          FROM authenticated;
        REVOKE ALL ON engine.companion_affinity         FROM authenticated;
        REVOKE ALL ON engine.companion_affinity_events  FROM authenticated;
        REVOKE ALL ON engine.companion_insights         FROM authenticated;
        REVOKE ALL ON engine.companion_memories         FROM authenticated;
        REVOKE ALL ON engine.persona_genomes            FROM authenticated;
        REVOKE ALL ON engine.persona_ownership          FROM authenticated;
        REVOKE ALL ON engine.sync_cursors               FROM authenticated;
        REVOKE ALL ON engine.wallet_links               FROM authenticated;
        REVOKE USAGE ON SCHEMA engine FROM authenticated;
    END IF;
END
$$;

-- Defense-in-depth RLS. No policies attached — owner (postgres) and
-- service_role bypass RLS, which covers every legitimate access path:
-- sqlx migration runs, the engine binary's pool, and any server-side
-- Supabase client that uses the service-role key. Browser-side clients
-- using the anon key would be blocked here even if the REVOKEs above
-- somehow drifted.
ALTER TABLE engine.chat_messages              ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.chat_sessions              ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.persona_instances          ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.companion_affinity         ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.companion_affinity_events  ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.companion_insights         ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.companion_memories         ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.persona_genomes            ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.persona_ownership          ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.sync_cursors               ENABLE ROW LEVEL SECURITY;
ALTER TABLE engine.wallet_links               ENABLE ROW LEVEL SECURITY;

-- SPDX-License-Identifier: AGPL-3.0-only
--
-- World Worldview (spec: docs/superpowers/specs/2026-07-28-world-worldview-design.md).
--
-- engine.world_worldviews — downstream-managed worldview table. The engine
-- only ever SELECTs it (over the service_role/owner connection); downstream
-- INSERTs/UPDATEs/DELETEs rows. The engine ships no default worldview: an
-- enrolled owner with no row here (or blank content) receives no World
-- System LLM activity until downstream provides one.
--
-- world_states gains two engine-owned columns: worldview_hash (SHA-256 hex
-- of the content used by the last completed director round; NULL = pre-
-- worldview world, forces an init-style reset on first sight of a
-- worldview) and worldview_set_at (start of the current worldview era; Town
-- AI activity never targets posts published before it).

CREATE TABLE engine.world_worldviews (
    owner_uid  UUID PRIMARY KEY,
    content    TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT worldview_content_len
        CHECK (char_length(content) BETWEEN 1 AND 10000)
);

-- Keep updated_at honest even if downstream forgets to set it: the WM
-- claim query uses it to detect "worldview touched since last round".
-- Bump ONLY on content change — content-identical UPDATEs (or explicit
-- updated_at writes, e.g. in tests) must not register as a touch.
--
-- Uses clock_timestamp(), NOT now(): now() is transaction-stable (pinned to
-- the calling transaction's START), so an UPDATE that blocks on
-- persist_round's `FOR SHARE` guard and then proceeds after that
-- transaction commits would otherwise stamp a time from BEFORE the block —
-- possibly earlier than the just-committed `last_run_at` — silently
-- defeating claim_due's touch-dueness check (`ww.updated_at >
-- ws.last_run_at`) a second time. clock_timestamp() reads the actual wall
-- clock at trigger-execution time, i.e. after the block clears, so the
-- stamp always reflects when the change really took effect.
CREATE OR REPLACE FUNCTION engine.touch_world_worldviews()
RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
    IF NEW.content IS DISTINCT FROM OLD.content THEN
        NEW.updated_at := clock_timestamp();
    END IF;
    RETURN NEW;
END
$fn$;

CREATE TRIGGER trg_world_worldviews_touch
    BEFORE UPDATE ON engine.world_worldviews
    FOR EACH ROW EXECUTE FUNCTION engine.touch_world_worldviews();

ALTER TABLE engine.world_states
    ADD COLUMN worldview_hash   TEXT,
    ADD COLUMN worldview_set_at TIMESTAMPTZ;

-- 0013-style lockdown: REVOKE from Supabase browser roles (when present) and
-- enable policy-less RLS so only owner/service_role connections reach rows.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.world_worldviews FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.world_worldviews FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.world_worldviews ENABLE ROW LEVEL SECURITY;

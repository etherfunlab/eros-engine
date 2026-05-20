-- SPDX-License-Identifier: AGPL-3.0-only

-- Post-EMA effective change (after − before) per affinity event. NULL on
-- rows written before this migration; the FE observation surface only needs
-- live/recent turns, so historical NULLs are acceptable.
ALTER TABLE engine.companion_affinity_events
    ADD COLUMN effective_deltas JSONB;

-- post_process.rs already emits event_type = 'proactive', but the original
-- 0002 CHECK omitted it, so those INSERTs silently failed (warn-logged) and
-- never landed. Widen the CHECK so proactive affinity events persist.
--
-- Drop the existing event_type CHECK by *catalog lookup* (not a guessed
-- name): a blind DROP CONSTRAINT IF EXISTS <wrong-name> would leave the old
-- constraint in place and keep rejecting 'proactive'.
DO $$
DECLARE
    cname text;
BEGIN
    SELECT con.conname INTO cname
    FROM pg_constraint con
    JOIN pg_class rel ON rel.oid = con.conrelid
    JOIN pg_namespace nsp ON nsp.oid = rel.relnamespace
    WHERE nsp.nspname = 'engine'
      AND rel.relname = 'companion_affinity_events'
      AND con.contype = 'c'
      AND pg_get_constraintdef(con.oid) ILIKE '%event_type%';
    IF cname IS NOT NULL THEN
        EXECUTE format(
            'ALTER TABLE engine.companion_affinity_events DROP CONSTRAINT %I',
            cname
        );
    END IF;
END $$;

ALTER TABLE engine.companion_affinity_events
    ADD CONSTRAINT companion_affinity_events_event_type_check
    CHECK (event_type IN ('message', 'ghost', 'gift', 'proactive', 'time_decay'));

-- Covering index for per-session event reads (join on affinity_id, order by
-- created_at DESC, id DESC). Existing idx_affinity_events_affinity is affinity_id-only.
CREATE INDEX idx_affinity_events_affinity_created
    ON engine.companion_affinity_events (affinity_id, created_at DESC, id DESC);

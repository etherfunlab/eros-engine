-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Life events, one row each (docs/superpowers/specs/2026-10-15-life-events-v2-design.md §6).
--
-- An event belongs to the instance's life: instance_id cascades. session_id is
-- where it was drawn and told; a deleted session does not undo it, so it is
-- SET NULL, and instance_id is stored because session_id can be null.
-- message_id is the message that told it; away_from_phone has none, and a
-- deleted message does not undo the event: SET NULL. kind has no CHECK: the
-- engine's core owns the list. Category and background-ness derive from kind.
-- Each FK column leads an index.

CREATE TABLE engine.persona_events (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id UUID NOT NULL
                REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    session_id  UUID
                REFERENCES engine.chat_sessions(id) ON DELETE SET NULL,
    event_key   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    started_at  TIMESTAMPTZ NOT NULL,
    ends_at     TIMESTAMPTZ NOT NULL CHECK (ends_at > started_at),
    message_id  UUID
                REFERENCES engine.chat_messages(id) ON DELETE SET NULL
);

CREATE INDEX persona_events_instance_idx
    ON engine.persona_events (instance_id, started_at DESC);
CREATE UNIQUE INDEX persona_events_session_key_uidx
    ON engine.persona_events (session_id, event_key);
CREATE UNIQUE INDEX persona_events_message_uidx
    ON engine.persona_events (message_id) WHERE message_id IS NOT NULL;

-- Supabase lockdown, mirroring 0067. REVOKEs are guarded by pg_roles existence
-- so non-Supabase Postgres (incl. the sqlx test DB) skips them silently. RLS is
-- enabled unconditionally; with no policy, only owner/service_role can touch it.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.persona_events FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.persona_events FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.persona_events ENABLE ROW LEVEL SECURITY;

-- Every event written as a message's metadata (0071). Those rows keep their
-- metadata and 0071's index stays: the previous image reads them.
INSERT INTO engine.persona_events
    (instance_id, session_id, event_key, kind, started_at, ends_at, message_id)
SELECT s.instance_id, m.session_id, m.metadata->>'event_key',
       m.metadata->'event'->>'kind', m.sent_at,
       (m.metadata->'event'->>'ends_at')::timestamptz, m.id
  FROM engine.chat_messages m
  JOIN engine.chat_sessions s ON s.id = m.session_id
 WHERE m.metadata->>'proactive' = 'event'
   AND s.instance_id IS NOT NULL
   AND m.metadata->>'event_key' IS NOT NULL
   AND m.metadata->'event'->>'kind' IS NOT NULL
   AND m.metadata->'event'->>'ends_at' IS NOT NULL;

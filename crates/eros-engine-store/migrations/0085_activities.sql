-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Activity: a date the user invites the persona to, run in a sub-session
-- (docs/superpowers/specs/2026-11-12-activity-sub-session-design.md §9).
--
-- chat_sessions.channel gains 'activity' (the sub-session's own rows), and
-- companion_affinity_events.event_type gains 'activity' (the one event a
-- settled date writes). Each widening swaps a CHECK constraint; every existing
-- row still satisfies the wider one.
--
-- engine.activities: one row per accepted date. The parent session, the
-- sub-session, the invitation message and the on_a_date event are each
-- referenced with a foreign key. The row stores no user_id or instance_id (the
-- parent session determines both) and no status (ongoing / ended / settled are
-- derived from the event's ends_at and settled_at).
--
-- ALTER TABLE takes ACCESS EXCLUSIVE on the table it swaps a CHECK on, so a
-- lock_timeout makes a blocked migration fail before the new image serves,
-- which is the safe failure.

SET LOCAL lock_timeout = '3s';

ALTER TABLE engine.chat_sessions
    DROP CONSTRAINT chat_sessions_channel_check;
ALTER TABLE engine.chat_sessions
    ADD CONSTRAINT chat_sessions_channel_check
    CHECK (channel IN ('text', 'voice', 'group', 'activity'));

ALTER TABLE engine.companion_affinity_events
    DROP CONSTRAINT companion_affinity_events_event_type_check;
ALTER TABLE engine.companion_affinity_events
    ADD CONSTRAINT companion_affinity_events_event_type_check
    CHECK (event_type IN ('message', 'ghost', 'gift', 'proactive', 'time_decay', 'life_choice', 'activity'));

CREATE TABLE engine.activities (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id        UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    sub_session_id    UUID NOT NULL UNIQUE REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    message_id        UUID UNIQUE REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    event_id          UUID REFERENCES engine.persona_events(id) ON DELETE SET NULL,
    duration          TEXT NOT NULL CHECK (duration IN ('short', 'mid')),
    place             TEXT NOT NULL,
    content           TEXT NOT NULL,
    settle_claimed_at TIMESTAMPTZ,
    settled_at        TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_activities_session ON engine.activities (session_id, created_at DESC);
CREATE INDEX idx_activities_event ON engine.activities (event_id);

-- Supabase lockdown, as in 0084: REVOKEs only where the roles exist; RLS with
-- no policy leaves the table to the owner and service_role.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.activities FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.activities FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.activities ENABLE ROW LEVEL SECURITY;

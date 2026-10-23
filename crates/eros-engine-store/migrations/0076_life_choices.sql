-- SPDX-License-Identifier: AGPL-3.0-only
--
-- A choice the persona puts to the user, one row each (docs/superpowers/specs/
-- 2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md §3.6).
--
-- A choice belongs to one relationship: session_id is non-null and cascades.
-- message_id is the audit thread to the ask, event_id to the taking_space row
-- an accept wrote (NULL on every other path); both outlive what they point at:
-- SET NULL. kind and status have no CHECK: the engine owns the vocabulary.
-- Expiry is derived from asked_at (24 hours), never stored. Each FK column
-- leads an index.

CREATE TABLE engine.life_choices (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id  UUID NOT NULL REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,              -- 'ask_for_space'
    status      TEXT NOT NULL DEFAULT 'pending',
                -- 'pending' | 'accepted' | 'declined' | 'ignored'
    tier_gap    SMALLINT NOT NULL,          -- 2..4, at ask time
    asked_at    TIMESTAMPTZ NOT NULL,
    resolved_at TIMESTAMPTZ,
    message_id  UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    event_id    UUID REFERENCES engine.persona_events(id) ON DELETE SET NULL
);
CREATE INDEX life_choices_session_idx ON engine.life_choices (session_id, asked_at DESC);
CREATE INDEX life_choices_message_idx ON engine.life_choices (message_id);
CREATE INDEX life_choices_event_idx   ON engine.life_choices (event_id);

-- Supabase lockdown, mirroring 0072. REVOKEs are guarded by pg_roles existence
-- so non-Supabase Postgres (incl. the sqlx test DB) skips them silently. RLS is
-- enabled unconditionally; with no policy, only owner/service_role can touch it.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.life_choices FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.life_choices FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.life_choices ENABLE ROW LEVEL SECURITY;

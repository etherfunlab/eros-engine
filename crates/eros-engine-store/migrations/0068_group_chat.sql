-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Group chat (docs/superpowers/specs/2026-10-12-group-chat-design.md).
--
-- A group is a chat_sessions row with channel = 'group' and no instance: the
-- user and up to three member sessions, each an existing 1:1 text session.
-- The cap and the member checks run in the add transaction under a row lock;
-- no constraint can count rows. title is the user's name for the group,
-- nullable and group-only.
--
-- chat_session_members is a dependent table — a membership means nothing
-- without either session — so both pointers cascade, and the primary key's
-- prefix serves group_session_id. A kick deletes the row. Who said a group
-- line lives on chat_messages.speaker_session_id instead: nullable (user rows
-- and 1:1 rows have none), so SET NULL — a deleted member's lines stay.

ALTER TABLE engine.chat_sessions
    DROP CONSTRAINT chat_sessions_channel_check;
ALTER TABLE engine.chat_sessions
    ADD CONSTRAINT chat_sessions_channel_check
        CHECK (channel IN ('text', 'voice', 'group'));
ALTER TABLE engine.chat_sessions
    ADD CONSTRAINT chat_sessions_group_no_instance
        CHECK (channel <> 'group' OR instance_id IS NULL);
ALTER TABLE engine.chat_sessions
    ADD COLUMN title TEXT;
ALTER TABLE engine.chat_sessions
    ADD CONSTRAINT chat_sessions_title_group_only
        CHECK (title IS NULL
               OR (channel = 'group' AND char_length(btrim(title)) BETWEEN 1 AND 64));

CREATE TABLE engine.chat_session_members (
    group_session_id  UUID NOT NULL
                      REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    member_session_id UUID NOT NULL
                      REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    joined_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (group_session_id, member_session_id)
);
CREATE INDEX chat_session_members_member_idx
    ON engine.chat_session_members (member_session_id);

-- Supabase lockdown, mirroring 0059. REVOKEs are guarded by pg_roles existence
-- so non-Supabase Postgres (incl. the sqlx test DB) skips them silently. RLS is
-- enabled unconditionally; with no policy, only owner/service_role can touch it.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.chat_session_members FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.chat_session_members FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.chat_session_members ENABLE ROW LEVEL SECURITY;

ALTER TABLE engine.chat_messages
    ADD COLUMN speaker_session_id UUID
        REFERENCES engine.chat_sessions(id) ON DELETE SET NULL;
ALTER TABLE engine.chat_messages
    ADD CONSTRAINT chat_messages_speaker_assistant_only
        CHECK (speaker_session_id IS NULL OR role = 'assistant');
CREATE INDEX chat_messages_speaker_session_idx
    ON engine.chat_messages (speaker_session_id) WHERE speaker_session_id IS NOT NULL;

-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Message reactions (spec 2026-10-05-message-reactions-design.md §3): the one
-- emoji a user set on an assistant row, and when it was set. Both columns are
-- new and NULL on every existing row, so the CHECKs validate in one pass.

ALTER TABLE engine.chat_messages
    ADD COLUMN reaction   TEXT,
    ADD COLUMN reacted_at TIMESTAMPTZ;

ALTER TABLE engine.chat_messages
    ADD CONSTRAINT chat_messages_reaction_pair
        CHECK ((reaction IS NULL) = (reacted_at IS NULL)),
    ADD CONSTRAINT chat_messages_reaction_assistant_only
        CHECK (reaction IS NULL OR role = 'assistant');

-- The per-turn read: a session's reactions set after a given instant.
CREATE INDEX idx_chat_messages_session_reacted_at
    ON engine.chat_messages (session_id, reacted_at)
    WHERE reaction IS NOT NULL;

-- SPDX-License-Identifier: AGPL-3.0-only
--
-- engine.chat_sessions gains a `channel` column separating voice-call sessions
-- from default text-chat sessions. Extends 0030's chat_messages.channel to the
-- session level: start/resume becomes channel-scoped, so a voice client and a
-- text client hold independent conversations for the same user × instance.
--
-- Unlike the message column (NULL = text), this is NOT NULL DEFAULT 'text':
-- every pre-existing session IS a text session, and a required value keeps the
-- resume filter a plain equality. CHECK mirrors 0030 and is trivially extended
-- when new channels appear.

ALTER TABLE engine.chat_sessions
    ADD COLUMN channel TEXT NOT NULL DEFAULT 'text'
        CHECK (channel IN ('text', 'voice'));

-- Covers the channel-scoped resume lookup (latest session per user × instance
-- × channel, ordered by recency).
CREATE INDEX idx_chat_sessions_user_instance_channel
    ON engine.chat_sessions (user_id, instance_id, channel, last_active_at DESC);

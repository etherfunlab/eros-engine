-- SPDX-License-Identifier: AGPL-3.0-only
--
-- engine.chat_messages gains a `channel` column distinguishing the voice
-- channel from the default text chat. Voice I/O is still text; only the entry
-- endpoint differs. NULL = prior behaviour (text). 'voice' = voice channel.
-- Symmetric across user and assistant rows, and the single key used to exclude
-- voice turns from session-end memory extraction (dreaming).
--
-- Adding a nullable column is a metadata-only change (no table rewrite). The
-- CHECK mirrors the codebase convention (cf. assistant_action_type in 0012) and
-- is trivially extended when new channels appear. No index: the dreaming filter
-- is already narrowed by session_id.

ALTER TABLE engine.chat_messages
    ADD COLUMN channel TEXT NULL
        CHECK (channel IS NULL OR channel IN ('voice'));

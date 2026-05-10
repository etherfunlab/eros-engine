-- SPDX-License-Identifier: AGPL-3.0-only
-- Marker for the dreaming-lite classifier sweep.
--
-- A background sweeper in eros-engine-server picks idle sessions
-- (last_active_at older than idle_threshold AND classified_at IS NULL),
-- runs the memory_extraction LLM task on their chat_messages, and writes
-- categorised profile-layer rows back to engine.companion_memories.
--
-- Setting classified_at = now() at the end of the sweep makes the run
-- idempotent — the next tick won't re-process a session that's already
-- been classified, even if more turns arrive afterwards. (OSS v1
-- treats post-classification activity as a separate session lifecycle;
-- incremental re-classification is a future iteration.)
ALTER TABLE engine.chat_sessions
    ADD COLUMN classified_at TIMESTAMPTZ;

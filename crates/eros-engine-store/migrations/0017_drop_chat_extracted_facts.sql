-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Drop the vestigial engine.chat_messages.extracted_facts column. It dates
-- to the original 0001 chat schema (inline per-message fact extraction) and
-- was never written by the live pipeline — memory/fact extraction moved to
-- engine.companion_memories (post_process raw-turn dump + dreaming-lite
-- classifier). The column was therefore always NULL, so dropping it loses
-- no data.
ALTER TABLE engine.chat_messages DROP COLUMN extracted_facts;

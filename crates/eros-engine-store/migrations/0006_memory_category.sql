-- SPDX-License-Identifier: AGPL-3.0-only
-- Optional category tag for companion_memories rows.
--
-- Currently NULL on every row — the raw-turn writer in
-- `pipeline::post_process::write_turn` does not classify content.
-- A future extraction step will populate it with values like
-- 'fact' | 'preference' | 'event' | 'emotion' so retrieval can
-- weight or filter by memory type. Schema is added now so that
-- shipping the classifier later doesn't require a backfill migration.
ALTER TABLE engine.companion_memories
    ADD COLUMN category TEXT;

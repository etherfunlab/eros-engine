-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Which turn an affinity event belongs to.
--
-- Points at the USER message that drove the turn: a turn has exactly one,
-- while it may have zero (ghost) or several (burst) assistant messages.
-- Assistant rows already reference it via chat_messages.user_message_id, so
-- "all replies for this event" is a join, not a second column.
--
-- A real FK, unlike the other audit tables: this table already cascades away
-- with its session through affinity_id, so "the trail outlives the row" never
-- held here. SET NULL (not CASCADE) so a message deleted on its own blanks
-- the pointer instead of erasing the event.
--
-- NULL on proactive / time_decay rows and on every row written before this
-- migration. No backfill: a timestamp-proximity guess is what this column
-- exists to replace.

ALTER TABLE engine.companion_affinity_events
    ADD COLUMN user_message_id UUID NULL
        REFERENCES engine.chat_messages(id) ON DELETE SET NULL;

CREATE INDEX idx_affinity_events_user_message
    ON engine.companion_affinity_events (user_message_id)
    WHERE user_message_id IS NOT NULL;

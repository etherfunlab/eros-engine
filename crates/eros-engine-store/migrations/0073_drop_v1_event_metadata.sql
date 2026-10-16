-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Life events live in engine.persona_events (0072;
-- docs/superpowers/specs/2026-10-15-life-events-v2-design.md §6.3). The event
-- messages that carried an event in their metadata (0071) drop it, and the
-- index on it goes. The rows keep metadata.proactive = 'event', the shape of
-- every event message.
--
-- DROP INDEX takes ACCESS EXCLUSIVE on chat_messages, so 0061's lock_timeout
-- reasoning carries over: waiting behind a long transaction would hold live
-- chat blocked, and a timeout fails migrate before the new image serves, which
-- is the safe failure. The UPDATE runs first, so the exclusive lock is held
-- only from the DROP to the commit.

SET LOCAL lock_timeout = '3s';

UPDATE engine.chat_messages
   SET metadata = metadata - 'event' - 'event_key'
 WHERE metadata->>'proactive' = 'event'
   AND (metadata->'event' IS NOT NULL OR metadata->'event_key' IS NOT NULL);

DROP INDEX engine.chat_messages_event_key_uidx;

-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Life events live in engine.persona_events (0072;
-- docs/superpowers/specs/2026-10-15-life-events-v2-design.md §6.3). The event
-- messages that carried an event in their metadata (0071) drop it, and the
-- index on it goes. The rows keep metadata.proactive = 'event', the shape of
-- every event message.

UPDATE engine.chat_messages
   SET metadata = metadata - 'event' - 'event_key'
 WHERE metadata->>'proactive' = 'event'
   AND (metadata->'event' IS NOT NULL OR metadata->'event_key' IS NOT NULL);

DROP INDEX engine.chat_messages_event_key_uidx;

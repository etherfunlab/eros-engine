-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Life events (docs/superpowers/specs/2026-10-13-life-events-design.md §5.5).
--
-- An event is the proactive message that tells it. event_key is the caller's
-- idempotency key, unique per session. The index also serves the event
-- history and event-under-way reads, which filter on the same predicate.
-- event_key is not a reference, so there is no FK.

CREATE UNIQUE INDEX chat_messages_event_key_uidx
    ON engine.chat_messages (session_id, (metadata->>'event_key'))
    WHERE metadata->>'proactive' = 'event';

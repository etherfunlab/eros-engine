-- SPDX-License-Identifier: AGPL-3.0-only
--
-- A life choice's tier drop is an affinity event of its own non-turn type,
-- `life_choice` (docs/superpowers/specs/2026-10-19-life-events-shopping-
-- gatherings-and-ask-for-space-design.md §3.5). Widen 0014's event_type CHECK
-- by one value; every existing row still satisfies it.
--
-- ALTER TABLE takes ACCESS EXCLUSIVE on companion_affinity_events, so 0061's
-- lock_timeout reasoning carries over: a timeout fails migrate before the new
-- image serves, which is the safe failure.

SET LOCAL lock_timeout = '3s';

ALTER TABLE engine.companion_affinity_events
    DROP CONSTRAINT companion_affinity_events_event_type_check;
ALTER TABLE engine.companion_affinity_events
    ADD CONSTRAINT companion_affinity_events_event_type_check
    CHECK (event_type IN ('message', 'ghost', 'gift', 'proactive', 'time_decay', 'life_choice'));

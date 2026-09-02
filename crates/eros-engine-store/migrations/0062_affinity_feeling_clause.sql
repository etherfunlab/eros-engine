-- SPDX-License-Identifier: AGPL-3.0-only
-- Feeling clause (spec 2026-09-03-affinity-feeling-clause-design.md §3).
-- The session's LLM-written [feelings] narrative: one authoritative clause
-- per session, rewritten by the affinity_summary task on movement turns.
-- NULL = never summarized ⇒ the prompt block is omitted. feeling_clause_at
-- is the observability read (when it was last rewritten), not a scheduler
-- input. Derived state — history lives in companion_affinity_events and
-- the prompt log, so no audit table.
ALTER TABLE engine.companion_affinity
    ADD COLUMN feeling_clause    TEXT        NULL,
    ADD COLUMN feeling_clause_at TIMESTAMPTZ NULL;

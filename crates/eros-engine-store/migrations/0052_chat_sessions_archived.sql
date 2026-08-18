-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Session soft-delete (docs/superpowers/specs/2026-08-18-session-soft-delete-design.md).
--
-- A client that wants "clear this conversation and start over" previously had
-- only DELETE, and chat_sessions has three ON DELETE CASCADE children — so the
-- transcript went with it. This column separates the two: the conversation
-- stops being visible and stops being resumable, and the rows stay.
--
-- Boolean, not a status enum: a session is visible or it is not, and there is
-- no third state to name. (Contrast persona_instances.status, which carries a
-- genuine active/archived distinction over ownership.)
--
-- No index. The only hot path is resume, which reaches its candidates through
-- idx_chat_sessions_user_instance_channel and lands on a single-digit number of
-- rows; filtering NOT archived on the heap from there costs nothing worth an
-- index. Archived rows are read only by hand-written SQL.
--
-- No backfill. Sessions destroyed by earlier hard deletes are gone.
--
-- Reviving one session (operator action; deliberately NOT an endpoint):
--   UPDATE engine.chat_sessions SET archived = false WHERE id = '<session-uuid>';
-- The transcript comes back. Affinity, relationship-layer memories and
-- character insights do not — the archive endpoint deletes those on purpose, so
-- a revived session resumes with a cold relationship.

ALTER TABLE engine.chat_sessions
    ADD COLUMN archived BOOLEAN NOT NULL DEFAULT false;

-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Persona needs (docs/superpowers/specs/2026-10-13-persona-needs-design.md §5.3).
--
-- The judge's needs rating lives on the rated turn's user row as
-- metadata.needs. ChatRepo::latest_needs reads the newest such row across the
-- instance's sessions; this partial index keeps that an index probe per
-- session, including on instances whose recent turns carry no rating.

CREATE INDEX chat_messages_needs_idx
    ON engine.chat_messages (session_id, sent_at DESC)
    WHERE role = 'user' AND metadata ? 'needs';

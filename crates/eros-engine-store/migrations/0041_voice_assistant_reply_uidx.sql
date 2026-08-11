-- SPDX-License-Identifier: AGPL-3.0-only
--
-- One assistant reply per voice user turn.
--
-- Barge-in (spec 2026-08-11-voice-barge-in-interrupt-design.md) introduces a
-- second writer for the same row: the interrupt endpoint reports what TTS
-- actually played. The client aborts the SSE and then POSTs the interrupt, but
-- the server may not have processed the FIN yet, so `run_voice_turn`'s
-- generator can still be alive and later run its own insert. Without this index
-- that produces two assistant rows for one user turn.
--
-- With it the outcome is order-independent: whichever writer arrives second
-- takes the ON CONFLICT path, so `content` always ends up the interrupt's
-- report and the audit columns always the generator's.
--
-- Partial, because only voice replies are one-per-turn — the text pipeline
-- writes several assistant rows against a single user turn (multi-part replies
-- via `continues_from_message_id`), and `user_message_id` is NULL on user rows,
-- which a unique index ignores anyway.
--
-- Existing data should already satisfy this (voice writes exactly one reply per
-- turn today). If a deployment has duplicates this migration fails loudly
-- rather than silently dropping rows; find them with:
--   SELECT user_message_id FROM engine.chat_messages
--    WHERE role='assistant' AND channel='voice'
--    GROUP BY 1 HAVING count(*) > 1;

CREATE UNIQUE INDEX IF NOT EXISTS chat_messages_voice_assistant_reply_uidx
    ON engine.chat_messages (user_message_id)
    WHERE role = 'assistant' AND channel = 'voice';

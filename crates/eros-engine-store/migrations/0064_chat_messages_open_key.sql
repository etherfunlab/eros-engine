-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Caller-named openers on /open (spec 2026-10-03-open-occasions-design.md §6.3).
-- An opener is a proactive assistant row whose metadata carries the caller's
-- idempotency key, open_key. The partial unique index makes one opener per
-- session and key. Holiday greetings carry no open_key, so the expression is
-- NULL for them and never conflicts. The index is new and matches no existing
-- row, so there is no duplicate pre-check.

CREATE UNIQUE INDEX chat_messages_proactive_open_key_uidx
    ON engine.chat_messages (session_id, (metadata->>'open_key'))
    WHERE assistant_action_type = 'proactive';

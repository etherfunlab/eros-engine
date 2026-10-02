-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Holiday greeting (spec 2026-09-26-user-locale-and-holiday-greeting-design.md §7).
-- A persona-initiated assistant row carries assistant_action_type = 'proactive'
-- and user_message_id NULL. The partial unique index IS the "already greeted
-- today" fact: one greeting per session per user-local date, the date being
-- metadata.local_date (YYYY-MM-DD). The index is new and matches no existing
-- row, so there is no duplicate pre-check.
--
-- The 0012 CHECK was added inline (unnamed); drop it via catalog lookup rather
-- than a guessed name (cf. 0034), then re-add with an explicit name.

DO $$
DECLARE
    cname text;
BEGIN
    SELECT con.conname INTO cname
    FROM pg_constraint con
    JOIN pg_class rel ON rel.oid = con.conrelid
    JOIN pg_namespace nsp ON nsp.oid = rel.relnamespace
    WHERE nsp.nspname = 'engine'
      AND rel.relname = 'chat_messages'
      AND con.contype = 'c'
      AND pg_get_constraintdef(con.oid) ILIKE '%assistant_action_type%';
    IF cname IS NOT NULL THEN
        EXECUTE format(
            'ALTER TABLE engine.chat_messages DROP CONSTRAINT %I',
            cname
        );
    END IF;
END $$;

ALTER TABLE engine.chat_messages
    ADD CONSTRAINT chat_messages_assistant_action_type_check
    CHECK (assistant_action_type IS NULL
           OR assistant_action_type IN ('reply', 'gift_reaction', 'proactive'));

CREATE UNIQUE INDEX chat_messages_proactive_day_uidx
    ON engine.chat_messages (session_id, (metadata->>'local_date'))
    WHERE assistant_action_type = 'proactive';

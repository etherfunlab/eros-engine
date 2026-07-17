-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Extend engine.chat_messages.channel to accept 'product_qa': the marker for
-- out-of-character product answers (PDE action product_qa). Non-NULL channel
-- rows are invisible to the companion brain (context/signals/dreaming) and
-- fully visible to the client. See
-- docs/superpowers/specs/2026-07-17-pde-product-qa-design.md.
--
-- The 0030 CHECK was added inline (unnamed); drop it via catalog lookup
-- rather than a guessed name (cf. 0014), then re-add with an explicit name.

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
      AND pg_get_constraintdef(con.oid) ILIKE '%channel%';
    IF cname IS NOT NULL THEN
        EXECUTE format(
            'ALTER TABLE engine.chat_messages DROP CONSTRAINT %I',
            cname
        );
    END IF;
END $$;

ALTER TABLE engine.chat_messages
    ADD CONSTRAINT chat_messages_channel_check
    CHECK (channel IS NULL OR channel IN ('voice', 'product_qa'));

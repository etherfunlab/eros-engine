-- SPDX-License-Identifier: AGPL-3.0-only
--
-- `image_edit` joins the composer audit's source vocabulary.
--
-- POST /v2/comp/session/{session_id}/message/{message_id}/image/edit runs the
-- same composer against a different payload — the source picture's subject plus
-- an edit instruction — so its calls belong in the same table. Its `inputs`
-- carries seven keys rather than the chat composer's five; `source` says which
-- shape to expect.
--
-- The old CHECK is found by column, not by name or by definition text: 0045
-- never named it, so there's no fixed name to target, and matching the
-- definition text (e.g. ILIKE '%source%') would also catch any future CHECK
-- that merely mentions the word "source" in its expression — and `SELECT ...
-- INTO` below takes the first match silently rather than erroring on more
-- than one. Matching con.conkey against `source`'s attnum instead pins the
-- lookup to "the CHECK whose column set is exactly {source}", which is
-- unambiguous regardless of constraint name or expression wording.

DO $$
DECLARE
    cname text;
BEGIN
    SELECT con.conname INTO cname
    FROM pg_constraint con
    JOIN pg_class rel ON rel.oid = con.conrelid
    JOIN pg_namespace nsp ON nsp.oid = rel.relnamespace
    JOIN pg_attribute att ON att.attrelid = rel.oid AND att.attname = 'source'
    WHERE nsp.nspname = 'engine'
      AND rel.relname = 'chat_images_events'
      AND con.contype = 'c'
      AND con.conkey = ARRAY[att.attnum];
    IF cname IS NOT NULL THEN
        EXECUTE format(
            'ALTER TABLE engine.chat_images_events DROP CONSTRAINT %I',
            cname
        );
    END IF;
END $$;

ALTER TABLE engine.chat_images_events
    ADD CONSTRAINT chat_images_events_source_check
    CHECK (source IN (
        'chat_reply_text_image', 'chat_reply_image',
        'compose_endpoint', 'compose_endpoint_stream',
        'image_edit'
    ));

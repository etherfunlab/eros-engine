-- SPDX-License-Identifier: AGPL-3.0-only
--
-- Persona items and wardrobe (docs/superpowers/specs/2026-10-12-persona-items-and-gift-design.md).
--
-- One row per thing an instance owns or can wear. Genome presets are not
-- copied here: they stay in persona_genomes.art_metadata and are merged at
-- read time. A dependent table, so instance_id cascades. user_message_id is
-- the gift turn; caller-written rows have none, and the item outlives a
-- deleted message, hence SET NULL. source is stored because "she was given
-- this" is not derivable once that pointer is gone. The partial unique index
-- makes the gift write idempotent and serves as the FK's index.

CREATE TABLE engine.persona_instance_items (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_id     UUID NOT NULL
                    REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL CHECK (kind IN ('item', 'clothing')),
    name            TEXT NOT NULL,
    source          TEXT NOT NULL CHECK (source IN ('gift', 'caller')),
    user_message_id UUID
                    REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX persona_instance_items_instance_idx
    ON engine.persona_instance_items (instance_id, created_at);
CREATE UNIQUE INDEX persona_instance_items_user_message_uidx
    ON engine.persona_instance_items (user_message_id)
    WHERE user_message_id IS NOT NULL;

-- Supabase lockdown, mirroring 0059. REVOKEs are guarded by pg_roles existence
-- so non-Supabase Postgres (incl. the sqlx test DB) skips them silently. RLS is
-- enabled unconditionally; with no policy, only owner/service_role can touch it.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'anon') THEN
        REVOKE ALL ON engine.persona_instance_items FROM anon;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'authenticated') THEN
        REVOKE ALL ON engine.persona_instance_items FROM authenticated;
    END IF;
END
$$;

ALTER TABLE engine.persona_instance_items ENABLE ROW LEVEL SECURITY;

-- gift_reaction has had no writer since the gift event was removed. Rows an
-- older version wrote become ordinary replies so the narrower CHECK validates
-- on every deployment.
UPDATE engine.chat_messages SET assistant_action_type = 'reply'
 WHERE assistant_action_type = 'gift_reaction';

ALTER TABLE engine.chat_messages
    DROP CONSTRAINT chat_messages_assistant_action_type_check;
ALTER TABLE engine.chat_messages
    ADD CONSTRAINT chat_messages_assistant_action_type_check
    CHECK (assistant_action_type IS NULL
           OR assistant_action_type IN ('reply', 'proactive'));

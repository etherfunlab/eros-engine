-- SPDX-License-Identifier: AGPL-3.0-only
CREATE TABLE persona_genomes (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name             TEXT NOT NULL,
    system_prompt    TEXT NOT NULL,
    tip_personality  TEXT,
    avatar_url       TEXT,
    art_metadata     JSONB NOT NULL DEFAULT '{}',
    is_active        BOOLEAN NOT NULL DEFAULT true,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE persona_instances (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    genome_id   UUID NOT NULL REFERENCES persona_genomes(id) ON DELETE RESTRICT,
    owner_uid   UUID NOT NULL,
    status      TEXT NOT NULL DEFAULT 'active',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(genome_id, owner_uid)
);
CREATE INDEX idx_persona_instances_owner ON persona_instances(owner_uid);

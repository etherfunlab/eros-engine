-- SPDX-License-Identifier: AGPL-3.0-only
--
-- A persona instance is either a genome instance (genome_id set) or a BYOC
-- instance (byoc_key set, genome_id NULL), never both and never neither
-- (docs/superpowers/specs/2026-10-16-byoc-design.md §7.1). The key is 32 bytes,
-- unique among BYOC instances. UNIQUE (genome_id, owner_uid) treats NULLs as
-- distinct, so one owner may hold any number of BYOC instances.
--
-- ALTER TABLE takes ACCESS EXCLUSIVE on persona_instances, so 0061's
-- lock_timeout reasoning carries over: a timeout fails migrate before the new
-- image serves, which is the safe failure.

SET LOCAL lock_timeout = '3s';

ALTER TABLE engine.persona_instances ALTER COLUMN genome_id DROP NOT NULL;
ALTER TABLE engine.persona_instances ADD COLUMN byoc_key BYTEA;
ALTER TABLE engine.persona_instances
    ADD CONSTRAINT persona_instances_kind_chk
    CHECK ((genome_id IS NULL) <> (byoc_key IS NULL));
ALTER TABLE engine.persona_instances
    ADD CONSTRAINT persona_instances_byoc_key_len_chk
    CHECK (byoc_key IS NULL OR octet_length(byoc_key) = 32);
CREATE UNIQUE INDEX persona_instances_byoc_key_uniq
    ON engine.persona_instances (byoc_key) WHERE byoc_key IS NOT NULL;

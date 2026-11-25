-- Fact groups (spec 2026-11-25-fact-groups-design.md).
--
-- engine.human_facts.group_id: the group a ledger fact belongs to. An opaque
-- token with no table behind it, so no foreign key; a group is the set of the
-- user's rows sharing one. Every row starts as its own one-row group (the
-- volatile default gives each existing row a fresh id too) and moves only
-- when the fact-group sweeper places it into an earlier fact's group.
--
-- engine.human_facts.embedding: the fact's vector, NULL until the sweeper
-- places the row. The vector and the placement are written together, so
-- `embedding IS NOT NULL` means "placed". The partial index is the sweeper's
-- queue, oldest first.

ALTER TABLE engine.human_facts
    ADD COLUMN group_id  UUID NOT NULL DEFAULT gen_random_uuid(),
    ADD COLUMN embedding vector(512);

CREATE INDEX idx_human_facts_unplaced
    ON engine.human_facts (first_seen_at, id) WHERE embedding IS NULL;

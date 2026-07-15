-- SPDX-License-Identifier: AGPL-3.0-only
-- Opaque per-memory extraction metadata (domain / evidence_type / temporality /
-- persistence / confidence — vocabulary owned by the memory_extraction prompt,
-- never enforced by the engine). NULL = extractor supplied none (raw-turn rows,
-- relationship rows, old-prompt deployments). Nullable ADD COLUMN = metadata-only
-- change, no table rewrite. `category` deliberately stays a first-class column:
-- it is the recall grouping key (search_profile_grouped PARTITION BY category).
ALTER TABLE engine.companion_memories
    ADD COLUMN metadata JSONB;

-- SPDX-License-Identifier: AGPL-3.0-only
-- Keyset index for the memories export endpoint (GET /svc/v1/memories/changes).
-- The export walks the extracted profile-layer rows in (created_at, id) order;
-- the partial predicate is the export's own filter. Index only, no data change;
-- the table's lockdown (0013) already covers it.
CREATE INDEX companion_memories_profile_created_idx
    ON engine.companion_memories (created_at, id)
    WHERE instance_id IS NULL AND category IS NOT NULL;

-- SPDX-License-Identifier: AGPL-3.0-only
-- Keyset index for the insights export endpoint (GET /svc/v1/insights/changes).
-- The export walks human_insights in (updated_at, user_id) order; without this
-- index every page is a full scan + sort. Index only — no data change; the
-- table's lockdown (0015) already covers it.
CREATE INDEX idx_human_insights_updated_user
    ON engine.human_insights (updated_at, user_id);

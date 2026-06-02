-- SPDX-License-Identifier: AGPL-3.0-only
-- Adds the geographic identity fields to the flat human_insights mirror.
-- city already exists (0015); location/hometown/nationality are new. Existing
-- table grants + RLS (0015) carry over to the new columns — no lockdown block.
-- No backfill: companion_insights JSONB has no geo data yet; rows repopulate on
-- the next insight_extraction run.
--
-- Spec: docs/superpowers/specs/2026-06-03-extraction-geo-and-config-prompts-design.md

ALTER TABLE engine.human_insights
    ADD COLUMN location    TEXT,
    ADD COLUMN hometown    TEXT,
    ADD COLUMN nationality TEXT;

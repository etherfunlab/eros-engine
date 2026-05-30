-- SPDX-License-Identifier: AGPL-3.0-only
-- DESTRUCTIVE. v0.5.1 BREAKING. Removes the entire NFT-ownership mirror.
-- engine no longer gates on wallet/asset ownership; user→wallet binding is
-- a downstream concern. The 0013 supabase_lockdown REVOKE/RLS statements
-- that referenced these tables become no-ops once the tables are gone.
--
-- Spec: docs/superpowers/specs/2026-05-29-engine-cleanup-and-snapshot-design.md §1

DROP TABLE IF EXISTS engine.wallet_links;
DROP TABLE IF EXISTS engine.persona_ownership;
DROP TABLE IF EXISTS engine.sync_cursors;
ALTER TABLE engine.persona_genomes DROP COLUMN IF EXISTS asset_id;

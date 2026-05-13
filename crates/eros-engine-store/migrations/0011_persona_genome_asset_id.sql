-- SPDX-License-Identifier: AGPL-3.0-only
-- engine.persona_genomes gains a nullable asset_id pointing at the cNFT
-- whose ownership gates chat access. Legacy seed-persona rows keep
-- asset_id=NULL and are exempt from the NFT gate at chat-start /
-- per-message time.
--
-- The partial UNIQUE index enforces "1 asset = 1 genome" without blocking
-- the legacy NULL rows from coexisting.

ALTER TABLE engine.persona_genomes
    ADD COLUMN asset_id TEXT NULL;

CREATE UNIQUE INDEX persona_genomes_asset_id_uidx
    ON engine.persona_genomes (asset_id)
    WHERE asset_id IS NOT NULL;

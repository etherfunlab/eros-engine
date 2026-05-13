-- SPDX-License-Identifier: AGPL-3.0-only
-- engine.persona_ownership — mirror of marketplace-svc's authoritative
-- "who owns which cNFT" view. The chat-start gate joins this table with
-- engine.wallet_links to decide access for NFT-backed genomes.
--
-- source_updated_at carries svc's event time and powers two things:
--   1. Stale-write protection (ON CONFLICT … WHERE incoming > existing).
--   2. Cursor pagination via the (source_updated_at, asset_id) compound key.

CREATE TABLE engine.persona_ownership (
    asset_id           TEXT         PRIMARY KEY,
    persona_id         TEXT         NOT NULL,
    owner_wallet       TEXT         NOT NULL,
    source_updated_at  TIMESTAMPTZ  NOT NULL,
    updated_at         TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE INDEX persona_ownership_owner_wallet_idx
    ON engine.persona_ownership (owner_wallet);
CREATE INDEX persona_ownership_persona_id_idx
    ON engine.persona_ownership (persona_id);
CREATE INDEX persona_ownership_source_updated_at_idx
    ON engine.persona_ownership (source_updated_at, asset_id);

-- engine.sync_cursors — one row per replicated entity. Storing both halves
-- of the (source_updated_at, pk) compound cursor avoids the
-- same-timestamp page-boundary bug.

CREATE TABLE engine.sync_cursors (
    name        TEXT         PRIMARY KEY,    -- 'ownership' | 'wallets'
    cursor_ts   TIMESTAMPTZ  NOT NULL,
    cursor_pk   TEXT         NOT NULL,
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT now()
);

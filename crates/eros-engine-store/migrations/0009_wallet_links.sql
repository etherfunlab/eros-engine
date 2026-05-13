-- SPDX-License-Identifier: AGPL-3.0-only
-- engine.wallet_links — mirror of marketplace-svc's wallet ↔ user bindings.
--
-- Tombstone on unlink (linked=false), never DELETE. The `since` cursor
-- self-heal pull cannot represent deletion as the absence of a row, so we
-- keep tombstones flowing through the same cursor as inserts and updates.
--
-- Partial UNIQUE on wallet_pubkey WHERE linked=true enforces "one wallet
-- bound to at most one user at a time," allowing later re-binding to a
-- different user after unlink. svc enforces the same invariant on its side.

CREATE TABLE engine.wallet_links (
    user_id            UUID         NOT NULL,
    wallet_pubkey      TEXT         NOT NULL,
    linked             BOOLEAN      NOT NULL DEFAULT true,
    linked_at          TIMESTAMPTZ  NOT NULL DEFAULT now(),
    source_updated_at  TIMESTAMPTZ  NOT NULL,
    updated_at         TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, wallet_pubkey)
);

CREATE UNIQUE INDEX wallet_links_active_pubkey_uidx
    ON engine.wallet_links (wallet_pubkey)
    WHERE linked = true;

-- Compound index for cursor pagination: ORDER BY source_updated_at ASC,
-- then by (user_id, wallet_pubkey) as a stable tie-breaker.
CREATE INDEX wallet_links_source_updated_at_idx
    ON engine.wallet_links (source_updated_at, user_id, wallet_pubkey);

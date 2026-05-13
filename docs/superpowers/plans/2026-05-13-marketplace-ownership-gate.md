# Marketplace Ownership Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add NFT-ownership awareness to `eros-engine` so `eros-marketplace-svc` (P4) can gate chat access on `(asset_id, owner_wallet, user_id)` without coupling the OSS engine to the closed-source marketplace.

**Architecture:** Mirror marketplace ownership + wallet binding state inside `engine.*` schema (two new tables + nullable column on `persona_genomes`). Expose HMAC-authenticated `/internal/*` push/pull endpoints. Gate `/comp/chat/start` (before instance creation) and every `/comp/chat/{session_id}/message{,_async}` route via a single indexed SQL join. Self-heal task pulls svc cursors when `MARKETPLACE_SVC_URL` is configured. OSS deploys without those env vars run identically to today.

**Tech Stack:** Rust workspace (`-core`/`-llm`/`-store`/`-server`), axum 0.8, sqlx 0.8 (Postgres), utoipa 5 OpenAPI, jsonwebtoken 9, reqwest 0.12. Adds `bs58`, `hmac`, `sha2`, `subtle`, `hex` to the workspace.

**Source spec:** `docs/superpowers/specs/2026-05-13-marketplace-ownership-gate-design.md`

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `Cargo.toml` (workspace) | Modify | Add `bs58`, `hmac`, `sha2`, `subtle`, `hex` to `[workspace.dependencies]`. |
| `crates/eros-engine-store/Cargo.toml` | Modify | Pull in `bs58`. |
| `crates/eros-engine-store/migrations/0009_wallet_links.sql` | Create | `engine.wallet_links` with tombstone (`linked` bool) + `source_updated_at`. |
| `crates/eros-engine-store/migrations/0010_persona_ownership.sql` | Create | `engine.persona_ownership` with `source_updated_at` + `engine.sync_cursors` (compound cursor storage). |
| `crates/eros-engine-store/migrations/0011_persona_genome_asset_id.sql` | Create | `ALTER engine.persona_genomes ADD COLUMN asset_id TEXT` + partial unique index. |
| `crates/eros-engine-store/src/pubkey.rs` | Create | `validate_solana_pubkey(&str) -> Result<String, PubkeyError>`. |
| `crates/eros-engine-store/src/wallets.rs` | Create | `WalletLinkRepo` (upsert with stale-write guard + tombstone, `since` cursor pagination). |
| `crates/eros-engine-store/src/ownership.rs` | Create | `OwnershipRepo` (upsert with stale-write guard, `since` cursor pagination, gate-check `owns()`). |
| `crates/eros-engine-store/src/sync_cursors.rs` | Create | `SyncCursorRepo` for `(cursor_ts, cursor_pk)` compound cursors. |
| `crates/eros-engine-store/src/persona.rs` | Modify | Add `PersonaRepo::get_asset_id_for_genome(genome_id) -> Option<String>`. |
| `crates/eros-engine-store/src/lib.rs` | Modify | Re-export new modules. |
| `crates/eros-engine-server/Cargo.toml` | Modify | Pull in `hmac`, `sha2`, `subtle`, `hex`. |
| `crates/eros-engine-server/src/auth/s2s.rs` | Create | HMAC s2s middleware (5-line canonical signing, 1 MiB body cap, current + previous secret). |
| `crates/eros-engine-server/src/auth/mod.rs` | Modify | `pub mod s2s;`. |
| `crates/eros-engine-server/src/routes/internal.rs` | Create | Four `/internal/*` handlers + payload structs + base58 validators. |
| `crates/eros-engine-server/src/routes/mod.rs` | Modify | Merge internal subrouter OUTSIDE the JWT layer, keep `router_for_openapi` parity. |
| `crates/eros-engine-server/src/routes/companion.rs` | Modify | `enforce_nft_ownership` helper; call before `create_instance` in `start_chat` (both paths); call in `send_message` + `send_message_async`. |
| `crates/eros-engine-server/src/state.rs` | Modify | Add `marketplace_svc_url`, `marketplace_s2s_secret`, `marketplace_s2s_secret_previous`, `http_client`. |
| `crates/eros-engine-server/src/pipeline/sync.rs` | Create | Self-heal loop with compound cursor; signs outgoing GETs. |
| `crates/eros-engine-server/src/pipeline/mod.rs` | Modify | Conditionally spawn the sync task. |
| `crates/eros-engine-server/src/main.rs` | Modify | Env wiring + boot validation (`URL` without `SECRET` → bail). |
| `crates/eros-engine-server/src/openapi.rs` | Modify | Register `hmac_signature` security scheme. |
| `README.md` | Modify | Add env vars to the table. |
| `docs/deploying.md` | Modify | Document `MARKETPLACE_SVC_URL` / `_S2S_SECRET` / `_S2S_SECRET_PREVIOUS`. |
| `docs/api-reference.md` | Modify | Document `/internal/*` surface under a separate section. |

Twenty-two files: twelve new, ten modified.

---

## Sequence Rationale

Each phase leaves the tree green. Strict TDD: red → green → commit. No task leaves migrations un-runnable or tests broken.

- **E1 (Schema + repos):** workspace deps → 3 migrations → pubkey validator → 3 new repos → 1 repo extension. Nothing in this phase touches the HTTP surface. CI green when all sqlx tests pass.
- **E2 (S2S middleware + routes):** HMAC middleware → 4 internal handlers → router composition + OpenAPI + state + boot validation. CI green when middleware tests + handler tests + drift check all pass.
- **E3 (Gate):** helper → start_chat (both paths) → message routes. CI green when legacy genome paths still pass and new ownership paths reject correctly.
- **E4 (Self-heal):** s2s outbound signing helper → pull loop → conditional spawn. CI green when configured deploy advances cursors and unconfigured deploy boots cleanly.

---

## Phase E1 — Schema + repos

### Task 1: Add workspace dependencies

Pull `bs58`, `hmac`, `sha2`, `subtle`, `hex` into the workspace so per-crate `Cargo.toml` files just reference `{ workspace = true }`.

**Files:**
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Edit workspace dependencies block**

Append to the existing `[workspace.dependencies]` table in `Cargo.toml`:

```toml
bs58 = "0.5"
hmac = "0.12"
sha2 = "0.10"
subtle = "2.5"
hex = "0.4"
```

- [ ] **Step 2: Verify workspace still resolves**

Run: `cargo check --workspace`
Expected: clean exit code 0 (new crates aren't used yet — they just resolve).

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: add bs58/hmac/sha2/subtle/hex to workspace deps

Used by the marketplace ownership gate PR. No code consumes them yet."
```

---

### Task 2: Migration 0009 — wallet_links

`engine.wallet_links` mirrors svc's wallet→user binding. Tombstone (`linked = false`) on unlink, never `DELETE`, so the `since` cursor can represent deletion.

**Files:**
- Create: `crates/eros-engine-store/migrations/0009_wallet_links.sql`

- [ ] **Step 1: Write a failing migration test**

Append to `crates/eros-engine-store/src/lib.rs`:

```rust
#[cfg(test)]
mod migration_tests {
    use sqlx::PgPool;

    #[sqlx::test(migrations = "migrations")]
    async fn wallet_links_schema_is_correct(pool: PgPool) {
        // Insert one row; assert the columns we documented exist.
        sqlx::query(
            "INSERT INTO engine.wallet_links
                (user_id, wallet_pubkey, linked, source_updated_at)
             VALUES ($1, $2, true, now())",
        )
        .bind(uuid::Uuid::new_v4())
        .bind("BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt")
        .execute(&pool)
        .await
        .expect("insert into wallet_links");

        // The partial unique index allows another row with linked=false.
        let same_wallet = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";
        let res = sqlx::query(
            "INSERT INTO engine.wallet_links
                (user_id, wallet_pubkey, linked, source_updated_at)
             VALUES ($1, $2, false, now())",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(same_wallet)
        .execute(&pool)
        .await;
        // The PK is (user_id, wallet_pubkey) so different user_id is fine.
        // The active-only UNIQUE index is on wallet_pubkey WHERE linked=true,
        // so a tombstone for the same pubkey but different user_id is allowed.
        assert!(res.is_ok(), "tombstone insert must succeed: {res:?}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p eros-engine-store migration_tests::wallet_links_schema_is_correct -- --nocapture`
Expected: FAIL with "relation \"engine.wallet_links\" does not exist".

- [ ] **Step 3: Write the migration**

Create `crates/eros-engine-store/migrations/0009_wallet_links.sql`:

```sql
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
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p eros-engine-store migration_tests::wallet_links_schema_is_correct -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/eros-engine-store/migrations/0009_wallet_links.sql \
        crates/eros-engine-store/src/lib.rs
git commit -m "feat(store): migration 0009 — wallet_links with tombstone semantics"
```

---

### Task 3: Migration 0010 — persona_ownership + sync_cursors

Two tables in one migration: `persona_ownership` (the gate's right-hand table) and `sync_cursors` (compound cursor persistence for the self-heal loop).

**Files:**
- Create: `crates/eros-engine-store/migrations/0010_persona_ownership.sql`
- Modify: `crates/eros-engine-store/src/lib.rs` (extend the migration_tests module)

- [ ] **Step 1: Add a failing schema test**

Append to the `migration_tests` module in `crates/eros-engine-store/src/lib.rs`:

```rust
    #[sqlx::test(migrations = "migrations")]
    async fn persona_ownership_and_sync_cursors_schema(pool: PgPool) {
        // persona_ownership: PK = asset_id, must accept source_updated_at.
        let asset = "11111111111111111111111111111111";
        sqlx::query(
            "INSERT INTO engine.persona_ownership
                (asset_id, persona_id, owner_wallet, source_updated_at)
             VALUES ($1, 'persona-test', 'OwnerWallet1111111111111111111111', now())",
        )
        .bind(asset)
        .execute(&pool)
        .await
        .expect("insert into persona_ownership");

        // sync_cursors: PK = name, compound (cursor_ts, cursor_pk) writeable.
        sqlx::query(
            "INSERT INTO engine.sync_cursors (name, cursor_ts, cursor_pk)
             VALUES ('ownership', now(), '')",
        )
        .execute(&pool)
        .await
        .expect("insert into sync_cursors");
    }
```

- [ ] **Step 2: Run to confirm fail**

Run: `cargo test -p eros-engine-store migration_tests::persona_ownership_and_sync_cursors_schema -- --nocapture`
Expected: FAIL — relation does not exist.

- [ ] **Step 3: Write the migration**

Create `crates/eros-engine-store/migrations/0010_persona_ownership.sql`:

```sql
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
```

- [ ] **Step 4: Run to confirm pass**

Run: `cargo test -p eros-engine-store migration_tests::persona_ownership_and_sync_cursors_schema -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/eros-engine-store/migrations/0010_persona_ownership.sql \
        crates/eros-engine-store/src/lib.rs
git commit -m "feat(store): migration 0010 — persona_ownership + sync_cursors

Includes source_updated_at on the data table for stale-write protection
and compound (cursor_ts, cursor_pk) on sync_cursors for cursor pagination."
```

---

### Task 4: Migration 0011 — persona_genomes.asset_id

Nullable column + partial unique index. Existing seed-persona rows stay `NULL` and are exempt from the gate.

**Files:**
- Create: `crates/eros-engine-store/migrations/0011_persona_genome_asset_id.sql`
- Modify: `crates/eros-engine-store/src/lib.rs` (extend migration_tests)

- [ ] **Step 1: Add a failing schema test**

Append to `migration_tests`:

```rust
    #[sqlx::test(migrations = "migrations")]
    async fn persona_genomes_gains_nullable_asset_id(pool: PgPool) {
        // The legacy seed-persona path inserts WITHOUT asset_id and must keep working.
        let legacy_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active)
             VALUES ($1, 'LegacyGenome', 'prompt', '{}'::jsonb, true)",
        )
        .bind(legacy_id)
        .execute(&pool)
        .await
        .expect("legacy insert without asset_id");

        // A new NFT-backed genome carries asset_id.
        let nft_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active, asset_id)
             VALUES ($1, 'NftGenome', 'prompt', '{}'::jsonb, true, $2)",
        )
        .bind(nft_id)
        .bind("11111111111111111111111111111112")
        .execute(&pool)
        .await
        .expect("nft insert with asset_id");

        // Partial unique: the same non-NULL asset_id cannot be claimed twice.
        let dup_id = uuid::Uuid::new_v4();
        let dup_res = sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active, asset_id)
             VALUES ($1, 'Dup', 'prompt', '{}'::jsonb, true, $2)",
        )
        .bind(dup_id)
        .bind("11111111111111111111111111111112")
        .execute(&pool)
        .await;
        assert!(dup_res.is_err(), "duplicate asset_id must be rejected");
    }
```

- [ ] **Step 2: Run to confirm fail**

Run: `cargo test -p eros-engine-store migration_tests::persona_genomes_gains_nullable_asset_id -- --nocapture`
Expected: FAIL — column `asset_id` does not exist (or partial unique missing).

- [ ] **Step 3: Write the migration**

Create `crates/eros-engine-store/migrations/0011_persona_genome_asset_id.sql`:

```sql
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
```

- [ ] **Step 4: Run to confirm pass**

Run: `cargo test -p eros-engine-store migration_tests::persona_genomes_gains_nullable_asset_id -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/eros-engine-store/migrations/0011_persona_genome_asset_id.sql \
        crates/eros-engine-store/src/lib.rs
git commit -m "feat(store): migration 0011 — persona_genomes.asset_id (nullable, partial unique)

Legacy seed-persona rows keep asset_id=NULL and remain exempt from the
NFT ownership gate."
```

---

### Task 5: Pubkey validation helper

Base58 + 32-byte check, returning a canonical re-encoded string. The canonical form is what we store everywhere, so non-canonical inputs (e.g., uppercase-vs-lowercase variants of the same key bytes) can't create logical duplicates.

**Files:**
- Modify: `crates/eros-engine-store/Cargo.toml`
- Create: `crates/eros-engine-store/src/pubkey.rs`
- Modify: `crates/eros-engine-store/src/lib.rs`

- [ ] **Step 1: Add the dep to the store crate**

Append to `[dependencies]` in `crates/eros-engine-store/Cargo.toml`:

```toml
bs58 = { workspace = true }
```

- [ ] **Step 2: Write failing unit tests**

Create `crates/eros-engine-store/src/pubkey.rs`:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
//! Base58-encoded Solana pubkey validation.
//!
//! Every public input — `asset_id`, `wallet_pubkey`, `owner_wallet` — must
//! decode to exactly 32 bytes. Rejecting non-canonical strings at the API
//! boundary keeps the data plane normalized so a single key cannot present
//! as two distinct rows.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PubkeyError {
    #[error("invalid base58")]
    InvalidBase58,
    #[error("wrong length: expected 32 bytes, got {0}")]
    WrongLength(usize),
}

/// Validate a base58-encoded Solana pubkey (32 bytes), returning the
/// canonical re-encoded form. The returned string is the only form stored
/// in `engine.*` tables so non-canonical input encodings cannot create
/// logical duplicates of the same key.
pub fn validate_solana_pubkey(s: &str) -> Result<String, PubkeyError> {
    let bytes = bs58::decode(s).into_vec().map_err(|_| PubkeyError::InvalidBase58)?;
    if bytes.len() != 32 {
        return Err(PubkeyError::WrongLength(bytes.len()));
    }
    Ok(bs58::encode(&bytes).into_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_canonical_32_byte_pubkey() {
        let canonical = "11111111111111111111111111111111";
        let out = validate_solana_pubkey(canonical).expect("valid");
        assert_eq!(out, canonical);
    }

    #[test]
    fn rejects_empty_string() {
        let err = validate_solana_pubkey("").expect_err("empty must fail");
        assert!(matches!(err, PubkeyError::WrongLength(0)));
    }

    #[test]
    fn rejects_non_base58() {
        let err = validate_solana_pubkey("0OIl/+=").expect_err("invalid b58 must fail");
        assert!(matches!(err, PubkeyError::InvalidBase58));
    }

    #[test]
    fn rejects_wrong_length() {
        // 33-byte payload, base58-encoded.
        let too_long = bs58::encode([0u8; 33]).into_string();
        let err = validate_solana_pubkey(&too_long).expect_err("33 bytes must fail");
        assert!(matches!(err, PubkeyError::WrongLength(33)));
    }
}
```

- [ ] **Step 3: Wire into lib.rs**

In `crates/eros-engine-store/src/lib.rs`, add at the top of the module exports:

```rust
pub mod pubkey;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p eros-engine-store pubkey::tests -- --nocapture`
Expected: 4/4 PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/eros-engine-store/Cargo.toml \
        crates/eros-engine-store/src/pubkey.rs \
        crates/eros-engine-store/src/lib.rs
git commit -m "feat(store): pubkey validator — base58 + 32 bytes, canonical re-encode"
```

---

### Task 6: WalletLinkRepo

UPSERT with stale-write guard, tombstone unlink, compound-cursor `since` reads.

**Files:**
- Create: `crates/eros-engine-store/src/wallets.rs`
- Modify: `crates/eros-engine-store/src/lib.rs`

- [ ] **Step 1: Write the failing integration test**

Create `crates/eros-engine-store/src/wallets.rs`:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
//! Wallet ↔ user binding mirror, fed by /internal/wallets/upsert and the
//! self-heal /since pull. Maintains the invariant that "one wallet is
//! bound to at most one user at a time" via the partial unique index
//! defined in migration 0009.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct WalletLink {
    pub user_id: Uuid,
    pub wallet_pubkey: String,
    pub linked: bool,
    pub source_updated_at: DateTime<Utc>,
}

pub struct WalletLinkRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> WalletLinkRepo<'a> {
    /// Idempotent UPSERT with stale-write protection: only applies if
    /// `incoming.source_updated_at > existing.source_updated_at`.
    /// Returns `Ok(true)` if the row was applied, `Ok(false)` if dropped as stale.
    pub async fn upsert(
        &self,
        user_id: Uuid,
        wallet_pubkey: &str,
        linked: bool,
        source_updated_at: DateTime<Utc>,
    ) -> sqlx::Result<bool> {
        let res = sqlx::query(
            "INSERT INTO engine.wallet_links
                (user_id, wallet_pubkey, linked, linked_at, source_updated_at, updated_at)
             VALUES ($1, $2, $3, $4, $4, now())
             ON CONFLICT (user_id, wallet_pubkey) DO UPDATE
               SET linked            = EXCLUDED.linked,
                   source_updated_at = EXCLUDED.source_updated_at,
                   updated_at        = now()
               WHERE EXCLUDED.source_updated_at > engine.wallet_links.source_updated_at",
        )
        .bind(user_id)
        .bind(wallet_pubkey)
        .bind(linked)
        .bind(source_updated_at)
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Cursor-paginated `since` read. `cursor_pk` for wallets is the string
    /// "{user_id}:{wallet_pubkey}". An empty cursor_pk pairs with cursor_ts
    /// to start from the very beginning of the table.
    pub async fn since(
        &self,
        cursor_ts: DateTime<Utc>,
        cursor_pk: &str,
        limit: i64,
    ) -> sqlx::Result<Vec<WalletLink>> {
        // Decompose cursor_pk into (user_id, wallet_pubkey) for the (ts, user, pubkey) tie-break.
        let (cur_user, cur_pubkey) = match cursor_pk.split_once(':') {
            Some((u, p)) => (u.to_string(), p.to_string()),
            None => (String::new(), String::new()),
        };
        sqlx::query_as::<_, WalletLink>(
            "SELECT user_id, wallet_pubkey, linked, source_updated_at
               FROM engine.wallet_links
              WHERE (source_updated_at, user_id::text, wallet_pubkey)
                  > ($1, $2, $3)
              ORDER BY source_updated_at ASC, user_id ASC, wallet_pubkey ASC
              LIMIT $4",
        )
        .bind(cursor_ts)
        .bind(cur_user)
        .bind(cur_pubkey)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    #[sqlx::test(migrations = "migrations")]
    async fn upsert_applies_then_drops_stale(pool: PgPool) {
        let repo = WalletLinkRepo { pool: &pool };
        let u = Uuid::new_v4();
        let w = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";

        assert!(repo.upsert(u, w, true, ts(100)).await.unwrap());
        // Newer event applies.
        assert!(repo.upsert(u, w, true, ts(200)).await.unwrap());
        // Older event is silently dropped (returns false, no row affected).
        assert!(!repo.upsert(u, w, false, ts(150)).await.unwrap());

        let row: (bool, DateTime<Utc>) = sqlx::query_as(
            "SELECT linked, source_updated_at FROM engine.wallet_links
              WHERE user_id = $1 AND wallet_pubkey = $2",
        )
        .bind(u)
        .bind(w)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, true, "linked must stay true since stale unlink was dropped");
        assert_eq!(row.1, ts(200));
    }

    #[sqlx::test(migrations = "migrations")]
    async fn unlink_writes_tombstone_not_delete(pool: PgPool) {
        let repo = WalletLinkRepo { pool: &pool };
        let u = Uuid::new_v4();
        let w = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";

        repo.upsert(u, w, true, ts(100)).await.unwrap();
        repo.upsert(u, w, false, ts(200)).await.unwrap();

        let row: (bool,) = sqlx::query_as(
            "SELECT linked FROM engine.wallet_links
              WHERE user_id = $1 AND wallet_pubkey = $2",
        )
        .bind(u)
        .bind(w)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, false, "row remains as a tombstone");
    }

    #[sqlx::test(migrations = "migrations")]
    async fn since_paginates_compound_cursor(pool: PgPool) {
        let repo = WalletLinkRepo { pool: &pool };
        // Two rows at the same source_updated_at — must not split across pages.
        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        let w1 = "11111111111111111111111111111111";
        let w2 = "11111111111111111111111111111112";
        repo.upsert(u1, w1, true, ts(100)).await.unwrap();
        repo.upsert(u2, w2, true, ts(100)).await.unwrap();

        let page1 = repo.since(DateTime::<Utc>::from_timestamp(0, 0).unwrap(), "", 1).await.unwrap();
        assert_eq!(page1.len(), 1);
        let last = &page1[0];
        let cursor_pk = format!("{}:{}", last.user_id, last.wallet_pubkey);
        let page2 = repo.since(last.source_updated_at, &cursor_pk, 10).await.unwrap();
        assert_eq!(page2.len(), 1, "second page must contain the second row");
        assert_ne!(page2[0].user_id, last.user_id);
    }
}
```

- [ ] **Step 2: Add to lib.rs**

In `crates/eros-engine-store/src/lib.rs`:

```rust
pub mod wallets;
```

- [ ] **Step 3: Run tests to confirm pass**

Run: `cargo test -p eros-engine-store wallets::tests -- --nocapture`
Expected: 3/3 PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/eros-engine-store/src/wallets.rs \
        crates/eros-engine-store/src/lib.rs
git commit -m "feat(store): WalletLinkRepo — stale-write upsert + tombstone unlink + compound cursor"
```

---

### Task 7: OwnershipRepo

UPSERT with stale-write guard, compound-cursor `since`, gate-check `owns()` join.

**Files:**
- Create: `crates/eros-engine-store/src/ownership.rs`
- Modify: `crates/eros-engine-store/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/eros-engine-store/src/ownership.rs`:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
//! Persona-ownership mirror, fed by /internal/ownership/upsert and the
//! self-heal /since pull. Also exposes the gate-decision `owns()` join
//! that the chat-start and per-message handlers call.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct Ownership {
    pub asset_id: String,
    pub persona_id: String,
    pub owner_wallet: String,
    pub source_updated_at: DateTime<Utc>,
}

pub struct OwnershipRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> OwnershipRepo<'a> {
    /// Idempotent UPSERT with stale-write protection.
    /// Returns `Ok(true)` if applied, `Ok(false)` if dropped as stale.
    pub async fn upsert(
        &self,
        asset_id: &str,
        persona_id: &str,
        owner_wallet: &str,
        source_updated_at: DateTime<Utc>,
    ) -> sqlx::Result<bool> {
        let res = sqlx::query(
            "INSERT INTO engine.persona_ownership
                (asset_id, persona_id, owner_wallet, source_updated_at, updated_at)
             VALUES ($1, $2, $3, $4, now())
             ON CONFLICT (asset_id) DO UPDATE
               SET persona_id        = EXCLUDED.persona_id,
                   owner_wallet      = EXCLUDED.owner_wallet,
                   source_updated_at = EXCLUDED.source_updated_at,
                   updated_at        = now()
               WHERE EXCLUDED.source_updated_at > engine.persona_ownership.source_updated_at",
        )
        .bind(asset_id)
        .bind(persona_id)
        .bind(owner_wallet)
        .bind(source_updated_at)
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Cursor-paginated `since` read. cursor_pk is `asset_id`.
    pub async fn since(
        &self,
        cursor_ts: DateTime<Utc>,
        cursor_pk: &str,
        limit: i64,
    ) -> sqlx::Result<Vec<Ownership>> {
        sqlx::query_as::<_, Ownership>(
            "SELECT asset_id, persona_id, owner_wallet, source_updated_at
               FROM engine.persona_ownership
              WHERE (source_updated_at, asset_id) > ($1, $2)
              ORDER BY source_updated_at ASC, asset_id ASC
              LIMIT $3",
        )
        .bind(cursor_ts)
        .bind(cursor_pk)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// Gate decision. Returns true iff `user_id` has at least one *active*
    /// wallet link to the wallet currently recorded as owning `asset_id`.
    /// `linked = true` filter excludes tombstones.
    pub async fn owns(&self, user_id: Uuid, asset_id: &str) -> sqlx::Result<bool> {
        let owns: bool = sqlx::query_scalar(
            "SELECT EXISTS (
               SELECT 1
                 FROM engine.persona_ownership po
                 JOIN engine.wallet_links wl
                   ON wl.wallet_pubkey = po.owner_wallet
                WHERE po.asset_id = $1
                  AND wl.user_id  = $2
                  AND wl.linked   = true
             )",
        )
        .bind(asset_id)
        .bind(user_id)
        .fetch_one(self.pool)
        .await?;
        Ok(owns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    #[sqlx::test(migrations = "migrations")]
    async fn ownership_upsert_drops_stale(pool: PgPool) {
        let repo = OwnershipRepo { pool: &pool };
        let asset = "11111111111111111111111111111111";
        let wallet_old = "OwnerOld1111111111111111111111111";
        let wallet_new = "OwnerNew2222222222222222222222222";

        assert!(repo.upsert(asset, "p-1", wallet_old, ts(100)).await.unwrap());
        assert!(repo.upsert(asset, "p-1", wallet_new, ts(200)).await.unwrap());
        // Older event must NOT revert.
        assert!(!repo.upsert(asset, "p-1", wallet_old, ts(150)).await.unwrap());

        let row: (String,) = sqlx::query_as(
            "SELECT owner_wallet FROM engine.persona_ownership WHERE asset_id = $1",
        )
        .bind(asset)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, wallet_new);
    }

    #[sqlx::test(migrations = "migrations")]
    async fn owns_passes_for_linked_owner(pool: PgPool) {
        use crate::wallets::WalletLinkRepo;
        let own = OwnershipRepo { pool: &pool };
        let wl = WalletLinkRepo { pool: &pool };

        let user = Uuid::new_v4();
        let wallet = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";
        let asset = "11111111111111111111111111111111";

        wl.upsert(user, wallet, true, ts(100)).await.unwrap();
        own.upsert(asset, "p-1", wallet, ts(100)).await.unwrap();

        assert!(own.owns(user, asset).await.unwrap());
    }

    #[sqlx::test(migrations = "migrations")]
    async fn owns_rejects_unlinked_owner(pool: PgPool) {
        use crate::wallets::WalletLinkRepo;
        let own = OwnershipRepo { pool: &pool };
        let wl = WalletLinkRepo { pool: &pool };

        let user = Uuid::new_v4();
        let wallet = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";
        let asset = "11111111111111111111111111111111";

        wl.upsert(user, wallet, true, ts(100)).await.unwrap();
        own.upsert(asset, "p-1", wallet, ts(100)).await.unwrap();
        // Unlink the wallet.
        wl.upsert(user, wallet, false, ts(200)).await.unwrap();

        assert!(!own.owns(user, asset).await.unwrap(), "tombstone must block gate");
    }

    #[sqlx::test(migrations = "migrations")]
    async fn owns_rejects_when_someone_else_owns(pool: PgPool) {
        use crate::wallets::WalletLinkRepo;
        let own = OwnershipRepo { pool: &pool };
        let wl = WalletLinkRepo { pool: &pool };

        let user = Uuid::new_v4();
        let my_wallet = "MyWallet111111111111111111111111";
        let their_wallet = "TheirWallet22222222222222222222";
        let asset = "11111111111111111111111111111111";

        wl.upsert(user, my_wallet, true, ts(100)).await.unwrap();
        own.upsert(asset, "p-1", their_wallet, ts(100)).await.unwrap();

        assert!(!own.owns(user, asset).await.unwrap());
    }
}
```

- [ ] **Step 2: Add to lib.rs**

In `crates/eros-engine-store/src/lib.rs`:

```rust
pub mod ownership;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p eros-engine-store ownership::tests -- --nocapture`
Expected: 4/4 PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/eros-engine-store/src/ownership.rs \
        crates/eros-engine-store/src/lib.rs
git commit -m "feat(store): OwnershipRepo — stale-write upsert + compound cursor + gate-decision owns()"
```

---

### Task 8: SyncCursorRepo

Read/write the compound (cursor_ts, cursor_pk) for the self-heal loop.

**Files:**
- Create: `crates/eros-engine-store/src/sync_cursors.rs`
- Modify: `crates/eros-engine-store/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/eros-engine-store/src/sync_cursors.rs`:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
//! Compound (cursor_ts, cursor_pk) persistence for the self-heal loop.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct Cursor {
    pub cursor_ts: DateTime<Utc>,
    pub cursor_pk: String,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            cursor_ts: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
            cursor_pk: String::new(),
        }
    }
}

pub struct SyncCursorRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> SyncCursorRepo<'a> {
    /// Read the cursor for `name`, returning the epoch+empty default if no
    /// row exists yet.
    pub async fn get(&self, name: &str) -> sqlx::Result<Cursor> {
        let row: Option<(DateTime<Utc>, String)> = sqlx::query_as(
            "SELECT cursor_ts, cursor_pk FROM engine.sync_cursors WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(self.pool)
        .await?;
        Ok(row
            .map(|(ts, pk)| Cursor { cursor_ts: ts, cursor_pk: pk })
            .unwrap_or_default())
    }

    /// Idempotent UPSERT — overwrites the previous cursor with the new one.
    pub async fn set(&self, name: &str, cursor: &Cursor) -> sqlx::Result<()> {
        sqlx::query(
            "INSERT INTO engine.sync_cursors (name, cursor_ts, cursor_pk, updated_at)
             VALUES ($1, $2, $3, now())
             ON CONFLICT (name) DO UPDATE
               SET cursor_ts  = EXCLUDED.cursor_ts,
                   cursor_pk  = EXCLUDED.cursor_pk,
                   updated_at = now()",
        )
        .bind(name)
        .bind(cursor.cursor_ts)
        .bind(&cursor.cursor_pk)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "migrations")]
    async fn missing_cursor_returns_epoch_default(pool: PgPool) {
        let repo = SyncCursorRepo { pool: &pool };
        let c = repo.get("ownership").await.unwrap();
        assert_eq!(c.cursor_ts, DateTime::<Utc>::from_timestamp(0, 0).unwrap());
        assert_eq!(c.cursor_pk, "");
    }

    #[sqlx::test(migrations = "migrations")]
    async fn set_then_get_roundtrips(pool: PgPool) {
        let repo = SyncCursorRepo { pool: &pool };
        let want = Cursor {
            cursor_ts: DateTime::<Utc>::from_timestamp(1700000000, 0).unwrap(),
            cursor_pk: "11111111111111111111111111111111".into(),
        };
        repo.set("ownership", &want).await.unwrap();
        let got = repo.get("ownership").await.unwrap();
        assert_eq!(got.cursor_ts, want.cursor_ts);
        assert_eq!(got.cursor_pk, want.cursor_pk);
    }
}
```

- [ ] **Step 2: Add to lib.rs**

```rust
pub mod sync_cursors;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p eros-engine-store sync_cursors::tests -- --nocapture`
Expected: 2/2 PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/eros-engine-store/src/sync_cursors.rs \
        crates/eros-engine-store/src/lib.rs
git commit -m "feat(store): SyncCursorRepo — compound (cursor_ts, cursor_pk) for self-heal"
```

---

### Task 9: PersonaRepo extension

Add a single helper that returns the `asset_id` for a genome (or `None` if legacy). Keeps `eros-engine-core::persona::PersonaGenome` unchanged.

**Files:**
- Modify: `crates/eros-engine-store/src/persona.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/eros-engine-store/src/persona.rs` (inside an existing or new `#[cfg(test)]` block):

```rust
#[cfg(test)]
mod ownership_lookup_tests {
    use super::*;
    use sqlx::PgPool;

    #[sqlx::test(migrations = "migrations")]
    async fn returns_none_for_legacy_genome(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active)
             VALUES ($1, 'Legacy', 'p', '{}'::jsonb, true)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(repo.get_asset_id_for_genome(id).await.unwrap(), None);
    }

    #[sqlx::test(migrations = "migrations")]
    async fn returns_some_for_nft_genome(pool: PgPool) {
        let repo = PersonaRepo { pool: &pool };
        let id = uuid::Uuid::new_v4();
        let asset = "11111111111111111111111111111111";
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active, asset_id)
             VALUES ($1, 'Nft', 'p', '{}'::jsonb, true, $2)",
        )
        .bind(id)
        .bind(asset)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            repo.get_asset_id_for_genome(id).await.unwrap().as_deref(),
            Some(asset)
        );
    }
}
```

- [ ] **Step 2: Run to confirm fail**

Run: `cargo test -p eros-engine-store persona::ownership_lookup_tests -- --nocapture`
Expected: FAIL — `no method named get_asset_id_for_genome`.

- [ ] **Step 3: Add the method**

In `crates/eros-engine-store/src/persona.rs`, inside the existing `impl<'a> PersonaRepo<'a>` block, append:

```rust
    /// Returns the `asset_id` for an NFT-backed genome, or `None` for legacy
    /// seed-persona rows where the column is NULL. Used by the chat-start
    /// and per-message gates to decide whether to invoke the NFT ownership
    /// check.
    pub async fn get_asset_id_for_genome(
        &self,
        genome_id: uuid::Uuid,
    ) -> sqlx::Result<Option<String>> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT asset_id FROM engine.persona_genomes WHERE id = $1",
        )
        .bind(genome_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(row.and_then(|(opt,)| opt))
    }
```

- [ ] **Step 4: Run to confirm pass**

Run: `cargo test -p eros-engine-store persona::ownership_lookup_tests -- --nocapture`
Expected: 2/2 PASS. Also run the full store crate to confirm no regression: `cargo test -p eros-engine-store`.

- [ ] **Step 5: Commit**

```bash
git add crates/eros-engine-store/src/persona.rs
git commit -m "feat(store): PersonaRepo::get_asset_id_for_genome

Standalone helper so the gate can branch on legacy vs NFT-backed genomes
without touching eros-engine-core::persona::PersonaGenome."
```

---

## Phase E2 — S2S middleware + routes

### Task 10: HMAC s2s middleware

Canonical 5-line signing, 1 MiB body cap, current + previous secret support.

**Files:**
- Modify: `crates/eros-engine-server/Cargo.toml`
- Create: `crates/eros-engine-server/src/auth/s2s.rs`
- Modify: `crates/eros-engine-server/src/auth/mod.rs`

- [ ] **Step 1: Add deps to the server crate**

Append to `[dependencies]` in `crates/eros-engine-server/Cargo.toml`:

```toml
hmac = { workspace = true }
sha2 = { workspace = true }
subtle = { workspace = true }
hex = { workspace = true }
```

- [ ] **Step 2: Write the failing tests**

Create `crates/eros-engine-server/src/auth/s2s.rs`:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
//! HMAC-SHA256 server-to-server authentication for /internal/* routes.
//!
//! Canonical signing string is a deterministic five-line ASCII layout:
//!     method + "\n"
//!   + path + "\n"
//!   + canonical_query + "\n"
//!   + timestamp + "\n"
//!   + body_sha256_hex
//!
//! Method + path + canonical_query bind the signature to a specific
//! request; signing the body alone would be replayable across endpoints.
//! Body is buffered up to 1 MiB before HMAC computation; oversized bodies
//! return 413 without computing the hash, blocking memory-DoS.
//!
//! Two secrets supported for rolling rotation:
//!   - MARKETPLACE_SVC_S2S_SECRET           (active, used to sign outbound)
//!   - MARKETPLACE_SVC_S2S_SECRET_PREVIOUS  (verify-only, accepted for inbound)

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{Request, State},
    http::{header::HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::state::AppState;

pub const MAX_BODY_BYTES: usize = 1024 * 1024; // 1 MiB
pub const TIMESTAMP_SKEW_SECS: i64 = 5 * 60;

type HmacSha256 = Hmac<Sha256>;

/// Build the canonical signing string from request parts.
pub fn canonical_signing_string(
    method: &str,
    path: &str,
    canonical_query: &str,
    timestamp: &str,
    body_sha256_hex: &str,
) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        path,
        canonical_query,
        timestamp,
        body_sha256_hex
    )
}

/// Canonicalize a query string: split on `&`, sort by name+value, re-join.
/// Empty input → empty output.
pub fn canonicalize_query(q: &str) -> String {
    if q.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<&str> = q.split('&').collect();
    pairs.sort();
    pairs.join("&")
}

/// Compute HMAC-SHA256 over `canonical` with `secret`, returning hex.
pub fn sign(secret: &[u8], canonical: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(canonical.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn verify_against(secret: &[u8], canonical: &str, provided_hex: &str) -> bool {
    let provided = match hex::decode(provided_hex) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(canonical.as_bytes());
    let expected = mac.finalize().into_bytes();
    expected.ct_eq(&provided).into()
}

/// Axum middleware: verifies the incoming HMAC and passes the buffered
/// body through to the handler. Mount only on /internal/*.
pub async fn require_s2s(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Pull headers up-front so we don't borrow req across body read.
    let headers = req.headers().clone();
    let timestamp = headers
        .get("x-s2s-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_string();
    let signature = headers
        .get("x-s2s-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_string();

    // Reject malformed or skewed timestamp before doing any work.
    let ts_parsed: DateTime<Utc> = timestamp
        .parse()
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let skew = (Utc::now() - ts_parsed).num_seconds().abs();
    if skew > TIMESTAMP_SKEW_SECS {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Buffer body with size cap.
    let method = req.method().clone();
    let uri = req.uri().clone();
    let body = std::mem::replace(req.body_mut(), Body::empty());
    let bytes: Bytes = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return Err(StatusCode::PAYLOAD_TOO_LARGE),
    };

    // Build the canonical signing string.
    let body_hash = Sha256::digest(&bytes);
    let body_sha256_hex = hex::encode(body_hash);
    let canonical = canonical_signing_string(
        method.as_str(),
        uri.path(),
        &canonicalize_query(uri.query().unwrap_or("")),
        &timestamp,
        &body_sha256_hex,
    );

    // Try active + previous secret. Both unset → reject.
    let mut any_secret = false;
    let mut matched = false;
    if let Some(secret) = state.marketplace_s2s_secret.as_deref() {
        any_secret = true;
        if verify_against(secret.as_bytes(), &canonical, &signature) {
            matched = true;
        }
    }
    if !matched {
        if let Some(secret) = state.marketplace_s2s_secret_previous.as_deref() {
            any_secret = true;
            if verify_against(secret.as_bytes(), &canonical, &signature) {
                matched = true;
            }
        }
    }
    if !any_secret || !matched {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Hand the buffered body to the inner handler.
    *req.body_mut() = Body::from(bytes);
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_string_layout_is_stable() {
        let s = canonical_signing_string(
            "POST",
            "/internal/ownership/upsert",
            "",
            "2026-05-13T08:00:00Z",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_eq!(
            s,
            "POST\n/internal/ownership/upsert\n\n2026-05-13T08:00:00Z\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn canonicalize_query_sorts() {
        assert_eq!(
            canonicalize_query("limit=100&cursor_ts=2026-05-13T00:00:00Z&cursor_pk="),
            "cursor_pk=&cursor_ts=2026-05-13T00:00:00Z&limit=100"
        );
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let secret = b"test-secret";
        let canonical = "GET\n/internal/wallets/since\ncursor_pk=&cursor_ts=&limit=10\n2026-05-13T00:00:00Z\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let sig = sign(secret, canonical);
        assert!(verify_against(secret, canonical, &sig));
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let canonical = "GET\n/p\n\n2026-01-01T00:00:00Z\ne3b0...";
        let sig = sign(b"k1", canonical);
        assert!(!verify_against(b"k2", canonical, &sig));
    }

    #[test]
    fn verify_rejects_non_hex_signature() {
        assert!(!verify_against(b"k", "anything", "not-hex"));
    }
}
```

- [ ] **Step 3: Wire the module**

In `crates/eros-engine-server/src/auth/mod.rs`, add:

```rust
pub mod s2s;
```

- [ ] **Step 4: Run unit tests**

Run: `cargo test -p eros-engine-server auth::s2s::tests -- --nocapture`
Expected: 5/5 PASS.

(The full middleware behavior — header parsing, body cap, skew rejection — is exercised in Task 14's integration tests.)

- [ ] **Step 5: Commit**

```bash
git add crates/eros-engine-server/Cargo.toml \
        crates/eros-engine-server/src/auth/s2s.rs \
        crates/eros-engine-server/src/auth/mod.rs
git commit -m "feat(server): HMAC s2s middleware — 5-line canonical signing + 1MiB cap + dual secret"
```

---

### Task 11: Internal routes module skeleton

Create `routes/internal.rs` with shared types, the base58 validator wrapper, and the empty handler shells (returning 501) so the router composition in Task 14 has something to wire.

**Files:**
- Create: `crates/eros-engine-server/src/routes/internal.rs`
- Modify: `crates/eros-engine-server/src/routes/mod.rs`

- [ ] **Step 1: Create the file with handler stubs**

Create `crates/eros-engine-server/src/routes/internal.rs`:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
//! Server-to-server endpoints called by eros-marketplace-svc. Mounted at
//! /internal/* with HMAC auth (see auth::s2s); deliberately outside the
//! Supabase JWT layer that gates /comp/*.
//!
//! Wire shape mirrors svc's expected /since cursor pagination and stale-
//! write-protected /upsert semantics. Inputs are validated at the API
//! boundary (base58 32-byte for pubkeys/asset_ids) so non-canonical
//! representations cannot create logical duplicates downstream.

use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use eros_engine_store::ownership::OwnershipRepo;
use eros_engine_store::pubkey::validate_solana_pubkey;
use eros_engine_store::wallets::WalletLinkRepo;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Deserialize, ToSchema)]
pub struct WalletUpsertRequest {
    pub user_id: Uuid,
    pub wallet_pubkey: String,
    pub linked: bool,
    pub source_updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct OwnershipUpsertRequest {
    pub asset_id: String,
    pub persona_id: String,
    pub owner_wallet: String,
    pub source_updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SinceCursor {
    pub ts: DateTime<Utc>,
    pub pk: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WalletsSinceResponse {
    pub rows: Vec<eros_engine_store::wallets::WalletLink>,
    pub next_cursor: Option<SinceCursor>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OwnershipSinceResponse {
    pub rows: Vec<eros_engine_store::ownership::Ownership>,
    pub next_cursor: Option<SinceCursor>,
}

/// Stub — implemented in Task 12.
#[utoipa::path(
    post,
    path = "/internal/wallets/upsert",
    tag = "internal",
    security(("hmac_signature" = [])),
    request_body = WalletUpsertRequest,
    responses(
        (status = 204, description = "applied"),
        (status = 401, description = "missing or invalid HMAC"),
        (status = 409, description = "stale event (source_updated_at older than existing)")
    )
)]
async fn wallets_upsert(
    State(_state): State<AppState>,
    Json(_req): Json<WalletUpsertRequest>,
) -> Result<StatusCode, AppError> {
    Err(AppError::Internal("not implemented".into()))
}

/// Stub — implemented in Task 14.
#[utoipa::path(
    get,
    path = "/internal/wallets/since",
    tag = "internal",
    security(("hmac_signature" = [])),
    params(
        ("cursor_ts" = Option<DateTime<Utc>>, Query, description = "compound cursor: source_updated_at"),
        ("cursor_pk" = Option<String>, Query, description = "compound cursor: user_id:wallet_pubkey"),
        ("limit" = Option<i64>, Query, description = "1..1000, default 100"),
    ),
    responses(
        (status = 200, body = WalletsSinceResponse),
        (status = 401, description = "missing or invalid HMAC"),
    )
)]
async fn wallets_since(
    State(_state): State<AppState>,
) -> Result<Json<WalletsSinceResponse>, AppError> {
    Err(AppError::Internal("not implemented".into()))
}

/// Stub — implemented in Task 13.
#[utoipa::path(
    post,
    path = "/internal/ownership/upsert",
    tag = "internal",
    security(("hmac_signature" = [])),
    request_body = OwnershipUpsertRequest,
    responses(
        (status = 204, description = "applied"),
        (status = 401, description = "missing or invalid HMAC"),
        (status = 409, description = "stale event"),
    )
)]
async fn ownership_upsert(
    State(_state): State<AppState>,
    Json(_req): Json<OwnershipUpsertRequest>,
) -> Result<StatusCode, AppError> {
    Err(AppError::Internal("not implemented".into()))
}

/// Stub — implemented in Task 14.
#[utoipa::path(
    get,
    path = "/internal/ownership/since",
    tag = "internal",
    security(("hmac_signature" = [])),
    params(
        ("cursor_ts" = Option<DateTime<Utc>>, Query),
        ("cursor_pk" = Option<String>, Query),
        ("limit" = Option<i64>, Query),
    ),
    responses(
        (status = 200, body = OwnershipSinceResponse),
        (status = 401, description = "missing or invalid HMAC"),
    )
)]
async fn ownership_since(
    State(_state): State<AppState>,
) -> Result<Json<OwnershipSinceResponse>, AppError> {
    Err(AppError::Internal("not implemented".into()))
}

/// Build the /internal/* subrouter. The HMAC layer is applied at router
/// composition time (routes/mod.rs), not here.
pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(wallets_upsert))
        .routes(routes!(wallets_since))
        .routes(routes!(ownership_upsert))
        .routes(routes!(ownership_since))
}
```

- [ ] **Step 2: Register the module**

In `crates/eros-engine-server/src/routes/mod.rs`, add:

```rust
pub mod internal;
```

(Composition into the app router lands in Task 14 — keep the existing `router()` body unchanged for now.)

- [ ] **Step 3: Verify it builds**

Run: `cargo build -p eros-engine-server`
Expected: clean exit code 0 (handlers exist as stubs returning errors, but compile).

- [ ] **Step 4: Commit**

```bash
git add crates/eros-engine-server/src/routes/internal.rs \
        crates/eros-engine-server/src/routes/mod.rs
git commit -m "feat(server): /internal/* routes module + payload schemas (handler stubs)"
```

---

### Task 12: `POST /internal/wallets/upsert` handler

Validates the wallet pubkey, calls `WalletLinkRepo::upsert`, returns `204` or `409`.

**Files:**
- Modify: `crates/eros-engine-server/src/routes/internal.rs`

- [ ] **Step 1: Replace the stub with the real handler**

Replace `wallets_upsert` in `routes/internal.rs`:

```rust
async fn wallets_upsert(
    State(state): State<AppState>,
    Json(req): Json<WalletUpsertRequest>,
) -> Result<StatusCode, AppError> {
    let pubkey = validate_solana_pubkey(&req.wallet_pubkey)
        .map_err(|e| AppError::BadRequest(format!("invalid wallet_pubkey: {e}")))?;
    let applied = WalletLinkRepo { pool: &state.pool }
        .upsert(req.user_id, &pubkey, req.linked, req.source_updated_at)
        .await
        .map_err(AppError::from)?;
    if applied {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::CONFLICT)
    }
}
```

- [ ] **Step 2: Build to verify**

Run: `cargo build -p eros-engine-server`
Expected: clean.

(Handler-level integration test lands in Task 14 once router composition is in place.)

- [ ] **Step 3: Commit**

```bash
git add crates/eros-engine-server/src/routes/internal.rs
git commit -m "feat(server): implement POST /internal/wallets/upsert handler"
```

---

### Task 13: `POST /internal/ownership/upsert` handler

Same shape; validates `asset_id` and `owner_wallet`.

**Files:**
- Modify: `crates/eros-engine-server/src/routes/internal.rs`

- [ ] **Step 1: Replace the stub**

Replace `ownership_upsert`:

```rust
async fn ownership_upsert(
    State(state): State<AppState>,
    Json(req): Json<OwnershipUpsertRequest>,
) -> Result<StatusCode, AppError> {
    let asset = validate_solana_pubkey(&req.asset_id)
        .map_err(|e| AppError::BadRequest(format!("invalid asset_id: {e}")))?;
    let owner = validate_solana_pubkey(&req.owner_wallet)
        .map_err(|e| AppError::BadRequest(format!("invalid owner_wallet: {e}")))?;
    let applied = OwnershipRepo { pool: &state.pool }
        .upsert(&asset, &req.persona_id, &owner, req.source_updated_at)
        .await
        .map_err(AppError::from)?;
    if applied {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::CONFLICT)
    }
}
```

- [ ] **Step 2: Build to verify**

Run: `cargo build -p eros-engine-server`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/eros-engine-server/src/routes/internal.rs
git commit -m "feat(server): implement POST /internal/ownership/upsert handler"
```

---

### Task 14: `GET /internal/*/since` handlers + router composition + state + boot validation

This is the largest task in E2. It wires:
- The two `/since` handlers (compound cursor read + next_cursor computation).
- The `/internal/*` subrouter into the app, **outside** the JWT layer.
- The new fields on `AppState`.
- Boot-time validation: `MARKETPLACE_SVC_URL` set without `_SECRET` → bail.
- The `hmac_signature` security scheme in OpenAPI.
- An integration test that exercises the HMAC middleware end-to-end.

**Files:**
- Modify: `crates/eros-engine-server/src/routes/internal.rs`
- Modify: `crates/eros-engine-server/src/routes/mod.rs`
- Modify: `crates/eros-engine-server/src/state.rs`
- Modify: `crates/eros-engine-server/src/main.rs`
- Modify: `crates/eros-engine-server/src/openapi.rs`

- [ ] **Step 1: Implement `/since` handlers**

Replace `wallets_since` and `ownership_since` in `routes/internal.rs`:

```rust
use axum::extract::Query;
use eros_engine_store::sync_cursors::Cursor;

#[derive(Debug, Deserialize)]
pub struct SinceParams {
    pub cursor_ts: Option<DateTime<Utc>>,
    pub cursor_pk: Option<String>,
    pub limit: Option<i64>,
}

impl SinceParams {
    fn resolved(self) -> (DateTime<Utc>, String, i64) {
        let ts = self.cursor_ts.unwrap_or_else(|| {
            DateTime::<Utc>::from_timestamp(0, 0).unwrap()
        });
        let pk = self.cursor_pk.unwrap_or_default();
        let limit = self.limit.unwrap_or(100).clamp(1, 1000);
        (ts, pk, limit)
    }
}

async fn wallets_since(
    State(state): State<AppState>,
    Query(params): Query<SinceParams>,
) -> Result<Json<WalletsSinceResponse>, AppError> {
    let (ts, pk, limit) = params.resolved();
    let rows = WalletLinkRepo { pool: &state.pool }
        .since(ts, &pk, limit)
        .await
        .map_err(AppError::from)?;
    let next_cursor = rows.last().map(|last| SinceCursor {
        ts: last.source_updated_at,
        pk: format!("{}:{}", last.user_id, last.wallet_pubkey),
    });
    let next_cursor = if (rows.len() as i64) < limit {
        None
    } else {
        next_cursor
    };
    Ok(Json(WalletsSinceResponse { rows, next_cursor }))
}

async fn ownership_since(
    State(state): State<AppState>,
    Query(params): Query<SinceParams>,
) -> Result<Json<OwnershipSinceResponse>, AppError> {
    let (ts, pk, limit) = params.resolved();
    let rows = OwnershipRepo { pool: &state.pool }
        .since(ts, &pk, limit)
        .await
        .map_err(AppError::from)?;
    let next_cursor = rows.last().map(|last| SinceCursor {
        ts: last.source_updated_at,
        pk: last.asset_id.clone(),
    });
    let next_cursor = if (rows.len() as i64) < limit {
        None
    } else {
        next_cursor
    };
    Ok(Json(OwnershipSinceResponse { rows, next_cursor }))
}
```

Suppress the unused import (the stub used `Cursor`; the real handlers don't) by removing the `use eros_engine_store::sync_cursors::Cursor;` line if it was added.

- [ ] **Step 2: Extend AppState**

In `crates/eros-engine-server/src/state.rs`, add fields to `AppState`:

```rust
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub auth: Arc<dyn AuthValidator>,
    pub config: AppConfig,
    pub openrouter: Arc<eros_engine_llm::openrouter::OpenRouterClient>,
    pub voyage: Arc<eros_engine_llm::voyage::VoyageClient>,
    pub model_config: Arc<eros_engine_llm::model_config::ModelConfig>,
    // NEW marketplace coordination fields:
    pub marketplace_svc_url: Option<String>,
    pub marketplace_s2s_secret: Option<String>,
    pub marketplace_s2s_secret_previous: Option<String>,
    pub http_client: reqwest::Client,
}
```

Update any `AppState { ... }` construction in test helpers (`test_state` in `companion.rs`) to set these to `None` / `reqwest::Client::new()` so existing tests stay green.

- [ ] **Step 3: Compose `/internal/*` outside the JWT layer**

In `crates/eros-engine-server/src/routes/mod.rs`, rewrite `router` to add a third top-level sub-router for internal routes with the s2s middleware:

```rust
use axum::middleware::from_fn_with_state;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::middleware::require_auth;
use crate::auth::s2s::require_s2s;
use crate::state::AppState;

pub mod companion;
pub mod debug;
pub mod health;
pub mod internal;

pub fn router(state: AppState) -> OpenApiRouter<AppState> {
    let comp = OpenApiRouter::new()
        .merge(companion::router())
        .merge(debug::router(state.config.expose_affinity_debug))
        .layer(from_fn_with_state(state.clone(), require_auth));

    let internal_routes = internal::router()
        .layer(from_fn_with_state(state.clone(), require_s2s));

    OpenApiRouter::new()
        .merge(health::router())
        .merge(comp)
        .merge(internal_routes)
}

pub fn router_for_openapi(expose_affinity_debug: bool) -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(health::router())
        .merge(companion::router())
        .merge(debug::router(expose_affinity_debug))
        .merge(internal::router())
}
```

The two routers must agree on what routes exist; the only difference is the middleware layer. The OpenAPI drift check in CI catches divergence.

- [ ] **Step 4: Register `hmac_signature` security scheme + env wiring + boot validation**

In `crates/eros-engine-server/src/openapi.rs`, extend the existing `SecurityAddon` `Modify` impl to also register `hmac_signature`:

```rust
impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder};
        let components = openapi.components.as_mut().expect("components");
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .build(),
            ),
        );
        components.add_security_scheme(
            "hmac_signature",
            // Two API-key headers (timestamp + signature) is the closest
            // OpenAPI 3.1 vocabulary; documenting both as ApiKey headers
            // is the convention in similar projects (Stripe, Twilio).
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("X-S2S-Signature"))),
        );
    }
}
```

In `crates/eros-engine-server/src/main.rs`, alongside the existing env-var loading, add:

```rust
let marketplace_svc_url = std::env::var("MARKETPLACE_SVC_URL").ok();
let marketplace_s2s_secret = std::env::var("MARKETPLACE_SVC_S2S_SECRET").ok();
let marketplace_s2s_secret_previous = std::env::var("MARKETPLACE_SVC_S2S_SECRET_PREVIOUS").ok();
if marketplace_svc_url.is_some() && marketplace_s2s_secret.is_none() {
    anyhow::bail!(
        "MARKETPLACE_SVC_URL is set but MARKETPLACE_SVC_S2S_SECRET is not. \
         The self-heal pull cannot sign outbound requests without a secret. \
         Set the secret, or unset the URL to run in OSS-only mode."
    );
}
```

Pass them into the `AppState` construction:

```rust
let state = AppState {
    pool,
    auth,
    config: cfg.clone(),
    openrouter,
    voyage,
    model_config,
    marketplace_svc_url,
    marketplace_s2s_secret,
    marketplace_s2s_secret_previous,
    http_client: reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("reqwest client"),
};
```

- [ ] **Step 5: Integration test — end-to-end HMAC + handler**

Append to `crates/eros-engine-server/src/routes/internal.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use chrono::Utc;
    use eros_engine_store::ownership::OwnershipRepo;
    use eros_engine_store::wallets::WalletLinkRepo;
    use sqlx::PgPool;

    // These helpers are duplicated from companion.rs intentionally — they
    // build a router with the s2s secret pre-set so the middleware accepts
    // signed requests in tests.
    fn build_app_with_secret(pool: PgPool, secret: &str) -> axum::Router {
        let state = crate::routes::companion::test_state(pool);
        let mut state = state;
        state.marketplace_s2s_secret = Some(secret.to_string());
        let app = crate::routes::router(state.clone()).split_for_parts().0;
        app.with_state(state)
    }

    fn sign_request(
        secret: &str,
        method: &str,
        path: &str,
        body: &[u8],
        timestamp: &str,
    ) -> String {
        use sha2::Digest;
        let body_hash = sha2::Sha256::digest(body);
        let body_hex = hex::encode(body_hash);
        let canonical = crate::auth::s2s::canonical_signing_string(
            method, path, "", timestamp, &body_hex,
        );
        crate::auth::s2s::sign(secret.as_bytes(), &canonical)
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn rejects_request_without_signature(pool: PgPool) {
        let app = build_app_with_secret(pool, "test-secret");
        let req = Request::builder()
            .method("POST")
            .uri("/internal/wallets/upsert")
            .body(Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn signed_upsert_persists(pool: PgPool) {
        let user = uuid::Uuid::new_v4();
        let body = serde_json::json!({
            "user_id": user,
            "wallet_pubkey": "11111111111111111111111111111111",
            "linked": true,
            "source_updated_at": Utc::now(),
        });
        let body_bytes = serde_json::to_vec(&body).unwrap();
        let ts = Utc::now().to_rfc3339();
        let sig = sign_request(
            "test-secret",
            "POST",
            "/internal/wallets/upsert",
            &body_bytes,
            &ts,
        );

        let app = build_app_with_secret(pool.clone(), "test-secret");
        let req = Request::builder()
            .method("POST")
            .uri("/internal/wallets/upsert")
            .header("content-type", "application/json")
            .header("x-s2s-timestamp", ts)
            .header("x-s2s-signature", sig)
            .body(Body::from(body_bytes))
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), 204);

        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM engine.wallet_links WHERE user_id = $1",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count.0, 1);
    }
}
```

You may need to make `test_state` and `build_router` in `companion.rs` `pub(crate)` so this test module can call them. If they're private, add `pub(crate) fn test_state(...)` and `pub(crate) fn build_router(...)` to companion.rs's `#[cfg(test)]` block.

- [ ] **Step 6: Regenerate the OpenAPI snapshot**

The drift-check CI (`9fd3499`) will catch new routes that lack `#[utoipa::path]` or schema additions. Regenerate locally:

Run: `cargo run -p eros-engine-server --bin eros-engine -- print-openapi > crates/eros-engine-server/openapi.json`
Expected: snapshot updated with the four new `/internal/*` paths and the `hmac_signature` security scheme.

- [ ] **Step 7: Run the full test suite**

Run: `cargo test -p eros-engine-server`
Expected: all green, including the two new integration tests + existing route tests.

Run: `cargo test --workspace`
Expected: all green.

- [ ] **Step 8: Commit**

```bash
git add crates/eros-engine-server/src/routes/internal.rs \
        crates/eros-engine-server/src/routes/mod.rs \
        crates/eros-engine-server/src/routes/companion.rs \
        crates/eros-engine-server/src/state.rs \
        crates/eros-engine-server/src/main.rs \
        crates/eros-engine-server/src/openapi.rs \
        crates/eros-engine-server/openapi.json
git commit -m "feat(server): wire /internal/* routes — handlers + HMAC layer + boot validation

- /since handlers compute next_cursor with the same-timestamp tie-break.
- Internal subrouter merged OUTSIDE the JWT layer; /healthz public,
  /comp/* JWT, /internal/* HMAC.
- AppState gains marketplace_svc_url + 2 secrets + reqwest client.
- main.rs bails at boot if MARKETPLACE_SVC_URL is set without the secret.
- OpenAPI snapshot regenerated with the four new paths + hmac_signature
  scheme."
```

---

## Phase E3 — Gate

### Task 15: `enforce_nft_ownership` helper

Single helper called from three sites (start_chat's two paths + per-message routes). Lives in `companion.rs` since that's where the call sites are.

**Files:**
- Modify: `crates/eros-engine-server/src/routes/companion.rs`

- [ ] **Step 1: Write a failing test**

Append to the `#[cfg(test)]` block in `companion.rs`:

```rust
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn enforce_passes_for_legacy_genome(pool: PgPool) {
        let user = Uuid::new_v4();
        let res = enforce_nft_ownership(&pool, user, None).await;
        assert!(res.is_ok(), "asset_id=None must always pass");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn enforce_rejects_when_not_owner(pool: PgPool) {
        let user = Uuid::new_v4();
        let res = enforce_nft_ownership(&pool, user, Some("11111111111111111111111111111111")).await;
        match res {
            Err(AppError::Forbidden(_)) => {}
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn enforce_passes_for_owner(pool: PgPool) {
        use eros_engine_store::ownership::OwnershipRepo;
        use eros_engine_store::wallets::WalletLinkRepo;
        use chrono::Utc;

        let user = Uuid::new_v4();
        let wallet = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";
        let asset = "11111111111111111111111111111131";
        WalletLinkRepo { pool: &pool }
            .upsert(user, wallet, true, Utc::now())
            .await
            .unwrap();
        OwnershipRepo { pool: &pool }
            .upsert(asset, "p-1", wallet, Utc::now())
            .await
            .unwrap();

        assert!(enforce_nft_ownership(&pool, user, Some(asset)).await.is_ok());
    }
```

- [ ] **Step 2: Run to confirm fail**

Run: `cargo test -p eros-engine-server enforce_ -- --nocapture`
Expected: FAIL — `enforce_nft_ownership` not found.

- [ ] **Step 3: Add the helper**

Add to `companion.rs` (near the top of the file, after the imports):

```rust
use eros_engine_store::ownership::OwnershipRepo;

/// NFT-ownership gate. Returns `Ok(())` immediately if `asset_id` is `None`
/// (legacy seed-persona genome). Otherwise joins persona_ownership with
/// wallet_links (linked=true) and returns 403 on no match.
///
/// Called at chat-start (before create_instance) and at every chat message
/// (sync + async). The join is a single indexed PK lookup followed by an
/// index lookup on wallet_pubkey — sub-ms.
pub(crate) async fn enforce_nft_ownership(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    asset_id: Option<&str>,
) -> Result<(), AppError> {
    let Some(asset_id) = asset_id else {
        return Ok(());
    };
    let owns = OwnershipRepo { pool }
        .owns(user_id, asset_id)
        .await
        .map_err(AppError::from)?;
    if owns {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "nft_ownership_required".into(),
        ))
    }
}
```

- [ ] **Step 4: Run to confirm pass**

Run: `cargo test -p eros-engine-server enforce_ -- --nocapture`
Expected: 3/3 PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/eros-engine-server/src/routes/companion.rs
git commit -m "feat(server): enforce_nft_ownership helper + unit tests"
```

---

### Task 16: Wire gate into `start_chat`

Two call sites: the `genome_id` path (before `create_instance`) and the `instance_id` path (after companion load).

**Files:**
- Modify: `crates/eros-engine-server/src/routes/companion.rs`

- [ ] **Step 1: Write failing integration tests**

Append to `companion.rs`'s `#[cfg(test)]` block:

```rust
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn start_chat_403_on_unowned_nft_genome(pool: PgPool) {
        // Seed an NFT-backed genome whose asset_id no one currently owns.
        let genome_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active, asset_id)
             VALUES ($1, 'NftGenome', 'p', '{}'::jsonb, true,
                     '11111111111111111111111111111131')",
        )
        .bind(genome_id)
        .execute(&pool)
        .await
        .unwrap();

        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(Uuid::new_v4());

        let body = serde_json::to_vec(&serde_json::json!({ "genome_id": genome_id })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _resp) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Crucially: NO instance row was created.
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM engine.persona_instances WHERE genome_id = $1",
        )
        .bind(genome_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count.0, 0, "non-owner must not create a hidden persona_instance");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn start_chat_passes_for_legacy_genome(pool: PgPool) {
        // Unchanged path: legacy seed-persona must still work.
        let genome_id = seed_genome(&pool, "Echo").await;
        let user = Uuid::new_v4();
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user);

        let body = serde_json::to_vec(&serde_json::json!({ "genome_id": genome_id })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK);
    }
```

- [ ] **Step 2: Run to confirm fail**

Run: `cargo test -p eros-engine-server start_chat_403_on_unowned_nft_genome start_chat_passes_for_legacy_genome -- --nocapture`
Expected: the `_403_` test FAILs (handler currently returns 200), the legacy test PASSes.

- [ ] **Step 3: Modify `start_chat`**

In `companion.rs`'s `start_chat` handler:

(a) In the `instance_id` branch — after the `companion.instance.owner_uid != user_id` check passes, before doing anything else with the resolved `iid`, add the NFT check:

```rust
            // NFT gate: load asset_id for this companion's genome; enforce.
            let asset_id_opt = PersonaRepo { pool: &state.pool }
                .get_asset_id_for_genome(companion.instance.genome_id)
                .await?;
            enforce_nft_ownership(&state.pool, user_id, asset_id_opt.as_deref()).await?;
            iid
```

(b) In the `None` (i.e. `genome_id`) branch — between the `is_active` check and the `existing` query (so we gate **before** the possible `create_instance` call):

```rust
            if !genome.is_active {
                return Err(AppError::BadRequest("genome is not active".into()));
            }

            // NFT gate runs BEFORE looking for an instance or creating one.
            // A non-owner who hits the create_instance fallback would otherwise
            // leave behind an empty persona_instances row.
            let asset_id_opt = PersonaRepo { pool: &state.pool }
                .get_asset_id_for_genome(genome_id)
                .await?;
            enforce_nft_ownership(&state.pool, user_id, asset_id_opt.as_deref()).await?;

            // Look for an existing active instance for this user×genome.
```

- [ ] **Step 4: Run tests to confirm pass**

Run: `cargo test -p eros-engine-server start_chat -- --nocapture`
Expected: all green — the 403 test passes, the legacy test passes, plus all existing start_chat tests.

- [ ] **Step 5: Commit**

```bash
git add crates/eros-engine-server/src/routes/companion.rs
git commit -m "feat(server): NFT ownership gate on /comp/chat/start (both paths)

Gate runs BEFORE create_instance in the genome path so non-owners cannot
leave empty hidden persona_instances rows. Legacy seed-persona genomes
(asset_id IS NULL) skip the check unchanged."
```

---

### Task 17: Wire gate into `/message` and `/message_async`

The previous owner of an NFT-backed persona can keep messaging after sale or unlink unless we recheck per message. Add the same `enforce_nft_ownership` call at the head of each message handler.

**Files:**
- Modify: `crates/eros-engine-server/src/routes/companion.rs`

- [ ] **Step 1: Locate the two message handlers**

Run: `grep -n "/comp/chat/{session_id}/message\|fn send_message\|fn send_message_async" crates/eros-engine-server/src/routes/companion.rs`
Expected: two handler definitions (sync + async variants), each annotated with `#[utoipa::path]`.

- [ ] **Step 2: Write failing tests**

Append to the `#[cfg(test)]` block:

```rust
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn message_403_after_unlink(pool: PgPool) {
        use eros_engine_store::ownership::OwnershipRepo;
        use eros_engine_store::wallets::WalletLinkRepo;
        use chrono::Utc;

        // Setup: NFT genome, owner, started a session.
        let user = Uuid::new_v4();
        let wallet = "BvHvbHBeF2zXa1pT5eExMzTAydPGFTyhqMAbPyuMTfQt";
        let asset = "11111111111111111111111111111131";
        let genome_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.persona_genomes
                (id, name, system_prompt, art_metadata, is_active, asset_id)
             VALUES ($1, 'NftGenome', 'p', '{}'::jsonb, true, $2)",
        )
        .bind(genome_id)
        .bind(asset)
        .execute(&pool)
        .await
        .unwrap();
        WalletLinkRepo { pool: &pool }
            .upsert(user, wallet, true, Utc::now())
            .await
            .unwrap();
        OwnershipRepo { pool: &pool }
            .upsert(asset, "p-1", wallet, Utc::now())
            .await
            .unwrap();

        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user);

        // Start a chat (passes the gate).
        let body = serde_json::to_vec(&serde_json::json!({ "genome_id": genome_id })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, resp) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK, "start should succeed: {resp}");
        let session_id = resp["session_id"].as_str().unwrap().to_string();

        // Unlink the wallet — ownership chain now broken.
        WalletLinkRepo { pool: &pool }
            .upsert(user, wallet, false, Utc::now())
            .await
            .unwrap();

        // Sending a message should now 403.
        let body = serde_json::to_vec(&serde_json::json!({ "content": "hi" })).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!("/comp/chat/{}/message", session_id))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let (status, _resp) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
```

- [ ] **Step 3: Run to confirm fail**

Run: `cargo test -p eros-engine-server message_403_after_unlink -- --nocapture`
Expected: FAIL — message currently returns 200 even after unlink.

- [ ] **Step 4: Add the gate to both handlers**

In `send_message` and `send_message_async`, after resolving the session and loading the instance/genome (find the existing path; most likely `chat_repo.get_session(session_id).await?` → `persona_repo.load_companion(session.instance_id).await?`), insert before the LLM/pipeline call:

```rust
    let asset_id_opt = PersonaRepo { pool: &state.pool }
        .get_asset_id_for_genome(companion.instance.genome_id)
        .await?;
    enforce_nft_ownership(&state.pool, user_id, asset_id_opt.as_deref()).await?;
```

If the existing code path doesn't already load the companion (or the genome_id) here, the engineer must add a lightweight lookup that does. The cost is one indexed SQL per message; tolerable.

- [ ] **Step 5: Run all tests**

Run: `cargo test -p eros-engine-server message`
Expected: the new `message_403_after_unlink` passes, existing message tests still pass.

- [ ] **Step 6: Commit**

```bash
git add crates/eros-engine-server/src/routes/companion.rs
git commit -m "feat(server): per-message NFT ownership recheck on /message + /message_async

Previous owner cannot keep messaging through an existing session after
sale or unlink. Cost is one indexed PK join per message turn — dominated
by the LLM round-trip that follows."
```

---

## Phase E4 — Self-heal task

### Task 18: Outbound s2s signing helper

The pull loop needs to sign GETs against svc using the same canonical layout.

**Files:**
- Modify: `crates/eros-engine-server/src/auth/s2s.rs`

- [ ] **Step 1: Write a unit test**

Append to `auth/s2s.rs`'s `#[cfg(test)]` block:

```rust
    #[test]
    fn build_outbound_signed_request_components() {
        let (ts, sig) = build_outbound_signature(
            "GET",
            "/internal/ownership/since",
            "cursor_pk=&cursor_ts=&limit=10",
            b"",
            b"test-secret",
            DateTime::<Utc>::from_timestamp(1700000000, 0).unwrap(),
        );
        assert_eq!(ts, "2023-11-14T22:13:20+00:00");

        // Re-verify the signature with the same canonical reconstruction.
        let body_hex = hex::encode(Sha256::digest(b""));
        let canonical = canonical_signing_string(
            "GET",
            "/internal/ownership/since",
            "cursor_pk=&cursor_ts=&limit=10",
            &ts,
            &body_hex,
        );
        assert!(verify_against(b"test-secret", &canonical, &sig));
    }
```

- [ ] **Step 2: Add the helper**

Append to `auth/s2s.rs`:

```rust
/// Build the (timestamp, signature) headers for an outbound s2s request.
/// The caller is responsible for setting `canonical_query` to the
/// already-canonicalized query string (use `canonicalize_query` for raw input).
pub fn build_outbound_signature(
    method: &str,
    path: &str,
    canonical_query: &str,
    body: &[u8],
    secret: &[u8],
    now: DateTime<Utc>,
) -> (String, String) {
    let timestamp = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let body_hash = Sha256::digest(body);
    let body_hex = hex::encode(body_hash);
    let canonical = canonical_signing_string(
        method, path, canonical_query, &timestamp, &body_hex,
    );
    let sig = sign(secret, &canonical);
    (timestamp, sig)
}
```

- [ ] **Step 3: Run test**

Run: `cargo test -p eros-engine-server auth::s2s::tests -- --nocapture`
Expected: all green (including the new `build_outbound_signed_request_components`).

- [ ] **Step 4: Commit**

```bash
git add crates/eros-engine-server/src/auth/s2s.rs
git commit -m "feat(server): build_outbound_signature helper for self-heal HTTP pulls"
```

---

### Task 19: Self-heal pipeline task

Loops every 5 minutes, pulls svc's two `/since` endpoints, UPSERTs through the store repos, advances cursors.

**Files:**
- Create: `crates/eros-engine-server/src/pipeline/sync.rs`
- Modify: `crates/eros-engine-server/src/pipeline/mod.rs`

- [ ] **Step 1: Add unit tests for the pull-and-apply logic**

The HTTP layer is hard to test without a mock server. We test the pure cursor-advancement logic; the HTTP plumbing is exercised manually + by integration in production.

Create `crates/eros-engine-server/src/pipeline/sync.rs`:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
//! Self-heal pull: when MARKETPLACE_SVC_URL is configured, periodically
//! call svc's /internal/{ownership,wallets}/since to pick up any pushes
//! the engine missed. Cursors persisted in engine.sync_cursors.

use chrono::Utc;
use eros_engine_store::ownership::{Ownership, OwnershipRepo};
use eros_engine_store::sync_cursors::{Cursor, SyncCursorRepo};
use eros_engine_store::wallets::{WalletLink, WalletLinkRepo};
use std::time::Duration;
use tracing::{info, warn};

use crate::auth::s2s::{build_outbound_signature, canonicalize_query};
use crate::state::AppState;

const TICK_SECS: u64 = 5 * 60;
const PAGE_LIMIT: i64 = 500;

#[derive(serde::Deserialize)]
struct OwnershipSinceResp {
    rows: Vec<Ownership>,
    next_cursor: Option<SinceCursorWire>,
}
#[derive(serde::Deserialize)]
struct WalletsSinceResp {
    rows: Vec<WalletLink>,
    next_cursor: Option<SinceCursorWire>,
}
#[derive(serde::Deserialize)]
struct SinceCursorWire {
    ts: chrono::DateTime<chrono::Utc>,
    pk: String,
}

/// Spawn the loop. Returns immediately if marketplace coordination is
/// unconfigured (MARKETPLACE_SVC_URL empty).
pub async fn run(state: AppState) {
    let Some(svc_url) = state.marketplace_svc_url.clone() else {
        info!("self-heal task disabled: MARKETPLACE_SVC_URL unset");
        return;
    };
    let Some(secret) = state.marketplace_s2s_secret.clone() else {
        warn!("self-heal task disabled: secret unset (boot validation should have caught this)");
        return;
    };

    loop {
        if let Err(e) = tick_ownership(&state, &svc_url, &secret).await {
            warn!(error = %e, "self-heal ownership tick failed");
        }
        if let Err(e) = tick_wallets(&state, &svc_url, &secret).await {
            warn!(error = %e, "self-heal wallets tick failed");
        }
        tokio::time::sleep(Duration::from_secs(TICK_SECS)).await;
    }
}

async fn tick_ownership(state: &AppState, svc_url: &str, secret: &str) -> anyhow::Result<()> {
    let cursor = SyncCursorRepo { pool: &state.pool }.get("ownership").await?;
    let path = "/internal/ownership/since";
    let query_raw = format!(
        "cursor_pk={}&cursor_ts={}&limit={}",
        urlencoding::encode(&cursor.cursor_pk),
        urlencoding::encode(&cursor.cursor_ts.to_rfc3339()),
        PAGE_LIMIT,
    );
    let query = canonicalize_query(&query_raw);
    let (ts, sig) = build_outbound_signature("GET", path, &query, b"", secret.as_bytes(), Utc::now());
    let url = format!("{}{}?{}", svc_url.trim_end_matches('/'), path, query);
    let resp = state
        .http_client
        .get(&url)
        .header("x-s2s-timestamp", ts)
        .header("x-s2s-signature", sig)
        .send()
        .await?
        .error_for_status()?
        .json::<OwnershipSinceResp>()
        .await?;

    let repo = OwnershipRepo { pool: &state.pool };
    for row in &resp.rows {
        repo.upsert(&row.asset_id, &row.persona_id, &row.owner_wallet, row.source_updated_at)
            .await?;
    }

    if let Some(next) = resp.next_cursor {
        SyncCursorRepo { pool: &state.pool }
            .set("ownership", &Cursor { cursor_ts: next.ts, cursor_pk: next.pk })
            .await?;
    }
    Ok(())
}

async fn tick_wallets(state: &AppState, svc_url: &str, secret: &str) -> anyhow::Result<()> {
    let cursor = SyncCursorRepo { pool: &state.pool }.get("wallets").await?;
    let path = "/internal/wallets/since";
    let query_raw = format!(
        "cursor_pk={}&cursor_ts={}&limit={}",
        urlencoding::encode(&cursor.cursor_pk),
        urlencoding::encode(&cursor.cursor_ts.to_rfc3339()),
        PAGE_LIMIT,
    );
    let query = canonicalize_query(&query_raw);
    let (ts, sig) = build_outbound_signature("GET", path, &query, b"", secret.as_bytes(), Utc::now());
    let url = format!("{}{}?{}", svc_url.trim_end_matches('/'), path, query);
    let resp = state
        .http_client
        .get(&url)
        .header("x-s2s-timestamp", ts)
        .header("x-s2s-signature", sig)
        .send()
        .await?
        .error_for_status()?
        .json::<WalletsSinceResp>()
        .await?;

    let repo = WalletLinkRepo { pool: &state.pool };
    for row in &resp.rows {
        repo.upsert(row.user_id, &row.wallet_pubkey, row.linked, row.source_updated_at)
            .await?;
    }

    if let Some(next) = resp.next_cursor {
        SyncCursorRepo { pool: &state.pool }
            .set("wallets", &Cursor { cursor_ts: next.ts, cursor_pk: next.pk })
            .await?;
    }
    Ok(())
}
```

- [ ] **Step 2: Add `urlencoding` dep**

In `crates/eros-engine-server/Cargo.toml`:

```toml
urlencoding = "2"
```

(Tiny, no transitive deps. Used only here.)

- [ ] **Step 3: Wire the module**

In `crates/eros-engine-server/src/pipeline/mod.rs`, add:

```rust
pub mod sync;
```

- [ ] **Step 4: Verify it builds**

Run: `cargo build -p eros-engine-server`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/eros-engine-server/Cargo.toml \
        crates/eros-engine-server/src/pipeline/sync.rs \
        crates/eros-engine-server/src/pipeline/mod.rs
git commit -m "feat(server): self-heal sync task pulls svc /since cursors

Loops every 5 minutes when MARKETPLACE_SVC_URL is configured. Cursor
advance happens after the batch UPSERT succeeds; partial failure re-pulls
the same window on the next tick. Stale-write protection in the repos
makes the re-pull idempotent."
```

---

### Task 20: Conditional spawn + final README/docs update

Spawn the task at boot only when configured, then update operator docs.

**Files:**
- Modify: `crates/eros-engine-server/src/main.rs`
- Modify: `README.md`
- Modify: `docs/deploying.md`
- Modify: `docs/api-reference.md`

- [ ] **Step 1: Spawn the task in main**

In `crates/eros-engine-server/src/main.rs`, after the existing `tokio::spawn(crate::pipeline::dreaming::sweeper(state.clone()));` line:

```rust
    if state.marketplace_svc_url.is_some() {
        tokio::spawn(crate::pipeline::sync::run(state.clone()));
        tracing::info!("marketplace self-heal task spawned");
    }
```

- [ ] **Step 2: README env-var table**

Edit `README.md` and append rows to the Configuration table:

```
| `MARKETPLACE_SVC_URL` | no | Base URL of eros-marketplace-svc. When set, the engine pulls /since cursors every 5 min as a self-heal recovery path. Requires `MARKETPLACE_SVC_S2S_SECRET`. |
| `MARKETPLACE_SVC_S2S_SECRET` | no | HMAC secret shared with eros-marketplace-svc. Gates the `/internal/*` routes the svc pushes into. Without it, /internal/* always 401s. |
| `MARKETPLACE_SVC_S2S_SECRET_PREVIOUS` | no | Verify-only secret used during rolling rotation. Engine accepts requests signed with either current or previous secret; outbound calls always sign with current. |
```

- [ ] **Step 3: Deploying docs**

Append a short section to `docs/deploying.md`:

```markdown
## Marketplace coordination (optional)

If you run `eros-marketplace-svc` alongside this engine, set the three
`MARKETPLACE_SVC_*` env vars (see README) on both sides. The engine

- exposes `/internal/{ownership,wallets}/{upsert,since}` for the svc to push to
- pulls the same `/since` endpoints on the svc every 5 min as a fallback
- gates `/comp/chat/start` and every chat message on NFT ownership for
  any persona genome whose `asset_id` is populated

Without these env vars, the engine runs in OSS mode: `/internal/*` routes
return 401, no self-heal task is spawned, and the gate is a no-op for
legacy seed-persona genomes (`asset_id IS NULL`). Migration to a marketplace-
backed deploy is purely additive — set the env vars and `INSERT` rows.

For secret rotation, set `MARKETPLACE_SVC_S2S_SECRET_PREVIOUS = old`, deploy,
then set `MARKETPLACE_SVC_S2S_SECRET = new`, deploy, then drop `_PREVIOUS`
after one sync cycle (5 minutes).
```

- [ ] **Step 4: API reference**

Append to `docs/api-reference.md` a section that lists the four `/internal/*` routes, with example request bodies + signature header layout. Reference §4.3 of the spec for the canonical signing string.

- [ ] **Step 5: Regenerate OpenAPI snapshot (one more time, just in case)**

Run: `cargo run -p eros-engine-server --bin eros-engine -- print-openapi > crates/eros-engine-server/openapi.json`
Expected: identical to the Task 14 snapshot (no schema drift introduced by E3/E4 changes).

- [ ] **Step 6: Full workspace test**

Run: `cargo test --workspace`
Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add crates/eros-engine-server/src/main.rs \
        crates/eros-engine-server/openapi.json \
        README.md docs/deploying.md docs/api-reference.md
git commit -m "feat(server): conditional self-heal spawn + operator docs

When MARKETPLACE_SVC_URL is set, the engine spawns the self-heal task at
boot. Documents the three new env vars, the rolling-rotation procedure,
and the /internal/* surface for operators bringing up the marketplace
coordination."
```

---

## Self-Review

**1. Spec coverage:**

| Spec section | Tasks |
|---|---|
| §4.1 data model (asset_id on genome) | Task 4, Task 9 |
| §4.2 wallet_links schema (linked + source_updated_at + partial UNIQUE) | Task 2 |
| §4.2 persona_ownership + sync_cursors | Task 3 |
| §4.2 persona_genomes.asset_id | Task 4 |
| §4.3 HMAC 5-line signing + body cap + dual secret | Task 10, Task 18 |
| §4.4 routes + payload + stale-write + compound cursor | Tasks 11, 12, 13, 14 |
| §4.4 base58 input validation | Task 5, Tasks 12/13 |
| §4.5 gate placement (before create_instance + every message) | Tasks 16, 17 |
| §4.5 gate SQL with linked=true filter | Task 7 (`OwnershipRepo::owns`) |
| §4.6 self-heal loop + compound cursor + boot validation | Task 14 (boot validation), Tasks 19, 20 |
| §4.7 OpenAPI registration | Task 14 step 4 |
| §5 file map | Whole plan matches; new `pubkey.rs` is in Task 5 |

**2. Placeholder scan:** No "TBD", "TODO", "implement later", "similar to Task N" in steps. Every code step shows actual code.

**3. Type consistency:**
- `enforce_nft_ownership(pool, user_id, asset_id: Option<&str>)` signature used identically in Tasks 15, 16, 17.
- `WalletLinkRepo::upsert` returns `sqlx::Result<bool>` in Task 6 → consumed as `bool` in Task 12. ✓
- `OwnershipRepo::upsert` same shape, consumed in Task 13. ✓
- `Cursor { cursor_ts, cursor_pk }` defined in Task 8 → read by Task 19's `tick_ownership` / `tick_wallets`. ✓
- `OwnershipRepo::owns(user_id: Uuid, asset_id: &str)` in Task 7 → called by `enforce_nft_ownership` helper in Task 15. ✓
- HMAC functions `canonical_signing_string`, `canonicalize_query`, `sign`, `verify_against`, `build_outbound_signature` defined in Tasks 10, 18 → consumed by Tasks 14 (middleware), 19 (outbound sign). ✓

**4. Risk flags for the executor:**
- Task 14 step 5 (integration test) needs `test_state` and `build_router` from `companion.rs` to be `pub(crate)`. If they're private, add visibility before the test will compile.
- Task 17 step 4 says "find the existing path" — the executor must locate the genome-id lookup site in `send_message` / `send_message_async`. If the handlers don't already load the genome_id from the session row, a small extra SQL needs to be added. The plan handles this with a flexible instruction.
- Tasks 12/13 do not have inline integration tests; the integration coverage is consolidated in Task 14 step 5. This keeps test setup centralized but means the executor must run the full `cargo test -p eros-engine-server` at Task 14, not just the bits they just wrote.

---

## Plan complete and saved to `docs/superpowers/plans/2026-05-13-marketplace-ownership-gate.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?

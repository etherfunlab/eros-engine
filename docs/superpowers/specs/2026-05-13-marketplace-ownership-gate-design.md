# `eros-engine` — Marketplace ownership gate

**Date:** 2026-05-13
**Status:** Draft (pending user review)
**Scope:** Add NFT-ownership awareness to `eros-engine` so `eros-marketplace-svc` (P4) can gate chat access on `(asset_id, owner_wallet, user_id)` without coupling the OSS engine to the closed-source marketplace.

## 1. Why this PR exists

`eros-marketplace-svc`'s design (`eros-nft/docs/superpowers/specs/2026-05-13-eros-marketplace-svc-design.md`, §4.5) lists five engine surfaces that **do not exist today** and that the svc's P4 depends on:

- `engine.persona_ownership` table — mirror of marketplace-side ownership
- `engine.wallet_links` table — mirror of marketplace-side wallet bindings
- `POST /internal/{ownership,wallets}/upsert` — svc-to-engine push endpoints
- `GET  /internal/{ownership,wallets}/since` — engine-to-svc self-heal pull
- Gate on `POST /comp/chat/start` — reject if caller doesn't own the persona's NFT

Until those land, the marketplace can mint cNFTs and reconcile ownership in its own DB, but `eros-engine` will happily let any signed-in user start a chat with any persona — the chat access gate that justifies the NFT does not exist.

This PR adds those surfaces. The OSS engine continues to be usable without a marketplace.

## 2. Goals

1. Mirror marketplace ownership + wallet-binding state inside `engine.*` schema so chat-access decisions are a single SQL join, not a remote call.
2. Expose HMAC-authenticated `/internal/*` endpoints for svc → engine push and engine ↔ svc self-heal pull, with no Supabase JWT involvement.
3. Gate `POST /comp/chat/start` on NFT ownership **only when** the requested genome carries an `asset_id`. Genomes without `asset_id` (seed-persona TOML loads, existing OSS deployments) keep current behavior unchanged.
4. Preserve OSS independence: an engine deploy without `MARKETPLACE_SVC_URL` configured runs identically to today, with no marketplace coupling at runtime.

## 3. Non-goals (this PR)

- **Engine-side ingestion of marketplace persona content.** Decrypting `prompt_ciphertext_ref` via KMS, fetching ciphertext, and materializing the prompt into `persona_genomes.system_prompt` is a separate PR with its own KMS provider integration. This PR only handles the access gate.
- **Frontend wallet linking flow.** `eros-engine-web`'s `/me/wallets/*` UI plus wallet-adapter signature flow live in that repo; this PR exposes only the engine-side data plane that mirrors svc state.
- **Bulk ownership queries.** "Which assets does user U own?" is a single-row probe per chat-start in v0.1. Aggregate endpoints (catalog joins, batch ownership) come later.
- **Eros-nft-extended (trained-persona transfer).** This PR's `asset_id` on `genomes` deliberately leaves room for a future instance-level transfer surface; we do not pre-build it.
- **OpenAPI changes for `/comp/*`.** The bearer security scheme stays as-is. Only `/internal/*` adds a new `hmac_signature` security scheme.

## 4. Architecture

### 4.1 Data model: `asset_id` on `persona_genomes`, not on `persona_instances`

`persona_genomes` is the character template (system prompt, name, behavior). `persona_instances` is the per-user state row (affinity vector, relationship memory). The marketplace NFT confers the right to *access* the character — to chat with this archetype — and explicitly does **not** confer the right to inherit someone else's relationship state with it. (The eros-nft v1 spec defers trained-persona transfer to `eros-nft-extended`.)

Therefore `asset_id` belongs on `genomes`. Consequences that fall out naturally:

| Behavior | This design |
|---|---|
| Seller A transfers asset X to buyer B | B's first chat-start auto-creates a fresh `persona_instance` with default affinity. A's old instance row stays — gate just denies A access. |
| Seller's chat history privacy | Automatic: B sees nothing of A's history because B has a different `instance_id`. |
| Engine-side migration code on transfer | Zero. Webhook lands an `UPSERT` into `persona_ownership`; nothing in `chat_sessions` / `chat_messages` / `affinity_*` moves. |
| Future `eros-nft-extended` (memory dossier transfer) | Slots in as instance-level state transfer on top of this design without conflicting with the access-level invariant. |

Bubblegum cNFTs encode `(merkle_tree, leaf_index)` — they identify a metadata claim, not a relationship. Mapping the NFT to a stateless character (`genome`) matches the on-chain semantics; mapping it to a stateful relationship (`instance`) would invent a state-transfer requirement the chain doesn't carry.

### 4.2 Schema migrations

Three new SQL files, numbered after the current head (`0008_session_classification_claim.sql`):

```sql
-- 0009_wallet_links.sql
CREATE TABLE engine.wallet_links (
    user_id        UUID         NOT NULL,
    wallet_pubkey  TEXT         NOT NULL,
    linked_at      TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, wallet_pubkey),
    UNIQUE (wallet_pubkey)
);
CREATE INDEX wallet_links_updated_at_idx
    ON engine.wallet_links (updated_at);
```

The `UNIQUE (wallet_pubkey)` constraint mirrors svc's invariant: a wallet maps to at most one user_id; a user may link many wallets. `updated_at` is indexed so `GET /internal/wallets/since` can stream by cursor without a full-table scan.

```sql
-- 0010_persona_ownership.sql
CREATE TABLE engine.persona_ownership (
    asset_id        TEXT         PRIMARY KEY,
    persona_id      TEXT         NOT NULL,
    owner_wallet    TEXT         NOT NULL,
    updated_at      TIMESTAMPTZ  NOT NULL DEFAULT now()
);
CREATE INDEX persona_ownership_owner_wallet_idx
    ON engine.persona_ownership (owner_wallet);
CREATE INDEX persona_ownership_persona_id_idx
    ON engine.persona_ownership (persona_id);
CREATE INDEX persona_ownership_updated_at_idx
    ON engine.persona_ownership (updated_at);

-- sync_cursors lives in the same migration because the self-heal loop
-- (§4.6) advances one cursor per replicated entity (ownership, wallets)
-- and the cursor write is naturally a sibling of the row UPSERT.
CREATE TABLE engine.sync_cursors (
    name        TEXT         PRIMARY KEY,    -- 'ownership' | 'wallets'
    cursor      TIMESTAMPTZ  NOT NULL,
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT now()
);
```

`asset_id` is the natural PK (one row per cNFT). `persona_id` is denormalized for `genome ↔ asset_id` reconciliation and indexed for occasional reverse lookups.

```sql
-- 0011_persona_genome_asset_id.sql
ALTER TABLE engine.persona_genomes
    ADD COLUMN asset_id TEXT NULL;
CREATE UNIQUE INDEX persona_genomes_asset_id_uidx
    ON engine.persona_genomes (asset_id)
    WHERE asset_id IS NOT NULL;
```

Nullable to keep every existing seed-persona genome untouched. Partial unique index enforces 1 asset = 1 genome without blocking the legacy `NULL` rows from coexisting.

### 4.3 HMAC s2s middleware

New `crates/eros-engine-server/src/auth/s2s.rs`, parallel to the existing JWT `auth/middleware.rs`. Mounted on `/internal/*` only.

**Verification:**

```
1. Read X-S2S-Timestamp header. Reject if missing or > 5 min from server clock.
2. Read X-S2S-Signature header (hex). Reject if missing.
3. Compute expected = HMAC-SHA256(
       key   = MARKETPLACE_SVC_S2S_SECRET,
       data  = b"{timestamp}\n" || raw_body_bytes
   )
4. constant_time_eq(expected, provided). Reject on mismatch.
5. Pass through to handler.
```

The raw body must be buffered up-front (axum middleware reads `Bytes` from the request body before passing to handlers); the handler reads from the buffered body, not a stream. This matches the security model of the marketplace-svc Helius webhook handler — the engine's signature verification follows the same shape so the two systems are operationally symmetric.

`MARKETPLACE_SVC_S2S_SECRET` gates verification, period. When the secret is unset, every `/internal/*` request fails the HMAC compare step (no expected signature can be computed) and returns `401`. This is the intended OSS-only posture: the routes are mounted for API discoverability and uniform behavior, but reject in practice. `MARKETPLACE_SVC_URL` is separate — it only controls whether the self-heal pull task spawns (§4.6). A deploy that wants to **receive** pushes but not poll svc back can set the secret without the URL; a deploy that wants both sets both.

### 4.4 S2S routes

New `crates/eros-engine-server/src/routes/internal.rs`. All four routes share the s2s middleware layer.

| Method | Path | Body / query | Response |
|---|---|---|---|
| `POST` | `/internal/wallets/upsert` | `{ user_id: UUID, wallet_pubkey: String, linked: bool }` | `204 No Content` |
| `GET` | `/internal/wallets/since` | `?cursor=<RFC3339 timestamp>&limit=<1..1000>` (default 100) | `{ rows: [WalletLink…], next_cursor: <RFC3339 or null> }` |
| `POST` | `/internal/ownership/upsert` | `{ asset_id: String, persona_id: String, owner_wallet: String }` | `204 No Content` |
| `GET` | `/internal/ownership/since` | `?cursor=<RFC3339 timestamp>&limit=<1..1000>` (default 100) | `{ rows: [Ownership…], next_cursor: <RFC3339 or null> }` |

Write semantics:

- `wallets/upsert`: `linked=true` → `INSERT ON CONFLICT (user_id, wallet_pubkey) DO UPDATE SET updated_at = now()`. `linked=false` → `DELETE`. The unlink path does not invalidate on-chain state; it only removes engine-side access. (Re-linking is just another `linked=true`.)
- `ownership/upsert`: `INSERT ON CONFLICT (asset_id) DO UPDATE SET persona_id, owner_wallet, updated_at`. The `(asset_id, owner_wallet)` history is not preserved; engine only cares about the current owner.

Read semantics:

- `since`: `SELECT … WHERE updated_at > $cursor ORDER BY updated_at ASC LIMIT $limit`. `next_cursor = max(updated_at) of returned rows`. When fewer rows than `limit` come back, the caller is caught up.

The handlers are thin SQL — the bulk of new logic is the gate (§4.5) and the optional self-heal task (§4.6).

### 4.5 Gate on `POST /comp/chat/start`

Slot the new check into `routes/companion.rs::start_chat`, **after** the existing instance/genome resolution (which already validates user-owns-instance) and **before** the session resume/create.

```rust
// (existing) instance_id is resolved; genome is loaded via persona_repo.
if let Some(asset_id) = &companion.genome.asset_id {
    let owns: bool = sqlx::query_scalar(
        "SELECT EXISTS (
           SELECT 1
             FROM engine.persona_ownership po
             JOIN engine.wallet_links wl
               ON wl.wallet_pubkey = po.owner_wallet
            WHERE po.asset_id = $1
              AND wl.user_id = $2
         )"
    )
    .bind(asset_id)
    .bind(user_id)
    .fetch_one(&state.pool)
    .await?;

    if !owns {
        return Err(AppError::Forbidden(
            "nft ownership required for this persona".into(),
        ));
    }
}
// (existing) resume or create session, return StartChatResponse.
```

When `genome.asset_id IS NULL` (every seed-persona today), the block is skipped entirely and behavior is bit-identical to current.

The join is a single PK lookup on `persona_ownership.asset_id` followed by an index lookup on `wallet_links.wallet_pubkey`. Sub-millisecond on the existing pool.

`AppError::Forbidden` already maps to `403` in the engine's error layer; the response body shape stays standard.

### 4.6 Optional self-heal task

When `MARKETPLACE_SVC_URL` is configured, the server spawns a background task at boot that periodically pulls svc's `since` endpoints and replays them through the same internal repos used by `/internal/*/upsert`. This is the recovery path for missed pushes; the **primary** path is svc → engine push.

```
loop every 5 minutes:
    fetch GET {svc_url}/internal/ownership/since?cursor={cursor_o}
        with X-S2S-Signature(MARKETPLACE_SVC_S2S_SECRET, …)
    UPSERT each row via OwnershipRepo
    cursor_o = response.next_cursor or unchanged

    fetch GET {svc_url}/internal/wallets/since?cursor={cursor_w}
        same shape
    UPSERT/DELETE each row via WalletLinkRepo
    cursor_w = response.next_cursor or unchanged
```

Cursors persist in `engine.sync_cursors` (created alongside `persona_ownership` in 0010 — see §4.2). Two rows: `name = 'ownership'`, `name = 'wallets'`. The self-heal loop reads its cursor at the top of each tick and writes the advanced cursor only after the UPSERTs for that batch succeed; if a batch fails mid-way, the cursor stays where it was and the next tick replays the same window (UPSERTs are idempotent).

If `MARKETPLACE_SVC_URL` is unset, the task is not spawned. Same outcome as today's OSS deploy.

### 4.7 OpenAPI

The four `/internal/*` routes get `#[utoipa::path]` annotations with a new `security = [("hmac_signature" = [])]` scheme alongside the existing `bearer`. Each route lives under an `internal` tag so the Scalar UI groups them apart from `/comp/*`. The drift-check CI (added in `9fd3499`) catches any handler that forgets the annotation.

## 5. Crate impact

Only `eros-engine-server` and `eros-engine-store` change. `-core` and `-llm` are untouched.

```
crates/eros-engine-server/src/
  auth/
    middleware.rs    # unchanged
    s2s.rs           # NEW: HMAC verifier
    mod.rs           # add `pub mod s2s;`
  routes/
    companion.rs     # MODIFY: gate inside start_chat
    internal.rs      # NEW: 4 routes + handlers
    mod.rs           # MODIFY: merge internal subrouter under s2s layer
  state.rs           # MODIFY: marketplace_svc_url, marketplace_s2s_secret, http_client
  pipeline/
    sync.rs          # NEW: optional self-heal loop
    mod.rs           # MODIFY: spawn sync task if configured
  main.rs            # MODIFY: env var wiring
  openapi.rs         # MODIFY: register hmac_signature security scheme

crates/eros-engine-store/src/
  ownership.rs       # NEW: OwnershipRepo (upsert, since)
  wallets.rs         # NEW: WalletLinkRepo (upsert, delete, since)
  persona.rs         # MODIFY: PersonaGenome struct + repo gains asset_id
  sync_cursors.rs    # NEW: tiny read/write for cursors
  lib.rs             # MODIFY: re-export new repos
  migrations/
    0009_wallet_links.sql              # NEW
    0010_persona_ownership.sql         # NEW (includes sync_cursors)
    0011_persona_genome_asset_id.sql   # NEW
```

Approximately 11 files: 7 new, 4 modified. No `-core` or `-llm` touch.

## 6. Phasing (E1–E4, each independently shippable)

| Phase | Scope | Definition of done |
|---|---|---|
| **E1 — Schema + repos** | Three migrations, `OwnershipRepo`, `WalletLinkRepo`, `PersonaGenome.asset_id` plumbing, sync_cursors helper | sqlx unit tests for UPSERT + `since` cursor pagination green; CI green |
| **E2 — S2S middleware + routes** | `auth/s2s.rs`, `routes/internal.rs`, OpenAPI registration, env wiring | Integration tests: HMAC pass/fail, timestamp skew rejection, body-tamper rejection, idempotent UPSERT |
| **E3 — Chat-start gate** | `start_chat` gate block, tests | New tests: legacy genome (asset_id NULL) still passes; NFT genome without wallet_link rejects 403; NFT genome with correct ownership chain passes |
| **E4 — Self-heal task** | `pipeline/sync.rs`, conditional spawn in `main.rs`, env var docs | Configured deploy pulls svc cursors; unconfigured deploy doesn't spawn; logs include cursor advancement |

E1 lands first (data model). E2 unlocks svc → engine push. E3 closes the gate. E4 is the recovery path; svc's P4 can ship with E1–E3 alone, treating E4 as immediate follow-up.

## 7. Key decisions

| # | Decision | Why | What changes if revisited |
|---|---|---|---|
| 1 | OSS engine remains usable without a marketplace; `MARKETPLACE_SVC_URL` is optional | AGPL OSS posture; nobody should be forced to run a marketplace to use the chat engine | Making it required would mean the legacy seed-persona TOML flow has to keep working anyway, so requirement gains nothing |
| 2 | `asset_id` on `persona_genomes`, NOT `persona_instances` | NFT v1 confers character access, not relationship transfer. Buyer gets fresh affinity, seller's chat history stays private, zero engine-side data migration on sale | Putting it on instances would invent a state-transfer requirement that eros-nft v1 explicitly defers to `eros-nft-extended` |
| 3 | Mirror svc state (push + cursor pull), not call svc per chat-start | Decouples chat latency from svc availability; engine remains the gate decision point | Pulling per chat-start adds an HTTP hop to the hot path and a hard dependency on svc uptime |
| 4 | Two tables (`wallet_links` + `persona_ownership`), not one denormalized `persona_access(user_id, asset_id)` | Engine keeps raw data for future joins (e.g., "all assets a user owns"); svc doesn't have to maintain a Cartesian-product view | Denormalizing in svc would explode write amplification on every wallet link / ownership change |
| 5 | HMAC-SHA256 + timestamp window, not mTLS or JWT | Lowest deployment friction; matches svc's Helius webhook pattern; per-deploy secret rotates cleanly | mTLS would require cert distribution + termination policy at every reverse proxy hop |
| 6 | `asset_id TEXT`, not `BYTEA` | Solana pubkeys are conventionally base58 strings; engine doesn't decode them; downstream tooling (logs, Grafana) is text-friendly | Switching to BYTEA later means a one-shot migration if performance demands it; not on the horizon |
| 7 | Legacy seed-persona genomes (`asset_id IS NULL`) are permanently exempt from the gate | Zero regression for current OSS users; the gate adds capability, doesn't remove it | Requiring all genomes to be NFT-backed would break every public OSS deploy |
| 8 | Self-heal is pull (engine → svc), not double-push (svc retries) | Engine controls its own catch-up cadence; svc isn't responsible for guaranteeing eventual delivery of every push | If we move to event-streaming infra later, the cursor pull is replaced by stream resume — same shape |
| 9 | `/internal/*` exists in OSS engine even without `MARKETPLACE_SVC_S2S_SECRET` (rejects in practice) | API discoverability; anyone running their own marketplace can use the surface by setting their own secret | Gating the route registration on env presence is silently confusing — better to mount and reject |

## 8. Risks

| Risk | Mitigation |
|---|---|
| **svc push fails and self-heal disabled (env unset) → engine state permanently stale** | Self-heal can't be selectively disabled; you either configure the svc URL (which auto-enables it) or run pure OSS (no marketplace coordination at all). No half-state. |
| **HMAC secret leaks** | Secret lives in fly secrets / Supabase env vault, never in code or CI logs. Rotation runbook mirrors svc admin-key rotation: deploy new secret to both engine and svc within a rolling window. |
| **Engine receives ownership row for an `asset_id` whose `persona_genomes.asset_id` hasn't been pushed yet** | Gate join falls through to empty → 403 on chat-start. Once the genome's `asset_id` is populated (via the future content-ingestion PR), retry from the user side succeeds. This is acceptable v0.1 behavior; the content-ingestion PR will sequence pushes properly. |
| **`/comp/chat/start` latency from the new gate join** | Single indexed PK join → ~ms. Pool already sized for chat traffic. No measurable impact. |
| **Webhook-style replay attacks against `/internal/*`** | Timestamp window (±5 min) + per-body HMAC means replaying a stale signed request fails. Cursor `since` endpoints are idempotent reads, replays are safe by construction. |
| **Wallet rebinding race** (svc unlinks W from user_A, links W to user_B; engine sees them out of order) | `since` cursor is the only authoritative state. Out-of-order pushes converge to chain-of-history truth on the next self-heal sweep. Strict ordering not required because the only invariant is "current state matches chain." |
| **Migration on a live engine** | Migrations are additive (new tables + nullable column). Zero existing row affected. Roll-forward only. |

## 9. Out-of-scope (recorded for next-round PRs)

- **Marketplace persona content ingestion** — `svc → /internal/genomes/upsert` plus KMS-unwrap-on-chat-load to materialize encrypted prompts. Separate PR, blocks the user-visible chat-with-NFT loop end-to-end.
- **Frontend wallet-linking flow** — `eros-engine-web` adds wallet adapter, `/me/wallets/challenge` + `/me/wallets/confirm` client-side flow.
- **Bulk ownership queries** — admin or catalog routes that list "all assets owned by user U" / "all owners of genome G." Not needed by chat-gating.
- **`eros-nft-extended` trained-persona transfer** — instance-level state transfer would add a `POST /internal/instance-transfer` surface and migration of affinity/memory rows. Entirely separate spec lineage.

## 10. Files touched (when implemented)

```
A docs/superpowers/specs/2026-05-13-marketplace-ownership-gate-design.md  # this doc
A crates/eros-engine-store/migrations/0009_wallet_links.sql
A crates/eros-engine-store/migrations/0010_persona_ownership.sql
A crates/eros-engine-store/migrations/0011_persona_genome_asset_id.sql
A crates/eros-engine-store/src/ownership.rs
A crates/eros-engine-store/src/wallets.rs
A crates/eros-engine-store/src/sync_cursors.rs
M crates/eros-engine-store/src/persona.rs
M crates/eros-engine-store/src/lib.rs
A crates/eros-engine-server/src/auth/s2s.rs
M crates/eros-engine-server/src/auth/mod.rs
A crates/eros-engine-server/src/routes/internal.rs
M crates/eros-engine-server/src/routes/companion.rs
M crates/eros-engine-server/src/routes/mod.rs
M crates/eros-engine-server/src/state.rs
A crates/eros-engine-server/src/pipeline/sync.rs
M crates/eros-engine-server/src/pipeline/mod.rs
M crates/eros-engine-server/src/main.rs
M crates/eros-engine-server/src/openapi.rs
M docs/api-reference.md
M docs/deploying.md
M README.md                                                                 # add env vars
```

Approximately 21 files: 11 new, 10 modified.

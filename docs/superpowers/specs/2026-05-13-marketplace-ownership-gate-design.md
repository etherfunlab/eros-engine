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
    user_id            UUID         NOT NULL,
    wallet_pubkey      TEXT         NOT NULL,
    linked             BOOLEAN      NOT NULL DEFAULT true,
    linked_at          TIMESTAMPTZ  NOT NULL DEFAULT now(),
    source_updated_at  TIMESTAMPTZ  NOT NULL,
    updated_at         TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, wallet_pubkey)
);
-- partial unique: a wallet is bound to at most one user *at a time*.
-- Tombstones (linked=false) are excluded so a wallet can later re-bind to a
-- different user after the original user unlinks it. The svc enforces the
-- same invariant on its authoritative side.
CREATE UNIQUE INDEX wallet_links_active_pubkey_uidx
    ON engine.wallet_links (wallet_pubkey)
    WHERE linked = true;
-- compound index for cursor pagination: ORDER BY source_updated_at ASC,
-- then by (user_id, wallet_pubkey) as a stable tie-breaker.
CREATE INDEX wallet_links_source_updated_at_idx
    ON engine.wallet_links (source_updated_at, user_id, wallet_pubkey);
```

`linked` is the tombstone bit: unlink writes `linked=false`, never `DELETE`. This is required because `GET /internal/wallets/since` (the self-heal pull) cannot represent a deletion as the absence of a row — a downstream consumer that polls by cursor would never observe a missing row. Tombstones flow through `since` exactly like upserts. `source_updated_at` is the svc's authoritative event time, used both for cursor ordering and stale-write protection (§4.4); `updated_at` is local cache freshness for observability.

```sql
-- 0010_persona_ownership.sql
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
-- compound for cursor pagination: stable tie-break by asset_id (the PK)
CREATE INDEX persona_ownership_source_updated_at_idx
    ON engine.persona_ownership (source_updated_at, asset_id);

-- sync_cursors persists the (source_updated_at, pk) compound cursor per
-- replicated entity. Storing both halves of the compound key avoids the
-- same-timestamp page-boundary bug where two rows at identical
-- source_updated_at would split across pages and one would be missed.
CREATE TABLE engine.sync_cursors (
    name        TEXT         PRIMARY KEY,    -- 'ownership' | 'wallets'
    cursor_ts   TIMESTAMPTZ  NOT NULL,
    cursor_pk   TEXT         NOT NULL,
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT now()
);
```

`asset_id` is the natural PK (one row per cNFT) and the canonical join key linking to `persona_genomes.asset_id`. `persona_id` is **informational only** — denormalized from the on-chain `PersonaManifest` for reverse lookup and operator-facing logs/queries. The chat-start gate (§4.5) joins by `asset_id`, not `persona_id`; the spec deliberately does not depend on a `persona_id ↔ genome` integrity invariant because `persona_id` is a string from off-chain manifest content, while `persona_genomes.id` is a local UUID. A reconciliation report can spot-check mismatches but it is not a hard constraint.

**Stale-write protection (`source_updated_at`):** every UPSERT carries the authoritative `source_updated_at` from svc. A write is applied only if `incoming.source_updated_at > existing.source_updated_at`; older events are silently dropped. Without this, a reordered or replayed webhook can revert ownership from the new owner back to the previous one. See §4.4 for the SQL form.

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

**Canonical signing string:**

The signature is computed over a deterministic five-line ASCII canonicalization. Signing just the body is **not** safe: a valid signature on one endpoint's body could be replayed against another endpoint with the same body. Method, path, and query bind the signature to the specific request:

```
canonical_string = method + "\n"
                 + path + "\n"
                 + canonical_query + "\n"
                 + timestamp + "\n"
                 + body_sha256_hex
```

Where:
- `method` is uppercase (`GET`, `POST`).
- `path` is the request path with no query string (e.g. `/internal/ownership/since`).
- `canonical_query` is the query string with parameters sorted by name, percent-encoded per RFC 3986; empty string if no query.
- `timestamp` is the value of `X-S2S-Timestamp`, an RFC 3339 UTC timestamp.
- `body_sha256_hex` is the lowercase hex SHA-256 of the raw request body (`e3b0c4...` for the empty body — defined for `GET` requests).

**Verification:**

```
1. Read X-S2S-Timestamp. Reject if missing, malformed, or skew > 5 min.
2. Read X-S2S-Signature (hex). Reject if missing.
3. Read body with a hard cap of MAX_BODY_SIZE (1 MiB). Reject 413 if exceeded.
4. body_sha256 = SHA-256(raw_body_bytes).
5. Build canonical_string per the layout above.
6. For each candidate secret in [SVC_S2S_SECRET, SVC_S2S_SECRET_PREVIOUS]:
       if any is set:
           expected = HMAC-SHA256(secret, canonical_string)
           if constant_time_eq(expected, provided) → pass
7. If no candidate matched → reject 401.
8. Pass buffered body through to handler (handler reads from Bytes).
```

The 1 MiB cap exists because the middleware must buffer the full body **before** running the HMAC verification (signatures cover the whole body). Without the cap, the middleware is a memory DoS surface that bypasses any later body-size limits the JSON extractor would impose.

**Secret rotation:** `MARKETPLACE_SVC_S2S_SECRET` is the active signing secret. `MARKETPLACE_SVC_S2S_SECRET_PREVIOUS` (optional) is accepted for verification only — never for outgoing signing. Rotation procedure: (a) set `_PREVIOUS = current`, deploy; (b) set new `current`, deploy; (c) after svc-side rotation is also complete, unset `_PREVIOUS`. This avoids the dead window where one side has rotated and the other hasn't.

**OSS-mode posture:** `MARKETPLACE_SVC_S2S_SECRET` gates verification, period. When the secret is unset, every `/internal/*` request fails at step 6 (no candidate secret) and returns `401`. The routes are mounted for API discoverability and uniform behavior, but reject in practice. `MARKETPLACE_SVC_URL` controls whether the self-heal pull task spawns (§4.6); a deploy that wants to **receive** pushes but not poll svc back can set the secret without the URL.

### 4.4 S2S routes

New `crates/eros-engine-server/src/routes/internal.rs`. All four routes share the s2s middleware layer.

| Method | Path | Body / query | Response |
|---|---|---|---|
| `POST` | `/internal/wallets/upsert` | `{ user_id: UUID, wallet_pubkey: String, linked: bool, source_updated_at: RFC3339 }` | `204` or `409` if stale |
| `GET` | `/internal/wallets/since` | `?cursor_ts=<RFC3339>&cursor_pk=<user_id:wallet_pubkey>&limit=<1..1000>` | `{ rows: [WalletLink…], next_cursor: { ts, pk } or null }` |
| `POST` | `/internal/ownership/upsert` | `{ asset_id: String, persona_id: String, owner_wallet: String, source_updated_at: RFC3339 }` | `204` or `409` if stale |
| `GET` | `/internal/ownership/since` | `?cursor_ts=<RFC3339>&cursor_pk=<asset_id>&limit=<1..1000>` | `{ rows: [Ownership…], next_cursor: { ts, pk } or null }` |

**Input validation (boundary, before SQL):**

- `wallet_pubkey` and `owner_wallet` must base58-decode to exactly 32 bytes (Solana ed25519 pubkey). Reject `400 invalid_pubkey` otherwise.
- `asset_id` must base58-decode to exactly 32 bytes (Bubblegum asset id). Reject `400 invalid_asset_id` otherwise.
- `user_id` must parse as a valid UUID.
- `source_updated_at` must parse as RFC 3339 with timezone, not in the future by more than the 5-min skew tolerance.

This validation rejects malformed strings at the API boundary so non-canonical encodings (uppercase vs lowercase variants of the same key) cannot create logical duplicates or false 403s. The strings stored in `engine.*` tables are the canonical form returned by the base58 round-trip.

**Write semantics (stale-write protection):**

- `wallets/upsert`:
  ```sql
  INSERT INTO engine.wallet_links
        (user_id, wallet_pubkey, linked, linked_at, source_updated_at, updated_at)
  VALUES ($1, $2, $3, COALESCE($4, now()), $4, now())
  ON CONFLICT (user_id, wallet_pubkey) DO UPDATE
    SET linked            = EXCLUDED.linked,
        source_updated_at = EXCLUDED.source_updated_at,
        updated_at        = now()
    WHERE EXCLUDED.source_updated_at > engine.wallet_links.source_updated_at
  RETURNING xmax = 0 AS inserted;
  ```
  If `ON CONFLICT DO UPDATE WHERE …` finds no rows to update (because the incoming event is older than what's stored), the handler returns `409 stale_event` — telling svc the engine already has a newer view.

- `ownership/upsert`: same shape, keyed by `asset_id`.

**Read semantics (compound cursor):**

Cursor format: `(cursor_ts, cursor_pk)`. Initial call uses `cursor_ts = '1970-01-01T00:00:00Z'`, `cursor_pk = ''`.

```sql
SELECT … FROM engine.persona_ownership
 WHERE (source_updated_at, asset_id) > ($cursor_ts, $cursor_pk)
 ORDER BY source_updated_at ASC, asset_id ASC
 LIMIT $limit;
```

`next_cursor.ts = last row's source_updated_at`; `next_cursor.pk = last row's PK`. When fewer rows than `limit` return, `next_cursor` is `null` and the caller is caught up. The compound key is the standard fix for the "same-timestamp page boundary" bug — two rows at identical `source_updated_at` cannot split across pages because the tie-break by `asset_id` (PK) gives them a deterministic order.

The `wallets/since` cursor uses `asset_id`'s analogue: `cursor_pk = "{user_id}:{wallet_pubkey}"`.

**Unlink semantics:** `wallets/upsert` with `linked=false` writes a tombstone row, never `DELETE`. The partial unique index on `(wallet_pubkey) WHERE linked = true` allows a later re-link to a different user. The gate join filters `wl.linked = true`.

The handlers are thin SQL — the bulk of new logic is the gate (§4.5) and the optional self-heal task (§4.6).

### 4.5 Gate placement: chat-start + every message turn

NFT ownership can revoke mid-session (seller transfers the asset to a buyer; user unlinks the wallet that owns the asset). Checking only at `chat/start` would let a previous owner keep messaging through an existing session forever. The gate runs at **two** points:

1. `POST /comp/chat/start` — before any DB writes.
2. `POST /comp/chat/{session_id}/message` and `/message_async` — before each LLM call.

Both call sites share a helper `enforce_nft_ownership(pool, user_id, genome) -> Result<(), AppError>` defined in `routes/companion.rs`. The helper returns `Ok(())` immediately when `genome.asset_id IS NULL` (legacy seed-persona). The runtime cost per message is one indexed PK join (~ms).

**Chat-start gate placement:**

The existing `start_chat` handler has two code paths:
- `instance_id` provided: validates user-owns-instance, then loads companion.
- `genome_id` provided: looks up an existing instance OR `persona_repo.create_instance(genome_id, user_id)`.

The gate must run **before** `create_instance` in the genome path, otherwise a non-owner causes a hidden empty `persona_instances` row to be inserted on every failed request — wasting rows, polluting analytics, and exposing the create_instance side effect to unauthorized callers.

```rust
// path A: instance_id resolution (unchanged) — companion loaded with owner check.
// path B: genome_id resolution — REWRITE so gate runs before create_instance:
let genome = persona_repo.get_genome(genome_id).await?
    .ok_or_else(|| AppError::NotFound("genome not found".into()))?;
if !genome.is_active {
    return Err(AppError::BadRequest("genome is not active".into()));
}
enforce_nft_ownership(&state.pool, user_id, &genome).await?;   // NEW — before create_instance
// existing: search for active instance; create_instance if missing.

// path A: also run the gate after loading companion (instance was validated by owner_uid
// but the NFT-ownership invariant is orthogonal — owner_uid is the engine's user
// concept; nft ownership is the marketplace's wallet concept).
enforce_nft_ownership(&state.pool, user_id, &companion.genome).await?;
```

**Per-message gate placement:** In each of `send_message` and `send_message_async` handlers, load the genome via the session's `instance_id → genome_id` lookup that already happens (or add it if missing), then call `enforce_nft_ownership` before invoking the chat pipeline. A 403 here ends the session in practice — the client should surface a "no longer the owner of this character" UX.

**Gate SQL (the helper):**

```sql
SELECT EXISTS (
  SELECT 1
    FROM engine.persona_ownership po
    JOIN engine.wallet_links wl
      ON wl.wallet_pubkey = po.owner_wallet
   WHERE po.asset_id = $1
     AND wl.user_id  = $2
     AND wl.linked   = true                  -- exclude tombstones
)
```

`AND wl.linked = true` is mandatory: unlinking a wallet writes a tombstone row, and the gate must treat tombstones as "no link." Without this clause, a user who unlinked their wallet would still pass the gate.

`AppError::Forbidden` already maps to `403`. The error body's `reason` field carries `nft_ownership_required` so the frontend can render a specific UX rather than a generic 403.

### 4.6 Optional self-heal task

When `MARKETPLACE_SVC_URL` is configured, the server spawns a background task at boot that periodically pulls svc's `since` endpoints and replays them through the same internal repos used by `/internal/*/upsert`. This is the recovery path for missed pushes; the **primary** path is svc → engine push.

**Boot config validation:** if `MARKETPLACE_SVC_URL` is set but `MARKETPLACE_SVC_S2S_SECRET` is unset, the server fails boot with `anyhow::bail!("MARKETPLACE_SVC_URL set without MARKETPLACE_SVC_S2S_SECRET …")`. Silently spawning a task that will fail forever is the wrong default. Either both are configured (full coordination) or neither is (OSS-only). The inverse — secret without URL — is allowed: it means "accept pushes but don't poll back."

```
loop every 5 minutes:
    cursor_o = SELECT (cursor_ts, cursor_pk) FROM sync_cursors WHERE name='ownership'
    fetch GET {svc_url}/internal/ownership/since
        ?cursor_ts={cursor_o.ts}&cursor_pk={cursor_o.pk}&limit=500
        with X-S2S-Timestamp + X-S2S-Signature (signed per §4.3)
    for each row → OwnershipRepo::upsert with stale-write check (§4.4)
    if response.next_cursor is not null:
        UPDATE sync_cursors SET cursor_ts, cursor_pk, updated_at = now()
            WHERE name='ownership'
    (else caught up — no cursor advance)

    same for wallets, name='wallets', cursor_pk format "{user_id}:{wallet_pubkey}"
```

Cursors persist in `engine.sync_cursors` (created alongside `persona_ownership` in 0010 — see §4.2). The loop reads its cursor at the top of each tick and writes the advanced cursor only after the UPSERTs for that batch succeed; partial failure leaves the cursor where it was, and the next tick re-pulls the same window. Stale-write protection (§4.4) makes the re-pull idempotent — older events arriving after a newer one are dropped at the SQL `WHERE` clause.

If `MARKETPLACE_SVC_URL` is unset, the task is not spawned. Same outcome as today's OSS deploy.

### 4.7 OpenAPI

The four `/internal/*` routes get `#[utoipa::path]` annotations with a new `security = [("hmac_signature" = [])]` scheme alongside the existing `bearer`. Each route lives under an `internal` tag so the Scalar UI groups them apart from `/comp/*`. The drift-check CI (added in `9fd3499`) catches any handler that forgets the annotation.

## 5. Crate impact

Only `eros-engine-server` and `eros-engine-store` change. `-core` and `-llm` are untouched.

```
crates/eros-engine-server/src/
  auth/
    middleware.rs    # unchanged
    s2s.rs           # NEW: HMAC verifier (canonical 5-line, 1MiB body cap,
                     #      current + previous secret support)
    mod.rs           # add `pub mod s2s;`
  routes/
    companion.rs     # MODIFY: enforce_nft_ownership helper; gate at start_chat
                     #         BEFORE create_instance; per-message gate in
                     #         send_message + send_message_async
    internal.rs      # NEW: 4 routes + base58/UUID validators
    mod.rs           # MODIFY: merge internal subrouter OUTSIDE the JWT layer;
                     #         /healthz public, /comp/* JWT, /internal/* HMAC
  state.rs           # MODIFY: marketplace_svc_url, marketplace_s2s_secret,
                     #         marketplace_s2s_secret_previous, http_client
  pipeline/
    sync.rs          # NEW: optional self-heal loop with compound cursor
    mod.rs           # MODIFY: spawn sync task if configured
  main.rs            # MODIFY: env var wiring + boot config validation
                     #         (URL without secret → bail)
  openapi.rs         # MODIFY: register hmac_signature security scheme

crates/eros-engine-store/src/
  ownership.rs       # NEW: OwnershipRepo (upsert with stale-write guard, since)
  wallets.rs         # NEW: WalletLinkRepo (upsert/tombstone, since)
  persona.rs         # MODIFY: PersonaGenome struct + repo gains asset_id
  sync_cursors.rs    # NEW: read/write compound (cursor_ts, cursor_pk)
  pubkey.rs          # NEW: base58 32-byte validation + canonicalization helpers
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
| 10 | Ownership recheck runs on **every chat message**, not just `chat/start` | Without per-message recheck, a previous owner can keep chatting through an existing session after sale or unlink. The recheck is a single PK join (~ms); cost is dominated by the LLM round-trip that comes next | Session-expiry-on-ownership-change (mark sessions stale on webhook) would save the per-message check but requires extra schema + webhook side effects; not worth it at v0.1 message volume |
| 11 | Every `/internal/*/upsert` payload carries `source_updated_at` and SQL applies it as a stale-write guard | Webhooks and self-heal pulls can deliver events out of order; a `now()`-only write would let an older event silently revert ownership | Removing this guard re-introduces correctness bugs under any non-trivial concurrency or replay |
| 12 | Unlink uses a tombstone row (`linked = false`), not `DELETE` | `GET /since` cursor pull cannot represent deletion as absence — an engine that polls would never observe a missing row. Tombstones flow through the cursor exactly like inserts and updates | The partial unique index on `(wallet_pubkey) WHERE linked = true` keeps the "one wallet → one user" invariant intact |
| 13 | Compound cursor `(source_updated_at, pk)` for `/since` endpoints | A simple `updated_at >` cursor drops rows that share a timestamp across page boundaries. Compound key with PK tie-break is the standard fix | Switching to a sequence-number cursor would also work, but `source_updated_at` is already mandated for stale-write protection and reusing it avoids a second source-of-truth |
| 14 | HMAC signing string is canonical 5-line layout (method + path + query + ts + body_sha256) | Body-only signatures are replayable across endpoints with the same body. Method+path+query binding makes each signed request specific. Defining canonical_query as sorted+percent-encoded avoids two valid encodings of the same query producing different signatures | The wire format is now fixed; downstream changes require coordinated svc+engine rollouts |
| 15 | `MARKETPLACE_SVC_URL` set without `MARKETPLACE_SVC_S2S_SECRET` fails boot | A task spawned without a secret would loop forever producing 401s and log noise. Better to fail loud at deploy time | The reverse (secret without URL) is intentionally allowed: a deploy can accept pushes without polling back |
| 16 | All public inputs (`asset_id`, `wallet_pubkey`, `owner_wallet`) base58-decode to exactly 32 bytes at the API boundary | Non-canonical strings (uppercase variants, padding differences) would create logical duplicates and false 403s. Validating at the boundary normalizes representation everywhere downstream | Switching to BYTEA storage in the future is a one-shot migration; the boundary validation is independent of column type |

## 8. Risks

| Risk | Mitigation |
|---|---|
| **svc push fails and self-heal disabled (env unset) → engine state permanently stale** | Boot-time validation: setting `MARKETPLACE_SVC_URL` without the secret fails boot; the OSS-only mode (neither set) accepts no marketplace coordination at all. No silent half-state. |
| **HMAC secret leaks** | Secret lives in fly secrets / Supabase env vault, never in code or CI logs. Rotation uses `MARKETPLACE_SVC_S2S_SECRET_PREVIOUS` for verification during transition; deploy new secret, wait one sync cycle, drop previous. |
| **Reordered webhooks revert ownership to a stale owner** | Every `/internal/*/upsert` carries `source_updated_at`; the SQL `ON CONFLICT … WHERE incoming > existing` clause silently drops older events. Self-heal pulls hit the same guard. |
| **Same-timestamp rows split across cursor pages → dropped from self-heal** | Compound cursor `(source_updated_at, pk)` with deterministic tie-break by primary key. Standard fix. |
| **HMAC body-only signature replayed across endpoints** | Signing string includes method + path + canonical_query, not just body. A signature for `POST /internal/ownership/upsert` cannot match `POST /internal/wallets/upsert` even if both carry the same JSON. |
| **HMAC middleware memory DoS via huge body** | 1 MiB hard cap on body read in middleware, returning 413 before HMAC computation. Cap is well above any legitimate payload (single UPSERT < 1 KiB). |
| **Engine receives ownership row for an `asset_id` whose `persona_genomes.asset_id` hasn't been set yet** | Gate join falls through to empty → 403 on chat-start. Acceptable v0.1 behavior; the future content-ingestion PR will sequence pushes so genome lands before ownership. Operator-facing reconciliation report flags drift. |
| **Previous owner keeps chatting through existing session after sale** | Per-message gate (§4.5) catches this on the next message. Latency cost is one indexed PK join, dominated by the LLM round-trip. |
| **Wallet rebinding race** (svc unlinks W from user_A, links W to user_B; engine sees them out of order) | Stale-write protection on `source_updated_at` plus tombstone rows mean the later event wins regardless of arrival order. The partial unique index on `(wallet_pubkey) WHERE linked = true` permits exactly one active binding at any time. |
| **Hidden `persona_instance` row created for non-owner** | Gate runs **before** `create_instance` in the genome path of `start_chat`. Non-owners get 403 with no DB side effect. |
| **Catalog leakage**: `/comp/personas` lists genomes whose `asset_id` the caller doesn't own | Recorded as a follow-up (§9). v0.1 lets non-owners *see* a genome's existence but not chat with it; the marketplace UI surfaces ownership state separately. Filtering at the catalog endpoint is a future privacy-hardening step. |
| **Webhook-style replay attacks against `/internal/*`** | Timestamp window (±5 min) + per-request HMAC binding to method/path/query/body. Cursor `since` endpoints are idempotent reads — replays are safe by construction; replay of writes hits the stale-write guard. |
| **Migration on a live engine** | Migrations are additive (new tables + nullable column). Zero existing row affected. Roll-forward only. |

## 9. Out-of-scope (recorded for next-round PRs)

- **Marketplace persona content ingestion** — `svc → /internal/genomes/upsert` plus KMS-unwrap-on-chat-load to materialize encrypted prompts. Separate PR, blocks the user-visible chat-with-NFT loop end-to-end.
- **Frontend wallet-linking flow** — `eros-engine-web` adds wallet adapter, `/me/wallets/challenge` + `/me/wallets/confirm` client-side flow.
- **Bulk ownership queries** — admin or catalog routes that list "all assets owned by user U" / "all owners of genome G." Not needed by chat-gating.
- **`eros-nft-extended` trained-persona transfer** — instance-level state transfer would add a `POST /internal/instance-transfer` surface and migration of affinity/memory rows. Entirely separate spec lineage.
- **Catalog filtering** — `/comp/personas` currently lists every active genome regardless of NFT ownership. A future PR can filter or annotate NFT-backed genomes by caller ownership state. Not a v0.1 requirement; the marketplace UI is the surface where ownership is rendered.
- **Observability instrumentation** — sync lag, rejected stale events (count + last reason), HMAC failures, cursor age per entity, gate deny reason counts. The implementation plan should add these as a thin metrics layer; the spec records the requirement here.
- **Dangling ownership row cleanup** — if a `persona_genomes` row is set inactive or its `asset_id` cleared, the corresponding `persona_ownership` row becomes orphaned (the gate cannot resolve it back to a genome). A reconciliation report is enough at v0.1; a future PR can add periodic cleanup or a `FOREIGN KEY` after the ingestion PR establishes genome→asset_id as the authoritative source.
- **Test plan for backward compatibility** — explicit Vitest/sqlx tests asserting `asset_id IS NULL` legacy seeds still pass `chat/start`, `message`, and `message_async` unchanged. Lands in the implementation plan, not the spec.

## 10. Files touched (when implemented)

```
A docs/superpowers/specs/2026-05-13-marketplace-ownership-gate-design.md  # this doc
A crates/eros-engine-store/migrations/0009_wallet_links.sql
A crates/eros-engine-store/migrations/0010_persona_ownership.sql
A crates/eros-engine-store/migrations/0011_persona_genome_asset_id.sql
A crates/eros-engine-store/src/ownership.rs
A crates/eros-engine-store/src/wallets.rs
A crates/eros-engine-store/src/sync_cursors.rs
A crates/eros-engine-store/src/pubkey.rs
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

Approximately 22 files: 12 new, 10 modified.

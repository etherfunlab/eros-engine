# eros-engine — Per-turn affinity event / delta endpoints (Spec)

**Status**: design, pending implementation plan
**Target release**: 0.2.x patch (additive, non-breaking)
**Audience**: anyone implementing the engine-side per-turn affinity observation surface for `eros-engine-web`

---

## 0. Background

`eros-engine-web` currently polls `GET /comp/affinity/{session_id}` after **every** turn to observe how the 6-axis affinity vector moved. That endpoint:

- lives on the **debug** router and is only registered when `EXPOSE_AFFINITY_DEBUG=true`;
- returns the **full current vector** (absolute values), not the per-turn change;
- forces the FE to diff consecutive full-vector reads to derive "what moved this turn" — which races against the async post-process that writes affinity, and can't see two events in one turn (e.g. gift + message).

The product direction has shifted (confirmed 2026-05-20):

1. The per-turn full-vector pull was a **debug** affordance. `EXPOSE_AFFINITY_DEBUG` will likely be **closed** in production later.
2. When the FE wants the **full affinity values for display**, it will read them itself via its own DB middleware — **no longer depending on the engine's `EXPOSE_AFFINITY_DEBUG` gate**.
3. The FE still needs an engine endpoint for **per-turn affinity delta observation** — specifically the **post-EMA effective change** of the latest turn. The FE cannot derive this from raw stored deltas (EMA inertia + clamping make it lossy to replay) nor reliably from diffing its own reads (event-boundary races).

This spec adds two endpoints and the storage needed to back them.

### Why the engine must own the post-EMA delta

`companion_affinity_events.deltas` stores only the **pre-EMA raw delta** the pipeline decided. The value the user actually sees the meters move by is the **post-EMA effective change** (`after − before`, after EMA blending in `Affinity::apply_deltas` **and** range clamping). That number is not recoverable after the fact from the raw delta alone, so the engine must **persist it at write time**. The FE diffing its own full-vector reads is unreliable because the affinity write is async post-process and a single turn can emit more than one event.

---

## 0.1 Relationship to the BFF layer convention

This spec adds endpoints under both the canonical `/comp/*` tree and the `/bff/v1/comp/*` tree introduced by `docs/superpowers/specs/2026-05-20-history-latency-cuts-design.md` §0.1. All convention rules there apply unchanged. In particular:

- The canonical `/comp/affinity/{sid}/event` is the engine-shaped, debug-gated, complete view.
- The BFF `/bff/v1/comp/affinity/{sid}/event` is the frontend-shaped, curated view; it may reshape/trim without versioning beyond `v1`.
- BFF reaches down to repos directly (never calls the canonical HTTP handler).
- The BFF handler gets a distinct Rust fn name so utoipa-axum emits a unique `operationId`; it is tagged `bff-companion`.

---

## 1. Data model change

### 1.1 Migration `0014_affinity_effective_deltas.sql`

Additive, low-risk on the shared prod Supabase Postgres (nullable column, no backfill; CHECK widened only — never narrowed).

```sql
-- SPDX-License-Identifier: AGPL-3.0-only

-- Post-EMA effective change (after − before) per event. NULL on rows
-- written before this migration; the FE observation surface only needs
-- live/recent turns, so historical NULLs are acceptable.
ALTER TABLE engine.companion_affinity_events
    ADD COLUMN effective_deltas JSONB;

-- Fix: post_process.rs already emits event_type = 'proactive' for
-- AI-initiated turns, but the original 0002 CHECK omitted it, so those
-- INSERTs silently failed (warn-logged, non-fatal) and never landed.
-- Widen the CHECK so proactive affinity events persist and show up in the
-- new delta feed. CHECK is only widened, never narrowed → safe.
--
-- Drop the existing event_type CHECK by *catalog lookup* rather than a
-- guessed name: a blind `DROP CONSTRAINT IF EXISTS <wrong-name>` would
-- silently leave the old constraint in place and keep rejecting 'proactive'.
DO $$
DECLARE
    cname text;
BEGIN
    SELECT con.conname INTO cname
    FROM pg_constraint con
    JOIN pg_class rel ON rel.oid = con.conrelid
    JOIN pg_namespace nsp ON nsp.oid = rel.relnamespace
    WHERE nsp.nspname = 'engine'
      AND rel.relname = 'companion_affinity_events'
      AND con.contype = 'c'
      AND pg_get_constraintdef(con.oid) ILIKE '%event_type%';
    IF cname IS NOT NULL THEN
        EXECUTE format(
            'ALTER TABLE engine.companion_affinity_events DROP CONSTRAINT %I',
            cname
        );
    END IF;
END $$;

ALTER TABLE engine.companion_affinity_events
    ADD CONSTRAINT companion_affinity_events_event_type_check
    CHECK (event_type IN ('message', 'ghost', 'gift', 'proactive', 'time_decay'));

-- Covering index for the new per-session event reads (join filters on
-- affinity_id, then orders by created_at DESC, id DESC). The existing
-- idx_affinity_events_affinity covers only affinity_id.
CREATE INDEX idx_affinity_events_affinity_created
    ON engine.companion_affinity_events (affinity_id, created_at DESC, id DESC);
```

> **Plan-stage note:** the `DO $$` block finds the event_type CHECK by definition (`pg_get_constraintdef ... ILIKE '%event_type%'`) so it works regardless of the auto-generated name. Verify in a scratch DB that exactly one matching constraint is found before relying on it. The migration runs as the schema owner (same role as prior additive migrations e.g. `0012`), so RLS (`0013`) does not block the DDL, and no new column grants are needed — browser roles already have zero access to `engine.*`.

### 1.2 Column semantics

| Column                          | Meaning                                                                 |
|---------------------------------|------------------------------------------------------------------------|
| `deltas` (existing)             | **Pre-EMA** raw delta the pipeline/LLM decided this event.              |
| `effective_deltas` (new)        | **Post-EMA** per-axis change actually applied = `after − before`, reflecting EMA inertia + clamping. |

Both are the same 6-axis shape (`warmth, trust, intrigue, intimacy, patience, tension`), each axis an `f64`.

- **message / gift / proactive** → `effective_deltas` carries real per-axis change.
- **ghost** → `effective_deltas` is all-zero (no axis moves; only `ghost_streak` / `total_ghosts` change).
- **time_decay** → `effective_deltas` carries the decay change (negative drift), if/when decay writes events.

> **Time-decay boundary (important):** `post_process` calls `affinity.apply_time_decay()` *before* `persist_with_event` ([post_process.rs](../../../crates/eros-engine-server/src/pipeline/post_process.rs) `apply_affinity`). The before-snapshot is taken inside `persist_with_event`, i.e. **after** that decay. So `effective_deltas` is the post-EMA *event* delta measured from the already-decayed baseline — it does **not** include the background time-decay drift applied earlier in the same turn. This matches the intent ("what this turn's event did to affinity"), but it is **not** the full DB-vector movement since the last persisted row. The FE must not treat `effective_deltas` as "absolute vector change since last poll."

---

## 2. Store layer (`crates/eros-engine-store/src/affinity.rs`)

### 2.1 Capture effective change in `persist_with_event`

`persist_with_event` mutates `affinity` in place via `apply_deltas`. Snapshot the 6 axes **before** the mutation, compute the effective change **after**, and write it to the new column in the same transaction.

```rust
pub async fn persist_with_event(
    &self,
    affinity: &mut Affinity,
    deltas: &AffinityDeltas,
    ema_inertia: f64,
    event_type: &str,
    context: serde_json::Value,
) -> Result<(), sqlx::Error> {
    // Snapshot pre-EMA axis values.
    let before = AffinityDeltas {
        warmth: affinity.warmth, trust: affinity.trust, intrigue: affinity.intrigue,
        intimacy: affinity.intimacy, patience: affinity.patience, tension: affinity.tension,
    };

    affinity.apply_deltas(deltas, ema_inertia);
    let label = affinity.infer_label();
    affinity.relationship_label = label;

    // Post-EMA effective change = after − before (captures EMA + clamping).
    let effective = AffinityDeltas {
        warmth: affinity.warmth - before.warmth,
        trust: affinity.trust - before.trust,
        intrigue: affinity.intrigue - before.intrigue,
        intimacy: affinity.intimacy - before.intimacy,
        patience: affinity.patience - before.patience,
        tension: affinity.tension - before.tension,
    };

    // ... existing UPDATE companion_affinity ...

    let deltas_json = serde_json::to_value(deltas).unwrap_or_default();
    let effective_json = serde_json::to_value(&effective).unwrap_or_default();
    sqlx::query(
        "INSERT INTO engine.companion_affinity_events \
           (affinity_id, event_type, deltas, effective_deltas, context) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(affinity.id).bind(event_type)
    .bind(deltas_json).bind(effective_json).bind(context)
    .execute(&mut *tx).await?;

    tx.commit().await?;
    Ok(())
}
```

`record_ghost` inserts a `ghost` event with `effective_deltas` set to an all-zero object (mirror the existing empty-`deltas` pattern, but `'{...zeros...}'::jsonb` or `serde_json::to_value(&AffinityDeltas::default())`).

> **Plan-stage note:** `AffinityDeltas::default()` is all-zeros (it derives `Default`). Use it for the ghost effective value rather than `'{}'` so the FE always gets a complete 6-axis object.

### 2.2 New read methods

```rust
/// One affinity event row joined to its session.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AffinityEventRow {
    /// Event UUID — the stable, unique freshness/dedup key for the FE
    /// (created_at alone is not unique under same-now() ties).
    pub id: Uuid,
    pub event_type: String,
    pub deltas: serde_json::Value,            // pre-EMA
    pub effective_deltas: Option<serde_json::Value>, // post-EMA (NULL for pre-migration rows)
    pub created_at: DateTime<Utc>,
}

impl<'a> AffinityRepo<'a> {
    /// Newest-first events for a session, optionally filtered by event_type.
    /// Joins companion_affinity_events → companion_affinity on affinity_id,
    /// filtered by session_id (uses idx_affinity_events_affinity_created).
    pub async fn list_events(
        &self, session_id: Uuid, limit: i64, offset: i64,
        event_type: Option<&str>,
    ) -> Result<Vec<AffinityEventRow>, sqlx::Error> { /* ... */ }

    /// Most-recent user-turn event (message/gift/proactive/ghost) for a
    /// session, or None if the session has no such event yet. Excludes
    /// time_decay (background drift). Ghost is included so the latest turn
    /// is never misreported as a stale prior turn — a ghost turn returns
    /// all-zero effective_deltas.
    pub async fn latest_turn_event(
        &self, session_id: Uuid,
    ) -> Result<Option<AffinityEventRow>, sqlx::Error> { /* ... */ }
}
```

`latest_turn_event` query:
```sql
SELECT e.id, e.event_type, e.deltas, e.effective_deltas, e.created_at
FROM engine.companion_affinity_events e
JOIN engine.companion_affinity a ON a.id = e.affinity_id
WHERE a.session_id = $1
  AND e.event_type IN ('message', 'gift', 'proactive', 'ghost')
ORDER BY e.created_at DESC, e.id DESC
LIMIT 1
```

`list_events` is the same join without the turn-class filter (or with an optional `event_type = $N` predicate), `ORDER BY e.created_at DESC, e.id DESC LIMIT $2 OFFSET $3`.

> **Deterministic ordering:** both queries break `created_at` ties with `e.id DESC`. The `id` is a random UUIDv4, so the tiebreak is *stable but arbitrary* (not chronological) — acceptable because turn-class events occur one-per-turn in distinct transactions and effectively never share a `created_at`. The tiebreak only guarantees a deterministic pick in the pathological tie case. The FE's freshness key is `event_id` (unique), not `created_at`.

> **Plan-stage note:** the join filters on `companion_affinity.session_id` (UNIQUE per session), so at most one affinity row participates; the result is just that session's events. No cross-session leakage. Adding `user_id` to the predicate is optional defense-in-depth — the handler's `require_session_for_user` ownership check already gates access.

---

## 3. Canonical endpoint — `GET /comp/affinity/{session_id}/event`

### 3.1 Placement & gating

Lives in `crates/eros-engine-server/src/routes/debug.rs`, registered **only** when `EXPOSE_AFFINITY_DEBUG=true` — same gate and module as the existing `get_affinity`. It is the complete, engine-shaped debug view.

### 3.2 Request

```
GET /comp/affinity/{session_id}/event?limit=20&offset=0&event_type=message
Auth: Bearer <Supabase JWT>
```

- `limit ∈ [1, 100]`, default 20 (clamped). `offset ≥ 0`, default 0.
- `event_type` optional filter ∈ `{message, ghost, gift, proactive, time_decay}`. Absent → all types. An **invalid** value returns `400 Bad Request` (not a silent empty result).
- JWT + ownership via `require_session_for_user` (404 missing / 403 not yours), reused as `pub(crate)` (already promoted by the history-latency-cuts work). Call it **first**, then list events, so an owned-but-empty session returns `200 events: []` rather than leaking existence via error codes.

### 3.3 Response DTO

```rust
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AffinityEventEntry {
    /// Stable unique id of this event (FE freshness/dedup key).
    pub event_id: Uuid,
    pub event_type: String,
    /// Pre-EMA raw delta the pipeline decided.
    pub deltas: AffinityDeltasDto,
    /// Post-EMA effective change (after − before). `None` for rows written
    /// before migration 0014.
    pub effective_deltas: Option<AffinityDeltasDto>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AffinityEventsResponse {
    pub session_id: Uuid,
    pub events: Vec<AffinityEventEntry>,
    /// Count returned in this page (== events.len()), NOT the grand total.
    pub total: usize,
}
```

Reuse the existing `AffinityDeltasDto` (6 axes) from `routes/companion.rs` (or move it to `routes/dto.rs` if a cleaner home is wanted — plan stage decides; reuse is fine).

### 3.4 Responses

`200` body above · `401` missing/invalid bearer · `403` not your session · `404` session not found. Newest-first. Empty session → `200` with `events: []`.

---

## 4. BFF endpoint — `GET /bff/v1/comp/affinity/{session_id}/event`

### 4.1 Placement & gating

New file `crates/eros-engine-server/src/routes/bff/affinity.rs`, merged into `bff::router()`, tag `bff-companion`. **Not** behind `EXPOSE_AFFINITY_DEBUG` — the FE owns this surface. Still JWT + ownership checked (auth ≠ debug gate).

### 4.2 Request

```
GET /bff/v1/comp/affinity/{session_id}/event
Auth: Bearer <Supabase JWT>
```

No query params. The endpoint exists to answer one question: "what did the latest turn do to affinity?"

### 4.3 Response DTO

```rust
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffAffinityDelta {
    /// Stable unique id of this event — the FE's freshness/dedup key.
    pub event_id: Uuid,
    pub event_type: String,        // "message" | "gift" | "proactive" | "ghost"
    /// Post-EMA effective change of the latest user-turn event. All-zero for
    /// a ghost turn (the AI didn't reply; no axis moved).
    pub effective_deltas: AffinityDeltasDto,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffAffinityDeltaResponse {
    pub session_id: Uuid,
    /// `None` when the session has no user-turn affinity event yet
    /// (brand-new session, or only time_decay so far), OR when the latest
    /// user-turn event predates migration 0014 (no effective_deltas).
    pub event: Option<BffAffinityDelta>,
}
```

- **Latest only**, **post-EMA only** (`effective_deltas`); pre-EMA raw delta is intentionally omitted (that's the canonical/debug surface).
- **Turn-class filter**: latest event WHERE `event_type IN ('message','gift','proactive','ghost')` — excludes only `time_decay` (background drift). **`ghost` is included** so the latest user turn is never misreported as a stale prior turn: a ghost turn returns all-zero `effective_deltas` and a fresh `created_at`/`event_id`, so FE polling advances correctly.
- If `latest_turn_event` returns a row whose `effective_deltas` is `NULL` (pre-0014), return `event: null` (we don't fabricate a post-EMA value). Note this differs from a ghost turn, which returns a present event with all-zero `effective_deltas`.

### 4.4 Responses

`200` body above (with `event` possibly null) · `401` · `403` · `404` session not found. Brand-new / no-turn-event session is **not** an error → `200` with `event: null`.

### 4.5 FE consumption (informative, not implemented here)

After a turn completes (sync reply or SSE stream end), the FE polls this endpoint. Because the affinity write is async post-process, the FE polls briefly until **`event_id` differs from the last seen value** (a new turn-class event landed). Use `event_id` — not `created_at` — as the freshness key, since `created_at` is not unique under same-`now()` ties. A ghost turn produces a new `event_id` with all-zero deltas, so polling terminates correctly even when the AI ghosts. (SSE-inline delta is explicitly out of scope — see §6.)

---

## 5. Testing

### 5.1 Store (`affinity.rs`)
- `persist_with_event` writes `effective_deltas` = post-EMA change; with `ema_inertia > 0`, effective ≠ raw `deltas` (the load-bearing assertion that proves we're storing the smoothed change, not the raw one).
- With clamping at a boundary (axis already near 1.0, large positive delta) → effective is the clamped change, smaller than raw.
- `record_ghost` writes an all-zero `effective_deltas` object (`AffinityDeltas::default()`, not `{}`).
- `list_events`: newest-first (`created_at DESC, id DESC`), limit/offset paging, `event_type` filter.
- `latest_turn_event`: returns the most-recent message/gift/proactive/**ghost**; **skips** an intervening `time_decay`; returns `None` for a session with no user-turn events. Key case: a `ghost` written *after* a `message` is returned (with zero `effective_deltas`) — NOT the older message.
- Determinism: seed events with explicit, strictly-increasing `created_at` (a single multi-row INSERT shares one `now()` → ties → undefined order; see the history-latency-cuts post-mortem). Also assert the `id` tiebreak makes a same-`created_at` pair deterministic.

### 5.2 Canonical endpoint
- gated: present when `EXPOSE_AFFINITY_DEBUG=true`, **absent (404)** when false.
- multi-row newest-first; `limit`/`offset`; `event_type` filter narrows.
- invalid `event_type` value → `400`.
- both `deltas` and `effective_deltas` present on post-0014 rows; `event_id` present.
- `401` / `403` / `404`; owned-but-empty session → `200 events:[]`.

### 5.3 BFF endpoint
- returns latest user-turn event, `effective_deltas` only, no raw `deltas` field; `event_id` present.
- **NOT** gated by `EXPOSE_AFFINITY_DEBUG` (present even when the flag is false).
- includes `ghost` (zero-delta) and skips only `time_decay` when finding the latest user-turn event.
- ghost-after-message case → returns the ghost event with all-zero `effective_deltas` and the ghost's `event_id`/`created_at` (NOT the older message).
- `event: null` on brand-new session and on a session whose only events are `time_decay`.
- `401` / `403` / `404`.

### 5.4 OpenAPI snapshot regenerated (`cargo run -p eros-engine-server -- print-openapi > crates/eros-engine-server/openapi.json`), idempotent, CI drift-check green. `bff-companion` tag already exists.

---

## 6. Out of scope (intentionally deferred)

- **SSE-inline affinity delta** (folding the delta into the stream's final frame). The FE polls `/bff/v1/comp/affinity/{sid}/event` after a turn instead. Revisit only if the extra round-trip is measured as a problem.
- **FE direct-DB read of full absolute affinity values** for display — the FE does this itself via its own middleware, independent of the engine's `EXPOSE_AFFINITY_DEBUG` gate.
- **Retiring `EXPOSE_AFFINITY_DEBUG`** entirely / removing the `get_affinity` full-vector debug endpoint. Separate decision once the FE migration lands.
- **Backfilling `effective_deltas`** for historical event rows. Live observation only needs new turns; pre-0014 rows stay `NULL`.
- **Cursor pagination** for the canonical events endpoint (offset is fine at current scale).

---

## 7. Implementation checklist (for plan stage)

```
Store + migration
  S1  migrations/0014_affinity_effective_deltas.sql:
        + ADD COLUMN effective_deltas JSONB (nullable)
        + widen event_type CHECK to include 'proactive' (catalog-lookup DO
          block to drop the existing CHECK by definition, not guessed name)
        + CREATE INDEX idx_affinity_events_affinity_created
          (affinity_id, created_at DESC, id DESC)
  S2  affinity.rs: persist_with_event captures before-snapshot, computes
        effective = after − before, writes effective_deltas in the same tx
  S3  affinity.rs: record_ghost writes all-zero effective_deltas (Default)
  S4  affinity.rs: + AffinityEventRow (incl id), list_events(),
        latest_turn_event() — both ORDER BY created_at DESC, id DESC
  S5  cargo test -p eros-engine-store (effective vs raw under EMA, clamping,
        ghost zeros, list/latest ordering + id tiebreak, ghost-after-message
        returns ghost not stale message, time_decay skipped)

Canonical endpoint (gated)
  C1  routes/debug.rs: + AffinityEventEntry (incl event_id), AffinityEventsResponse
  C2  routes/debug.rs: + handler get_affinity_events
        (GET /comp/affinity/{session_id}/event), registered in the
        enabled branch of debug::router() alongside get_affinity;
        require_session_for_user first; invalid event_type → 400
  C3  cargo test -p eros-engine-server (gated on/off, paging, filter,
        invalid-event_type 400, owned-empty 200 [], 401/403/404)

BFF endpoint (ungated by debug flag)
  B1  routes/bff/affinity.rs (new): + BffAffinityDelta (incl event_id),
        BffAffinityDeltaResponse + handler bff_get_affinity_delta
        (GET /bff/v1/comp/affinity/{session_id}/event)
  B2  routes/bff/mod.rs: merge affinity::router() into bff::router()
  B3  cargo test -p eros-engine-server (latest user-turn incl ghost, post-EMA
        only, ghost-after-message returns zero-delta ghost, null on empty,
        NOT gated by EXPOSE_AFFINITY_DEBUG, 401/403/404)

OpenAPI
  O1  regenerate crates/eros-engine-server/openapi.json; CI drift-check green
```

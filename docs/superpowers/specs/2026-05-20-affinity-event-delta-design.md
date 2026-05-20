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
ALTER TABLE engine.companion_affinity_events
    DROP CONSTRAINT companion_affinity_events_event_type_check;
ALTER TABLE engine.companion_affinity_events
    ADD CONSTRAINT companion_affinity_events_event_type_check
    CHECK (event_type IN ('message', 'ghost', 'gift', 'proactive', 'time_decay'));
```

> **Plan-stage note:** confirm the exact auto-generated constraint name (`companion_affinity_events_event_type_check` is Postgres's default for a table-level CHECK on `0002`). If it differs, use the real name. A guarded `DO $$ ... $$` block that looks the constraint up by table is acceptable if the name is uncertain.

### 1.2 Column semantics

| Column                          | Meaning                                                                 |
|---------------------------------|------------------------------------------------------------------------|
| `deltas` (existing)             | **Pre-EMA** raw delta the pipeline/LLM decided this event.              |
| `effective_deltas` (new)        | **Post-EMA** per-axis change actually applied = `after − before`, reflecting EMA inertia + clamping. |

Both are the same 6-axis shape (`warmth, trust, intrigue, intimacy, patience, tension`), each axis an `f64`.

- **message / gift / proactive** → `effective_deltas` carries real per-axis change.
- **ghost** → `effective_deltas` is all-zero (no axis moves; only `ghost_streak` / `total_ghosts` change).
- **time_decay** → `effective_deltas` carries the decay change (negative drift), if/when decay writes events.

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
    pub event_type: String,
    pub deltas: serde_json::Value,            // pre-EMA
    pub effective_deltas: Option<serde_json::Value>, // post-EMA (NULL for pre-migration rows)
    pub created_at: DateTime<Utc>,
}

impl<'a> AffinityRepo<'a> {
    /// Newest-first events for a session, optionally filtered by event_type.
    /// Joins companion_affinity_events → companion_affinity on affinity_id,
    /// filtered by session_id (uses idx_affinity_events_affinity).
    pub async fn list_events(
        &self, session_id: Uuid, limit: i64, offset: i64,
        event_type: Option<&str>,
    ) -> Result<Vec<AffinityEventRow>, sqlx::Error> { /* ... */ }

    /// Most-recent "turn-class" event (message/gift/proactive) for a session,
    /// or None if the session has no such event yet.
    pub async fn latest_turn_event(
        &self, session_id: Uuid,
    ) -> Result<Option<AffinityEventRow>, sqlx::Error> { /* ... */ }
}
```

`latest_turn_event` query:
```sql
SELECT e.event_type, e.deltas, e.effective_deltas, e.created_at
FROM engine.companion_affinity_events e
JOIN engine.companion_affinity a ON a.id = e.affinity_id
WHERE a.session_id = $1
  AND e.event_type IN ('message', 'gift', 'proactive')
ORDER BY e.created_at DESC
LIMIT 1
```

`list_events` is the same join without the turn-class filter (or with an optional `event_type = $N` predicate), `ORDER BY e.created_at DESC LIMIT $2 OFFSET $3`.

> **Plan-stage note:** the join filters on `companion_affinity.session_id` (UNIQUE per session), so at most one affinity row participates; the result is just that session's events. No cross-session leakage.

---

## 3. Canonical endpoint — `GET /comp/affinity/{session_id}/event`

### 3.1 Placement & gating

Lives in `crates/eros-engine-server/src/routes/debug.rs`, registered **only** when `EXPOSE_AFFINITY_DEBUG=true` — same gate and module as the existing `get_affinity`. It is the complete, engine-shaped debug view.

### 3.2 Request

```
GET /comp/affinity/{session_id}/event?limit=20&offset=0&event_type=message
Auth: Bearer <Supabase JWT>
```

- `limit ∈ [1, 100]`, default 20. `offset ≥ 0`, default 0.
- `event_type` optional filter ∈ `{message, ghost, gift, proactive, time_decay}`. Absent → all types.
- JWT + ownership via `require_session_for_user` (404 missing / 403 not yours), reused as `pub(crate)` (already promoted by the history-latency-cuts work).

### 3.3 Response DTO

```rust
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AffinityEventEntry {
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
    pub event_type: String,        // "message" | "gift" | "proactive"
    /// Post-EMA effective change of the latest turn-class event.
    pub effective_deltas: AffinityDeltasDto,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffAffinityDeltaResponse {
    pub session_id: Uuid,
    /// `None` when the session has no turn-class affinity event yet
    /// (brand-new session, or only ghost/time_decay so far), OR when the
    /// latest turn-class event predates migration 0014 (no effective_deltas).
    pub event: Option<BffAffinityDelta>,
}
```

- **Latest only**, **post-EMA only** (`effective_deltas`); pre-EMA raw delta is intentionally omitted (that's the canonical/debug surface).
- **Turn-class filter**: latest event WHERE `event_type IN ('message','gift','proactive')` — excludes `time_decay` (background drift) and `ghost` (zero-delta), so the FE always sees a meaningful per-turn movement.
- If `latest_turn_event` returns a row whose `effective_deltas` is `NULL` (pre-0014), return `event: null` (we don't fabricate a post-EMA value).

### 4.4 Responses

`200` body above (with `event` possibly null) · `401` · `403` · `404` session not found. Brand-new / no-turn-event session is **not** an error → `200` with `event: null`.

### 4.5 FE consumption (informative, not implemented here)

After a turn completes (sync reply or SSE stream end), the FE polls this endpoint. Because the affinity write is async post-process, the FE may need to poll briefly until `created_at` advances past the last seen value. The `created_at` field is the dedup/freshness key. (SSE-inline delta is explicitly out of scope — see §6.)

---

## 5. Testing

### 5.1 Store (`affinity.rs`)
- `persist_with_event` writes `effective_deltas` = post-EMA change; with `ema_inertia > 0`, effective ≠ raw `deltas` (the load-bearing assertion that proves we're storing the smoothed change, not the raw one).
- With clamping at a boundary (axis already near 1.0, large positive delta) → effective is the clamped change, smaller than raw.
- `record_ghost` writes an all-zero `effective_deltas` object (not `{}`).
- `list_events`: newest-first, limit/offset paging, `event_type` filter.
- `latest_turn_event`: returns the most-recent message/gift/proactive; **skips** an intervening `ghost` / `time_decay`; returns `None` for a session with no turn-class events.
- Determinism: seed events with explicit, strictly-increasing `created_at` (a single multi-row INSERT shares one `now()` → ties → undefined order; see the history-latency-cuts post-mortem).

### 5.2 Canonical endpoint
- gated: present when `EXPOSE_AFFINITY_DEBUG=true`, **absent (404)** when false.
- multi-row newest-first; `limit`/`offset`; `event_type` filter narrows.
- both `deltas` and `effective_deltas` present on post-0014 rows.
- `401` / `403` / `404`; empty session → `200 events:[]`.

### 5.3 BFF endpoint
- returns latest turn-class event, `effective_deltas` only, no raw `deltas` field.
- **NOT** gated by `EXPOSE_AFFINITY_DEBUG` (present even when the flag is false).
- skips ghost/time_decay to find the latest turn-class event.
- `event: null` on brand-new session and on a session whose only events are ghost/time_decay.
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
        + widen event_type CHECK to include 'proactive'
  S2  affinity.rs: persist_with_event captures before-snapshot, computes
        effective = after − before, writes effective_deltas in the same tx
  S3  affinity.rs: record_ghost writes all-zero effective_deltas (Default)
  S4  affinity.rs: + AffinityEventRow, list_events(), latest_turn_event()
  S5  cargo test -p eros-engine-store (effective vs raw under EMA, clamping,
        ghost zeros, list/latest ordering + turn-class skip)

Canonical endpoint (gated)
  C1  routes/debug.rs: + AffinityEventEntry, AffinityEventsResponse
  C2  routes/debug.rs: + handler get_affinity_events
        (GET /comp/affinity/{session_id}/event), registered in the
        enabled branch of debug::router() alongside get_affinity
  C3  cargo test -p eros-engine-server (gated on/off, paging, filter, 401/403/404)

BFF endpoint (ungated by debug flag)
  B1  routes/bff/affinity.rs (new): + BffAffinityDelta, BffAffinityDeltaResponse
        + handler bff_get_affinity_delta
        (GET /bff/v1/comp/affinity/{session_id}/event)
  B2  routes/bff/mod.rs: merge affinity::router() into bff::router()
  B3  cargo test -p eros-engine-server (latest turn-class only, post-EMA only,
        null on empty, NOT gated by EXPOSE_AFFINITY_DEBUG, 401/403/404)

OpenAPI
  O1  regenerate crates/eros-engine-server/openapi.json; CI drift-check green
```

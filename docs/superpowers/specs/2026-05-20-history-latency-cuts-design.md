# eros-engine — Chat history fetch latency cuts (Spec)

**Status**: design, pending implementation plan
**Target release**: 0.2.x patch (Plans A + B), 0.2.x or later (Plan C — additive, non-breaking)
**Audience**: anyone implementing the engine-side cold-mount speedups for `/app/chat/<companion>` on `eros-engine-web`

---

## 0. Background

Cold-mount of `/app/chat/<companion_id>` on `eros-engine-web` makes **three sequential round-trips** before the user sees their chat history:

```
onMounted →
  ensureSession()    → POST /comp/chat/start                ⏱ RT 1
  getHistory()       → GET  /comp/chat/<sid>/history?limit=50  ⏱ RT 2
  hydrateAffinity()  → GET  /comp/affinity/<sid>             ⏱ RT 3
```

Each round-trip carries TLS + JWT verification + Fly NRT proxy + Postgres query. Aggregate network time alone is **200–900 ms** depending on user location; on a cold machine, a Rust binary cold-start (~500 ms–3 s) stacks on top of that.

The handler is already async (`async fn get_history`, Tokio, sqlx). User-perceived latency comes from **round-trip count** and **machine cold-start**, not from handler concurrency.

This spec ships three orthogonal mitigations:

| Plan | Lever | Code surface | Risk | Order |
|------|-------|--------------|------|-------|
| A | `min_machines_running = 1` (no scale-to-zero in NRT) | `fly.toml` +1 line | nil | ship first |
| B | New BFF route `/bff/v1/comp/chat/<sid>/history` returning slim FE DTO | new module `routes/bff/` | nil to existing endpoints | ship second |
| C | New BFF route `/bff/v1/comp/chat/start` returning session + history + affinity in one shot | extends `routes/bff/` | nil to existing endpoints | ship third |

**Out of scope:** the web-side migration that switches `loadHistory` from 3 calls to 1. That waits on (1) Plan C engine release, (2) the in-flight SSE web work merging. Web spec will land separately in `eros-engine-web/docs/superpowers/specs/`.

---

## 0.1 The BFF layer convention (introduced by this spec)

Plans B and C introduce a new top-level routing tree:

```
/comp/*            — canonical engine API (engine-shaped DTOs, OSS-stable contract)
/bff/v1/comp/*     — frontend-shaped mirror of /comp/*    ← NEW
/bff/v1/user/*     — frontend-shaped mirror of /user/* (future)
/bff/v1/<area>/*   — pattern continues for other areas
```

### Convention rules

1. **`/bff/v1/<area>/*` mirrors the path layout of `/<area>/*`.** Same `session_id` / `instance_id` semantics in the path. The transformation lives in request/response *shape*, not URL structure.
2. **BFF routes serve `eros-engine-web` (and any future first-party web/mobile client).** They are NOT a stable third-party API surface. **Additive** changes (new optional fields, new endpoints, looser validation) ship in-place within `v1`. **Breaking** changes (removed/renamed fields, tightened validation, changed required body shape) go to `/bff/v2/...` — see rule 5.
3. **Engine-canonical routes (`/comp/*` etc.) are NEVER modified to satisfy frontend ergonomics.** If the FE needs a different shape, add a BFF route. This keeps the OSS engine contract stable for downstream consumers.
4. **BFF endpoints reuse the same auth (Supabase JWT) and middleware** as their engine counterparts — same `require_auth` layer attached to the merged sub-router. They also inherit the engine's `AppError` JSON taxonomy (`{ "error": code, "message": ... }`) and the bare-`401` auth-middleware response — no BFF-specific error envelope.
5. **`v1` is reserved for additive evolution; breaking changes mean `v2`.** A `/bff/v2/...` endpoint may co-exist with its `v1` predecessor for one minor release before the `v1` form is deleted.
6. **The `v` version in the URL is BFF-layer-wide, not per-endpoint.** All `/bff/v1/*` endpoints share the same versioning lifecycle. Avoid stamping individual endpoints with different versions.
7. **OSS scope: BFF for `/comp/*` lives in `eros-engine` (AGPL-3.0).** The transformation is a thin shape adapter on top of an already-OSS surface — no commercial IP. Future BFF for closed-source areas (`/match/*`, etc.) will live in the future closed `eros-gateway`, mirroring its own non-OSS counterparts. BFF routes in **this** OSS repo MUST NOT import from, or assume the existence of, `eros-gateway`.
8. **BFF routes never call other BFF routes or canonical HTTP handlers.** They reach down to repos (`ChatRepo`, `AffinityRepo`, …) and shared `pub(crate)` helpers (`resolve_or_create_session`, …) directly. This keeps auth, error mapping, and OpenAPI explicit — no in-process HTTP recursion, no double-wrapping of `AppError`.
9. **Each BFF handler gets a distinct Rust function name** (`bff_get_history`, `bff_start_chat`, …) so utoipa-axum emits a unique OpenAPI `operationId` that doesn't collide with the canonical handler of the same shape. All BFF handlers tag themselves `bff-companion` (or future `bff-<area>`), declared in `openapi.rs`.

### Why a BFF layer?

The web client's needs diverge from the engine contract in two directions:

- **Trim:** `extracted_facts` JSONB is dreaming-lite internal state; engine returns it for completeness but the FE never reads it.
- **Aggregate:** `loadHistory` cold-mount needs *session + history + affinity* in one call. Engine has no business creating a "cold-mount bundle" endpoint — that's a UI concern.

Adding query flags (`include_history`, `include_affinity`) to `/comp/chat/start` to satisfy a UI shape would couple the engine API to one client's flow. A BFF layer keeps the engine clean and gives the FE freedom to evolve.

### Module layout

```
crates/eros-engine-server/src/routes/
├── mod.rs                ← merge bff::router() into the auth-protected subtree
├── companion.rs          (existing /comp/* — UNTOUCHED by this spec)
├── companion_stream.rs   (existing /comp/.../stream — UNTOUCHED)
├── debug.rs
├── health.rs
├── s2s.rs
└── bff/                  ← NEW
    ├── mod.rs            ← assembles bff::router()
    └── companion.rs      ← /bff/v1/comp/* handlers (Plans B + C)
```

Each BFF handler's `#[utoipa::path]` uses the full path including the `/bff/v1/` prefix (the project merges routers rather than nests them; see `routes/mod.rs` doc-comment).

---

## 1. Plan A — Pin one warm machine in NRT

### 1.1 Change

`fly.toml`:

```diff
   [http_service]
     internal_port = 8080
     force_https = true
     auto_stop_machines = "stop"
     auto_start_machines = true
-    min_machines_running = 0
+    # Keep one warm machine in NRT so the next-after-idle visitor doesn't pay
+    # the ~500ms–3s Rust binary cold-start tax. Costs ~$5/mo for one
+    # shared-cpu-1x in NRT. `auto_stop_machines = "stop"` stays, so excess
+    # capacity above 1 still scales down to one (not zero).
+    min_machines_running = 1
     processes = ["app"]
```

That's the entire change.

### 1.2 Note on deploy

The repo `fly.toml` is the live config for whoever runs this image: this PR's one-line change ships directly via `flyctl deploy <your-app>`. The file's "example config — adapt to your own deployment" header still applies for downstream forks; operators of any first-party deploy keep their app name / region in their own private values file or `flyctl deploy` args, not in this OSS file.

### 1.3 Verification

After `flyctl deploy`:

```
flyctl status -a <your-app>
# expected: ≥ 1 machine in `started` state when traffic is zero
```

### 1.4 Cost

One `shared-cpu-1x` (currently `512 MB` per `fly.toml`) running 24/7 in NRT. At current Fly pricing this is a small recurring compute charge (single-digit USD/mo before bandwidth and org allowances); call it "a coffee a month" rather than pin a precise number that will rot.

### 1.5 Rollback

Revert the one-line change. Zero data risk.

---

## 2. Plan B — `GET /bff/v1/comp/chat/<sid>/history`

### 2.1 Problem

`/comp/chat/{sid}/history` returns `ChatHistoryEntry { role, content, sent_at, extracted_facts }`. The web client reads `r.role` and `r.content` — that's it. `extracted_facts` (JSONB written by dreaming-lite memory extraction) is shipped for every user row and consumed by nobody on the FE.

### 2.2 New endpoint

```
GET /bff/v1/comp/chat/{session_id}/history?limit=50&offset=0
Auth: Bearer <Supabase JWT>
```

Same auth, same `(session_id, user_id)` ownership check, same `limit ∈ [1, 50]`, same `offset ≥ 0` as the engine endpoint. **Intentional divergence: BFF defaults `limit=50` (the cap), engine defaults `limit=20`.** Reason: BFF exists for cold-mount, where the FE wants a full backscroll in one round-trip; the canonical endpoint stays conservative for OSS consumers paging through history. Plan-stage tests pin both defaults so the divergence is explicit.

### 2.3 Response DTO

```rust
// crates/eros-engine-server/src/routes/bff/companion.rs

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffHistoryEntry {
    pub role: String,        // "user" | "assistant" | "gift_user" | "system_error"
    pub content: String,
    pub sent_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffHistoryResponse {
    pub session_id: Uuid,
    pub messages: Vec<BffHistoryEntry>,
    /// Count of `messages` in this response (== `messages.len()`). NOT the
    /// total row count for the session — pagination doesn't know how many
    /// rows remain. Mirrors the existing `/comp/.../history` `total` field
    /// so a FE that already keys off it doesn't have to relearn semantics.
    pub total: usize,
}
```

Note the omissions: no `extracted_facts`, no SSE-metadata columns. Pure UI-rendering payload.

### 2.4 Store-layer change

`crates/eros-engine-store/src/chat.rs`:

```rust
#[derive(sqlx::FromRow)]
pub struct ChatMessageSlim {
    pub role: String,
    pub content: String,
    pub sent_at: DateTime<Utc>,
}

impl<'a> ChatRepo<'a> {
    // existing history() kept untouched — pipeline callers still need the full row.

    /// Projection-narrowed read used by BFF (and any caller that doesn't
    /// need extracted_facts / SSE metadata).
    pub async fn history_slim(
        &self,
        session_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ChatMessageSlim>, sqlx::Error> {
        let mut rows = sqlx::query_as::<_, ChatMessageSlim>(
            "SELECT role, content, sent_at FROM engine.chat_messages \
             WHERE session_id = $1 \
             ORDER BY sent_at DESC \
             LIMIT $2 OFFSET $3",
        )
        .bind(session_id).bind(limit).bind(offset)
        .fetch_all(self.pool).await?;
        rows.reverse();
        Ok(rows)
    }
}
```

Same `idx_chat_messages_session (session_id, sent_at DESC)` index, same DESC+reverse trick. Pure projection narrowing — no behaviour change, no migration.

### 2.5 Handler sketch

```rust
// routes/bff/companion.rs

#[utoipa::path(
    get,
    path = "/bff/v1/comp/chat/{session_id}/history",
    tag = "bff-companion",
    params(
        ("session_id" = Uuid, Path),
        ("limit" = Option<i64>, Query, description = "Max rows (default 50, capped at 50)"),
        ("offset" = Option<i64>, Query, description = "Page offset, default 0")
    ),
    responses(
        (status = 200, body = BffHistoryResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session not found")
    ),
    security(("bearer" = []))
)]
async fn bff_get_history(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<BffHistoryResponse>, AppError> {
    require_session_for_user(&state, session_id, user_id).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 50);
    let offset = query.offset.unwrap_or(0).max(0);

    let rows = ChatRepo { pool: &state.pool }
        .history_slim(session_id, limit, offset).await?;
    let messages: Vec<BffHistoryEntry> = rows.into_iter().map(|r| BffHistoryEntry {
        role: r.role, content: r.content, sent_at: r.sent_at,
    }).collect();
    let total = messages.len();

    Ok(Json(BffHistoryResponse { session_id, messages, total }))
}
```

`HistoryQuery` and `require_session_for_user` are reused from the existing module (or made `pub(crate)` if not already).

### 2.6 Backwards compatibility

`/comp/chat/{sid}/history` is **completely untouched**. Existing OSS consumers that read `extracted_facts` continue to work. The BFF endpoint is a new addition with its own schema.

### 2.7 Estimated savings

For a session with ~25 user rows × ~500 B `extracted_facts` ≈ 12 KB of JSONB shipped wholesale today. After Plan B that's 0 KB on the BFF path. Wire saving is single-digit KB gzipped; CPU saving is negligible. The real win is keeping the response body small enough to fit one TCP segment after the response starts — observable as <50 ms time-to-last-byte on a warm machine.

### 2.8 Rollback

Drop the new `routes/bff/` module and the `routes::router` merge line. Zero impact on existing routes or data.

---

## 3. Plan C — `POST /bff/v1/comp/chat/start`

> **Amendment (2026-05-20, v0.2.1):** the `affinity` field described below was
> **removed** from `BffStartResponse` shortly after it shipped, before any FE
> consumer depended on it. The FE bootstrap reads affinity separately (full
> values via its own DB middleware; per-turn deltas via
> `GET /bff/v1/comp/affinity/{sid}/event`, see
> `2026-05-20-affinity-event-delta-design.md`), so bundling it here coupled
> bootstrap to `EXPOSE_AFFINITY_DEBUG` for no benefit. `bff_start_chat` now
> returns session + slim history only; the §3.3/§3.4 affinity bits below are
> historical. Pre-consumption removal, so no `v2` bump.

### 3.1 Problem & lever

Cold-mount on `eros-engine-web` serialises three engine calls. Folding session creation + history + affinity into one BFF endpoint collapses **3 RT → 1 RT**.

### 3.2 New endpoint

```
POST /bff/v1/comp/chat/start
Auth: Bearer <Supabase JWT>
Body:
{
  "genome_id":   "<uuid>"  | null,    // required if no instance_id
  "instance_id": "<uuid>"  | null,    // optional explicit pick
  "is_demo":     false                 // optional
}
```

The request body **extends** today's `POST /comp/chat/start` body (`instance_id`, `genome_id`, `is_demo`) with one optional BFF-only field, `history_limit`. The canonical endpoint never sees `history_limit`; it is consumed and dropped at the BFF boundary. The response is the bundled FE shape.

### 3.3 Request / Response DTOs

```rust
// routes/bff/companion.rs

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BffStartRequest {
    pub instance_id: Option<Uuid>,
    pub genome_id: Option<Uuid>,
    #[serde(default)]
    pub is_demo: Option<bool>,
    // History page size for the bundled history. Default 50, capped at 50.
    pub history_limit: Option<i64>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffStartResponse {
    pub session_id: Uuid,
    pub instance_id: Uuid,
    pub persona_name: String,
    pub is_new: bool,
    /// Most-recent N messages, oldest-first. Empty for brand-new sessions.
    pub history: Vec<BffHistoryEntry>,
    /// Affinity snapshot. `None` for brand-new sessions (no affinity row yet)
    /// or when `EXPOSE_AFFINITY_DEBUG=false` (see §3.5).
    pub affinity: Option<AffinitySnapshot>,
}
```

No `include_history` / `include_affinity` flags — the BFF endpoint **always bundles**, because that is its entire reason to exist. Simpler API, no caller-side decision matrix.

### 3.4 Handler logic

```rust
async fn bff_start_chat(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<BffStartRequest>,
) -> Result<Json<BffStartResponse>, AppError> {
    // 1. Reuse the existing engine logic to resolve session_id + instance_id.
    //    Either call the canonical start_chat handler internally (after
    //    refactoring it into a pure function), or inline the resolution.
    //    Plan stage decides; "extract a pure fn" is the cleaner option.
    let resolved = resolve_or_create_session(&state, user_id, &req).await?;
    let history_limit = req.history_limit.unwrap_or(50).clamp(1, 50);

    // 2. Fire history + affinity in parallel on the same sqlx pool. Both
    //    arms return Result<_, AppError> directly so try_join! has nothing
    //    to coerce.
    let history_fut = async {
        Ok::<_, AppError>(
            ChatRepo { pool: &state.pool }
                .history_slim(resolved.session_id, history_limit, 0)
                .await?
        )
    };
    let affinity_fut = async {
        if !state.config.expose_affinity_debug {
            return Ok::<_, AppError>(None);
        }
        Ok(AffinityRepo { pool: &state.pool }
            .load(resolved.session_id)
            .await?
            .map(|mut a| { a.apply_time_decay(); AffinitySnapshot::from(a) }))
    };
    let (rows, affinity) = tokio::try_join!(history_fut, affinity_fut)?;

    let history = rows.into_iter().map(|r| BffHistoryEntry {
        role: r.role, content: r.content, sent_at: r.sent_at,
    }).collect();

    Ok(Json(BffStartResponse {
        session_id: resolved.session_id,
        instance_id: resolved.instance_id,
        persona_name: resolved.persona_name,
        is_new: resolved.is_new,
        history,
        affinity,
    }))
}
```

Key points:
- `tokio::try_join!` runs both reads concurrently on the same shared sqlx pool. No new pool is opened, but the two arms each check out a pooled connection for the duration of their query — peak handler concurrency for one BFF call is two connections, not one. Bench cost is negligible since both queries are sub-ms indexed lookups.
- Brand-new session ⇒ `history: []` (definitionally empty).
- `affinity: None` covers **three** cases that the FE should treat as "no calibration to show":
  1. `EXPOSE_AFFINITY_DEBUG=false` — gate closed (§3.5).
  2. Brand-new session — `create_session_with_metadata` doesn't create an affinity row; `AffinityRepo::load_or_create` only runs inside message/gift flows.
  3. Resumed session that has never had an affinity-producing event yet — same reason: no row exists.
- The session-resolution step (instance lookup, NFT gate, session create/resume) is the **same logic** as `/comp/chat/start`. To avoid copy-paste, extract it into a pure helper that both handlers call. See §3.6. The handler's documented response set is `200 / 401 / 403 / 404` — `403` propagates from the NFT gate inside `resolve_or_create_session` exactly like the canonical endpoint.

### 3.5 Affinity gating (`EXPOSE_AFFINITY_DEBUG`)

`/comp/affinity/{sid}` today lives on the **debug** router and is only registered when `EXPOSE_AFFINITY_DEBUG=true`. The vector exposes warmth/trust/intrigue/intimacy/patience/tension calibration internals — historically debug-only.

The BFF endpoint **honors the same gate**: `affinity: None` when `EXPOSE_AFFINITY_DEBUG=false`. Symmetric with the standalone endpoint. Prod currently has the gate open (`EXPOSE_AFFINITY_DEBUG="true"`) so prod behaviour is identical to "always present"; if a future operator closes the gate, both the standalone debug endpoint and the BFF affinity field disappear together. Promoting affinity to an unconditionally-stable production surface would be a deliberate later decision (`v2` BFF bump).

### 3.5.a Shared DTO move

The existing `AffinityDebugResponse` lives in `routes/debug.rs` and is only registered conditionally. The BFF endpoint needs the same shape on a non-debug code path. Rename + move:

```
routes/debug.rs       :  pub struct AffinityDebugResponse { ... }
→
routes/dto.rs (new)   :  pub struct AffinitySnapshot { ... }   // identical fields
routes/debug.rs       :  use crate::routes::dto::AffinitySnapshot;
routes/bff/companion.rs : use crate::routes::dto::AffinitySnapshot;
```

Add `impl From<eros_engine_core::affinity::Affinity> for AffinitySnapshot` to centralize the conversion currently inlined in `debug::get_affinity`.

### 3.6 Avoiding copy-paste with the engine start handler

`POST /comp/chat/start` and `POST /bff/v1/comp/chat/start` share the session-resolution flow (resolve `instance_id` → check NFT → resume-or-create session). Refactor that into a pure helper:

```rust
// routes/companion.rs (or new shared module)
pub(crate) struct ResolvedSession {
    pub session_id: Uuid,
    pub instance_id: Uuid,
    pub persona_name: String,
    pub is_new: bool,
}

pub(crate) async fn resolve_or_create_session(
    state: &AppState,
    user_id: Uuid,
    req: &StartChatLikeRequest,   // shared input shape, or accept the two fields directly
) -> Result<ResolvedSession, AppError> {
    // ... extract the body of the existing start_chat handler, minus the
    //     final Json(...) construction ...
}
```

Both `start_chat` (canonical, in `routes/companion.rs`) and `bff_start_chat` (in `routes/bff/companion.rs`) call this. The canonical engine handler keeps building its own response shape; the BFF handler builds the bundled shape. Zero behaviour change to the existing endpoint.

### 3.7 Backwards compatibility

`/comp/chat/start` is **completely untouched** — same request, same response, same wire shape. The BFF endpoint is purely additive.

### 3.8 Versioning

Plan C is **additive** — ships in any 0.2.x patch. No need to wait for 0.3.

### 3.9 Open items for plan stage

- **Whether to allow `history_limit = 0`.** Recommend: no. Clamp to `[1, 50]`. A caller that doesn't want history can use a different endpoint (or pagination later). Avoids ambiguous semantics.
- **Tracing / metrics.** Add `bff.start.bundle_emitted` counter so we can measure web adoption once the consumer PR lands. On each BFF handler add a `tracing` span with `route`, `session_id`, `instance_id`, `is_new`, `history_count`, `affinity_present`, `affinity_gate_open`, and elapsed durations for the resolve / history / affinity slices — handy for spotting the next bottleneck after Plans A/B/C land.
- **NFT-gate parity tests.** `resolve_or_create_session` must preserve the canonical handler's NFT-gate ordering (gate on explicit `instance_id` after the load+owner check; gate on `genome_id` before find-or-create). E2E coverage should hit both `/comp/chat/start` and `/bff/v1/comp/chat/start` with the same gated input and assert identical 4xx response.
- **Concurrent-first-call idempotency.** Canonical `/comp/chat/start` is resume-or-create with no DB-level unique `(user_id, instance_id, status='active')` constraint, so two simultaneous first calls can race and create two sessions. Plan C does not change this — but the web migration concentrates more cold-mount POSTs through this code path, which makes the race observable in a way it wasn't before. Plan stage should decide whether to add the unique constraint now or treat as a follow-up; do **not** silently inherit the race.

---

## 4. Combined impact (typical Taiwan user)

Current cold-mount profile:

| Step                 | Latency      |
|----------------------|--------------|
| Cold machine start   | 500–3000 ms  |
| RT 1: start          | 80–150 ms    |
| RT 2: history        | 80–150 ms    |
| RT 3: affinity       | 80–150 ms    |
| **Total**            | **740–3450 ms** |

After Plans A + B + C:

| Step                                       | Latency      |
|--------------------------------------------|--------------|
| Cold machine start                         | 0 ms (eliminated by Plan A) |
| RT 1: `/bff/v1/comp/chat/start` (bundled)  | 100–180 ms (both PG reads in parallel) |
| **Total**                                  | **100–180 ms** |

≈ **6–19× faster** at typical cold-mount.

---

## 5. Risk register

| #  | Plan | Risk                                                              | Severity | Mitigation                                                                |
|----|------|-------------------------------------------------------------------|----------|----------------------------------------------------------------------------|
| R1 | A    | $5/mo cost increase                                                | nil      | acceptable; product trade-off                                              |
| R2 | A    | Warm machine drifts state in some way (sqlx pool, JWKS cache)     | Low      | Engine binary holds no long-lived stateful caches that degrade with uptime; sqlx pool already does connection recycling |
| R3 | B    | Drift between `/comp/.../history` and `/bff/v1/comp/.../history`   | Low      | Inevitable by design — that's the whole point of the BFF layer. Plan-stage doc-comment on each handler points to the other for cross-reference. |
| R4 | C    | Bundle response too large at limit=50 with long messages          | Low      | per-message `content` capped at 4096 chars upstream; worst case 50 × 4096 ≈ 200 KB — within reasonable HTTP body budget |
| R5 | C    | Extracted `resolve_or_create_session` helper introduces a behaviour drift between engine and BFF start | Low | Same helper called from both handlers; covered by E2E tests that hit both endpoints with the same input and assert the resolved `session_id` matches |
| R6 | C    | `tokio::try_join!` doubles DB load per BFF start request           | nil      | both reads are cheap indexed lookups; aggregate engine DB load increases by ≪ 1% |
| R7 | C    | Brand-new session has no affinity row, handler erroring on absent  | Low      | §3.4 handles `Option<Affinity>` explicitly, returns `affinity: None` |
| R8 | B+C  | BFF surface grows organically and becomes its own monolith         | Medium   | Document the BFF convention (§0.1) in `dev_wiki/` once it has 3+ endpoints. Hard rule: BFF routes never call other BFF routes — always reach down to repos / engine helpers. |

---

## 6. Out of scope (intentionally deferred)

- **Web-side migration to consume `/bff/v1/comp/chat/start` and `/bff/v1/comp/chat/<sid>/history`.** Will land after both (a) engine 0.2.x ships Plans B + C, (b) the in-flight `eros-engine-web` SSE work merges. Tracking spec in `eros-engine-web/docs/superpowers/specs/` at that time. Until then, web keeps making 3 calls — already works, just slower.
- **Deprecation / removal of `/comp/chat/{sid}/history`.** OSS consumers may rely on it (with `extracted_facts`); leave it alone indefinitely.
- **Nuxt SSR prefetch of chat history.** Bigger architectural lift; revisit only if post-Plan-C latency is still a user complaint.
- **IndexedDB / localStorage cache of last-known history.** KISS-violating until measured need.
- **Cursor pagination (`sent_at < x`)** replacing offset pagination. Current scale doesn't justify.
- **Other `/bff/v1/*` endpoints** (e.g. `/bff/v1/comp/affinity/<sid>` standalone). The convention is established by this spec; specific new endpoints get their own spec when needed.

---

## 7. Implementation checklist (for plan stage)

```
Plan A — fly.toml (zero code, ship first)
  A1   fly.toml: min_machines_running 0 → 1; add cost-rationale comment
  A2   flyctl deploy; flyctl status confirms warm machine in idle state

Plan B — /bff/v1/comp/chat/{session_id}/history
  B1   crates/eros-engine-store/src/chat.rs:
         + struct ChatMessageSlim { role, content, sent_at }
         + fn ChatRepo::history_slim(session_id, limit, offset)
  B2   crates/eros-engine-server/src/routes/bff/mod.rs (new):
         pub mod companion;
         pub fn router() -> OpenApiRouter<AppState>  { /* merge of bff handlers */ }
  B3   crates/eros-engine-server/src/routes/bff/companion.rs (new):
         + BffHistoryEntry, BffHistoryResponse
         + handler bff_get_history  (distinct name so OpenAPI operationId
           doesn't collide with companion::get_history)
  B4   routes/mod.rs: add bff::router() to the `comp` merged subtree
         (so it inherits require_auth)
  B4a  routes/mod.rs: add bff::router() to router_for_openapi() too —
         CI diffs `openapi.json` against `print-openapi` output, so the
         BFF routes must appear in the openapi-extraction router
  B4b  openapi.rs: extend the `tags(...)` list with
         (name = "bff-companion", description = "BFF mirror of /comp/* shaped for eros-engine-web")
  B5   cargo test -p eros-engine-server  (BFF history E2E: success / 401 / 403 / 404,
         plus default-limit-is-50 assertion to pin the intentional divergence
         from canonical's default of 20)
  B6   regenerate OpenAPI snapshot:
         cargo run -p eros-engine-server --quiet -- print-openapi \
           > crates/eros-engine-server/openapi.json

Plan C — /bff/v1/comp/chat/start
  C1   routes/companion.rs: extract resolve_or_create_session as pub(crate) fn;
         refactor existing start_chat to call it. Preserve NFT-gate ordering
         (gate on explicit instance_id AFTER load+owner check; gate on
         genome_id BEFORE find-or-create — see §3.9 NFT-gate parity tests)
  C2   routes/dto.rs (new): move AffinityDebugResponse → AffinitySnapshot;
         add From<Affinity> for AffinitySnapshot;
         routes/debug.rs updates its import + re-export under old name for one release
  C3   routes/bff/companion.rs:
         + BffStartRequest, BffStartResponse
         + handler bff_start_chat (distinct name — see B3 note;
           uses resolve_or_create_session + history_slim + AffinityRepo)
  C4   cargo test -p eros-engine-server  (BFF start E2E:
         brand-new-session-empty-history / resumed-session-with-history /
         affinity-null-when-debug-off / affinity-present-when-debug-on /
         shared-resolver-matches-engine-endpoint /
         nft-gate-rejects-explicit-instance-id-not-owned /
         nft-gate-rejects-genome-without-nft)
  C5   regenerate OpenAPI snapshot (same command as B6)
```

Web migration (separately tracked in `eros-engine-web/docs/superpowers/specs/`):

```
W1   composables/useErosChat.ts: collapse loadHistory + hydrateAffinity + ensureSession
       into a single call to /bff/v1/comp/chat/start; drop the separate getHistory /
       getAffinity round trips
W2   lib/erosClient.ts: add bffStartChat(genomeId, opts) returning the bundled shape
W3   chat store rehydrate: consume bundle.history + bundle.affinity directly
```

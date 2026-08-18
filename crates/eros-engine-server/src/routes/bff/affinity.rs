// SPDX-License-Identifier: AGPL-3.0-only
//! BFF affinity surface (`/bff/v1/comp/affinity/*`).
//!
//! See docs/superpowers/specs/2026-05-20-affinity-event-delta-design.md §4 and
//! docs/superpowers/specs/2026-08-17-affinity-41-design.md §6 (the value
//! endpoint, and `state_after` on the event shape).

use axum::{
    extract::{Extension, Path, Query, State},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::affinity::AffinityRepo;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::routes::companion::{require_session_for_user, AffinityDeltasDto};
use crate::routes::dto::{AffinitySnapshot, BondChemistryDeltas, TurnLabelChangesDto};
use crate::state::AppState;

// ─── DTOs ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffAffinityDelta {
    /// Stable unique id of this event — the FE freshness/dedup key.
    pub event_id: Uuid,
    pub event_type: String, // "message" | "gift" | "proactive" | "ghost"
    /// Effective committed change of the latest user-turn event. All-zero for
    /// a ghost turn (AI didn't reply; no axis moved).
    pub effective_deltas: AffinityDeltasDto,
    /// Exact per-turn bond/chemistry delta (floored), read from the stored event
    /// column. None on pre-migration rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_deltas_computed: Option<BondChemistryDeltas>,
    /// Engine-authoritative per-turn tier transition; absent when no tier moved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_changes: Option<TurnLabelChangesDto>,
    /// Post-turn absolute state — axes, both line scores, both tiers, both
    /// judge levels. Absent on rows written before migration 0049.
    ///
    /// Takes the place of client-side accumulation: with this, a consumer
    /// adopts the authoritative absolute value every turn instead of adding
    /// deltas to a running total. It is a WRITE-TIME snapshot, so after an
    /// absence only `GET /bff/v1/comp/affinity/{sid}` is correct — that one
    /// refreshes the derived endpoints at read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_after: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffAffinityDeltaResponse {
    pub session_id: Uuid,
    /// None when no user-turn event yet (brand-new, or only time_decay), or
    /// when the latest user-turn event predates migration 0014.
    pub event: Option<BffAffinityDelta>,
}

/// Long-poll knobs for `bff_get_affinity_delta` (issue #147). The affinity
/// write lands in a post-process task *after* the chat stream's Final frame,
/// so without these a consumer can only short-poll for the per-turn delta.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct BffAffinityDeltaQuery {
    /// Long-poll baseline: the `event_id` the caller already has. While the
    /// session's latest turn event still matches (or none exists yet), the
    /// request is held until a newer event lands or `wait` elapses — the
    /// timed-out response returns the unchanged state, same shape as the
    /// immediate path. Absent ⇒ return the latest immediately (pre-#147
    /// behavior, unchanged).
    pub after: Option<Uuid>,
    /// How long to hold the request open, in milliseconds. Only meaningful
    /// with `after`. Default 10000, server-capped at 25000.
    pub wait: Option<u64>,
}

/// Default / ceiling for `wait` (ms). The cap keeps a held request well under
/// typical proxy/idle timeouts; a client wanting a longer horizon re-issues.
const LONG_POLL_DEFAULT_WAIT_MS: u64 = 10_000;
const LONG_POLL_MAX_WAIT_MS: u64 = 25_000;
/// Internal re-query cadence while holding. The post-process write commits
/// within ~a second of the Final frame in the common case, so a held request
/// resolves on the first tick or two.
const LONG_POLL_TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// Latest user-turn affinity delta as committed. For per-turn FE observation;
/// JWT + ownership checked. With `?after=<event_id>` the request long-polls (see
/// [`BffAffinityDeltaQuery`]), collapsing the per-turn short-poll loop into
/// one held request.
#[utoipa::path(
    get,
    path = "/bff/v1/comp/affinity/{session_id}/event",
    tag = "bff-companion",
    params(
        ("session_id" = Uuid, Path, description = "Chat session id"),
        BffAffinityDeltaQuery
    ),
    responses(
        (status = 200, body = BffAffinityDeltaResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session not found")
    ),
    security(("bearer" = []))
)]
async fn bff_get_affinity_delta(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Query(query): Query<BffAffinityDeltaQuery>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<BffAffinityDeltaResponse>, AppError> {
    require_session_for_user(&state, session_id, user_id).await?;

    let repo = AffinityRepo { pool: &state.pool };
    let mut row = repo.latest_turn_event(session_id).await?;

    if let Some(after) = query.after {
        let wait = query
            .wait
            .unwrap_or(LONG_POLL_DEFAULT_WAIT_MS)
            .min(LONG_POLL_MAX_WAIT_MS);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(wait);
        // Hold while there is nothing newer than the caller's baseline: the
        // latest event still IS the baseline, or no turn event exists yet.
        while row.as_ref().map(|r| r.id) == Some(after) || row.is_none() {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            tokio::time::sleep(LONG_POLL_TICK.min(deadline - now)).await;
            row = repo.latest_turn_event(session_id).await?;
        }
    }

    let event = row.and_then(|r| {
        // Pre-0014 rows have NULL effective_deltas → omit (don't fabricate).
        let effective = r.effective_deltas?;
        let effective_deltas: AffinityDeltasDto = serde_json::from_value(effective).ok()?;
        let effective_deltas_computed = r
            .effective_line_deltas
            .and_then(|v| serde_json::from_value::<BondChemistryDeltas>(v).ok());
        let label_changes = r
            .label_changes
            .and_then(|v| serde_json::from_value::<TurnLabelChangesDto>(v).ok());
        Some(BffAffinityDelta {
            event_id: r.id,
            event_type: r.event_type,
            effective_deltas,
            effective_deltas_computed,
            label_changes,
            state_after: r.state_after,
            created_at: r.created_at,
        })
    });

    Ok(Json(BffAffinityDeltaResponse { session_id, event }))
}

// ─── Absolute value ─────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffAffinityValueResponse {
    pub session_id: Uuid,
    /// None when the session has no affinity row yet. The row is created on
    /// the first turn, so a just-started session legitimately has none —
    /// render an empty relationship, not an error.
    pub affinity: Option<AffinitySnapshot>,
}

/// Absolute affinity for a session, refreshed at read time — the supported way
/// for a client to render a relationship.
///
/// `apply_time_decay()` + `refresh_endpoints()` run before serialising, and
/// that is the whole reason this endpoint exists rather than a direct read of
/// `engine.companion_affinity`: as of 4.0 `warmth`/`patience` are derived from
/// the judge level, the counterpart line and the elapsed gap, with the columns
/// holding only a write-time cache. A reader that selects the columns gets a
/// relationship that reads warmer the longer the user has been away.
///
/// Returns both the tier number and the tier key, so a client needs neither a
/// threshold table nor an ordered tier array.
#[utoipa::path(
    get,
    path = "/bff/v1/comp/affinity/{session_id}",
    tag = "bff-companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    responses(
        (status = 200, body = BffAffinityValueResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session not found")
    ),
    security(("bearer" = []))
)]
async fn bff_get_affinity_value(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<BffAffinityValueResponse>, AppError> {
    // Ownership via the same helper as the event endpoint, so both BFF
    // affinity routes report one status-code contract (404 unknown / 403 not
    // yours). No redundant `user_id` filter on the affinity row itself:
    // `companion_affinity.session_id` is UNIQUE and ownership is settled here.
    require_session_for_user(&state, session_id, user_id).await?;

    let affinity = AffinityRepo { pool: &state.pool }
        .load(session_id)
        .await?
        .map(|mut a| {
            a.apply_time_decay();
            a.refresh_endpoints(&state.config.affinity_tuning);
            AffinitySnapshot::from(a)
        });

    Ok(Json(BffAffinityValueResponse {
        session_id,
        affinity,
    }))
}

// ─── User-scoped list ───────────────────────────────────────────────

/// Page size default / ceiling. Nothing in the engine caps how many companions
/// a user may talk to, so the page size is the only bound on the response.
/// 200 is a typo guard, not a business gate — raise it by changing the number.
const LIST_DEFAULT_LIMIT: i64 = 50;
const LIST_MAX_LIMIT: i64 = 200;

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct BffAffinityListQuery {
    /// Page size. Default 50, server-capped at 200.
    pub limit: Option<i64>,
    /// Opaque position taken verbatim from the previous page's `next_cursor`.
    /// Absent ⇒ first page. Malformed ⇒ 400 (never a silent first page: that
    /// would restart a scroll at the top and look like duplicated rows).
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffAffinityListItem {
    pub session_id: Uuid,
    /// Companion this relationship is with. A client list keyed by companion
    /// joins on this without a second round trip to resolve the session.
    pub genome_id: Uuid,
    /// Never null here: the list is driven by the affinity rows themselves, so
    /// a session that has not had its first turn is absent rather than present
    /// with an empty value. Treat "missing from the response" as "no
    /// relationship yet" — the same state the single-session endpoint reports
    /// as `affinity: null`.
    pub affinity: AffinitySnapshot,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffAffinityListResponse {
    pub user_id: Uuid,
    pub items: Vec<BffAffinityListItem>,
    /// Pass as `cursor` to fetch the next page. Null on the last page.
    pub next_cursor: Option<String>,
}

/// Requested page size → the size actually served. Clamped, never rejected: a
/// client that asks for more than the ceiling wants "as many as you'll give
/// me", and a 400 there would break a list for no reader's benefit.
fn page_limit(requested: Option<i64>) -> i64 {
    requested
        .unwrap_or(LIST_DEFAULT_LIMIT)
        .clamp(1, LIST_MAX_LIMIT)
}

/// The keyset position, opaque on the wire. Encoded rather than exposed as two
/// query params so the sort key stays an implementation detail — a client that
/// reconstructs it by hand would pin the ordering.
fn encode_cursor(updated_at: DateTime<Utc>, session_id: Uuid) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(format!("{}|{session_id}", updated_at.to_rfc3339()))
}

fn decode_cursor(raw: &str) -> Result<(DateTime<Utc>, Uuid), AppError> {
    use base64::Engine as _;
    let bad = || AppError::BadRequest("malformed cursor".into());
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| bad())?;
    let text = String::from_utf8(decoded).map_err(|_| bad())?;
    let (ts, sid) = text.split_once('|').ok_or_else(bad)?;
    let ts = DateTime::parse_from_rfc3339(ts)
        .map_err(|_| bad())?
        .with_timezone(&Utc);
    Ok((ts, sid.parse().map_err(|_| bad())?))
}

/// Every companion's current affinity for one user, in one round trip —
/// newest-updated first, keyset-paginated. The `{user_id}` path parameter MUST
/// match the JWT's user_id; mismatch returns 403.
///
/// Exists because the single-session endpoint is the only authoritative read of
/// an absolute value, and a list view rendering one row per companion could
/// otherwise only call it N times or fall back to the write-time columns —
/// which read systematically warm after an absence, since absence decay only
/// moves the stored scores down. Every row here gets the same read-time refresh
/// as the single-session endpoint; a row that skipped it would be no better
/// than the column it replaces.
///
/// Ordering is `updated_at DESC, session_id DESC`, which puts the rows a list
/// actually shows first on page one. No 403/404 for an individual session: the
/// response only ever contains rows the caller owns, so a foreign session is
/// absent rather than reported.
#[utoipa::path(
    get,
    path = "/bff/v1/comp/affinities/{user_id}",
    tag = "bff-companion",
    params(
        ("user_id" = Uuid, Path, description = "Owner user id (must equal JWT sub)"),
        BffAffinityListQuery
    ),
    responses(
        (status = 200, body = BffAffinityListResponse),
        (status = 400, description = "malformed cursor"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "user_id does not match JWT")
    ),
    security(("bearer" = []))
)]
async fn bff_list_affinities(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    Query(query): Query<BffAffinityListQuery>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<Json<BffAffinityListResponse>, AppError> {
    if user_id != jwt_user {
        return Err(AppError::Forbidden("not your data".into()));
    }
    let limit = page_limit(query.limit);
    let after = query.cursor.as_deref().map(decode_cursor).transpose()?;

    // Read one past the page: whether that row exists is what tells a full page
    // apart from a last page, without a second COUNT.
    let mut rows = AffinityRepo { pool: &state.pool }
        .list_for_user(user_id, after, limit + 1)
        .await?;
    let has_more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);

    // From the stored values, before the read-time refresh — the keyset is on
    // the DB columns, and the refresh is in-memory only.
    let next_cursor = has_more
        .then(|| {
            rows.last()
                .map(|r| encode_cursor(r.affinity.updated_at, r.affinity.session_id))
        })
        .flatten();

    let items = rows
        .into_iter()
        .map(|r| {
            let mut a = r.affinity;
            let session_id = a.session_id;
            a.apply_time_decay();
            a.refresh_endpoints(&state.config.affinity_tuning);
            BffAffinityListItem {
                session_id,
                genome_id: r.genome_id,
                affinity: AffinitySnapshot::from(a),
            }
        })
        .collect();

    Ok(Json(BffAffinityListResponse {
        user_id,
        items,
        next_cursor,
    }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(bff_get_affinity_delta))
        .routes(routes!(bff_get_affinity_value))
        .routes(routes!(bff_list_affinities))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
    };
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        build_router, mint_test_jwt, seed_genome, seed_instance, seed_session, send_request,
    };

    async fn seed_affinity(
        pool: &PgPool,
        session_id: Uuid,
        user_id: Uuid,
        instance_id: Uuid,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn seed_event(
        pool: &PgPool,
        affinity_id: Uuid,
        event_type: &str,
        eff_warmth: f64,
        secs_ago: i64,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, created_at) \
             VALUES ($1, $2, '{}'::jsonb, $3, now() - make_interval(secs => $4)) \
             RETURNING id",
        )
        .bind(affinity_id)
        .bind(event_type)
        .bind(json!({ "warmth": eff_warmth }))
        .bind(secs_ago as f64)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn req(token: &str, sid: Uuid) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(format!("/bff/v1/comp/affinity/{sid}/event"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_returns_latest_post_ema_delta(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let aid = seed_affinity(&pool, session_id, user_id, instance_id).await;
        seed_event(&pool, aid, "message", 0.05, 20).await;
        seed_event(&pool, aid, "gift", 0.12, 10).await; // newest turn

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, req(&token, session_id)).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        let ev = &body["event"];
        assert_eq!(ev["event_type"], "gift");
        assert!(ev["event_id"].is_string());
        assert!((ev["effective_deltas"]["warmth"].as_f64().unwrap() - 0.12).abs() < 1e-9);
        // No raw pre-decay field on the BFF shape.
        assert!(ev.get("deltas").is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_ghost_after_message_returns_zero_ghost(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let aid = seed_affinity(&pool, session_id, user_id, instance_id).await;
        seed_event(&pool, aid, "message", 0.2, 20).await;
        seed_event(&pool, aid, "ghost", 0.0, 10).await; // newest user turn
        seed_event(&pool, aid, "time_decay", -0.05, 5).await; // newest overall, excluded

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, req(&token, session_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["event"]["event_type"], "ghost"); // not the older message
        assert_eq!(
            body["event"]["effective_deltas"]["warmth"]
                .as_f64()
                .unwrap(),
            0.0
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_null_when_no_turn_event(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let aid = seed_affinity(&pool, session_id, user_id, instance_id).await;
        seed_event(&pool, aid, "time_decay", -0.05, 5).await; // only background

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, req(&token, session_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["event"].is_null());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_403_on_other_users_session(pool: PgPool) {
        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(intruder);
        let (status, _b) = send_request(&mut app, req(&token, session_id)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_includes_computed_delta_and_label_changes(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let aid = seed_affinity(&pool, session_id, user_id, instance_id).await;

        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, effective_line_deltas, label_changes, created_at) \
             VALUES ($1, 'message', '{}'::jsonb, $2, $3, $4, now())",
        )
        .bind(aid)
        .bind(json!({ "warmth": 0.3, "trust": 0.3 }))
        .bind(json!({ "bond": 0.2, "chemistry": 0.1 }))
        .bind(json!({ "bond": { "from": "acquaintance", "to": "friend" } }))
        .execute(&pool)
        .await
        .unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, req(&token, session_id)).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        let ev = &body["event"];
        assert!((ev["effective_deltas_computed"]["bond"].as_f64().unwrap() - 0.2).abs() < 1e-9);
        assert!(
            (ev["effective_deltas_computed"]["chemistry"]
                .as_f64()
                .unwrap()
                - 0.1)
                .abs()
                < 1e-9
        );
        assert_eq!(ev["label_changes"]["bond"]["to"], "friend");
    }

    fn req_longpoll(token: &str, sid: Uuid, after: Uuid, wait_ms: u64) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(format!(
                "/bff/v1/comp/affinity/{sid}/event?after={after}&wait={wait_ms}"
            ))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_longpoll_stale_after_returns_immediately(pool: PgPool) {
        // `after` differing from the latest event_id ⇒ the caller is behind;
        // return the latest event at once, no hold.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let aid = seed_affinity(&pool, session_id, user_id, instance_id).await;
        seed_event(&pool, aid, "message", 0.05, 10).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let started = std::time::Instant::now();
        let (status, body) = send_request(
            &mut app,
            req_longpoll(&token, session_id, Uuid::new_v4(), 5_000),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        assert_eq!(body["event"]["event_type"], "message");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(2_000),
            "stale `after` must not hold the request"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_longpoll_returns_new_event_when_it_lands(pool: PgPool) {
        // `after` == latest ⇒ hold; a newer event landing mid-hold is returned
        // well before the requested wait elapses.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let aid = seed_affinity(&pool, session_id, user_id, instance_id).await;
        let baseline = seed_event(&pool, aid, "message", 0.05, 10).await;

        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let inserter = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            seed_event(&pool, aid, "gift", 0.12, 0).await
        });

        let started = std::time::Instant::now();
        let (status, body) =
            send_request(&mut app, req_longpoll(&token, session_id, baseline, 10_000)).await;
        let new_id = inserter.await.unwrap();
        assert_eq!(status, StatusCode::OK, "body={body}");
        assert_eq!(body["event"]["event_type"], "gift");
        assert_eq!(body["event"]["event_id"], new_id.to_string());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(8_000),
            "must return on the new event, not the full wait"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_longpoll_timeout_returns_unchanged(pool: PgPool) {
        // Nothing new within `wait` ⇒ hold for ~wait, then return the (stale)
        // latest event unchanged — same shape as the immediate path.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let aid = seed_affinity(&pool, session_id, user_id, instance_id).await;
        let baseline = seed_event(&pool, aid, "message", 0.05, 10).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let started = std::time::Instant::now();
        let (status, body) =
            send_request(&mut app, req_longpoll(&token, session_id, baseline, 500)).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        assert_eq!(body["event"]["event_id"], baseline.to_string());
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(450),
            "matching `after` must hold until the wait elapses"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_longpoll_no_event_times_out_null(pool: PgPool) {
        // No turn event at all: `after` (a client can only guess a sentinel
        // here) holds until timeout, then returns event: null as today.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        seed_affinity(&pool, session_id, user_id, instance_id).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let started = std::time::Instant::now();
        let (status, body) = send_request(
            &mut app,
            req_longpoll(&token, session_id, Uuid::new_v4(), 400),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        assert!(body["event"].is_null());
        assert!(started.elapsed() >= std::time::Duration::from_millis(350));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_401_without_bearer(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let r = Request::builder()
            .uri(format!("/bff/v1/comp/affinity/{session_id}/event"))
            .body(Body::empty())
            .unwrap();
        let (status, _b) = send_request(&mut app, r).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // ─── Value endpoint ─────────────────────────────────────────────

    fn value_req(token: &str, sid: Uuid) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(format!("/bff/v1/comp/affinity/{sid}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_value_returns_scores_tiers_and_labels(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        seed_affinity(&pool, session_id, user_id, instance_id).await;
        // bond = (0.6 + 0.6) / 2 = 0.6 → tier 3.
        sqlx::query("UPDATE engine.companion_affinity SET trust = 0.6, intrigue = 0.6 WHERE session_id = $1")
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, value_req(&token, session_id)).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        let a = &body["affinity"];
        assert!((a["bond"].as_f64().unwrap() - 0.6).abs() < 1e-9);
        assert_eq!(a["bond_tier"], 3);
        assert_eq!(a["bond_label"], "close_friend");
        assert_eq!(a["chem_tier"], 1);
        assert_eq!(a["chemistry_label"], "spark");
        // The retired legacy label must not reappear on the new surface.
        assert!(a.get("relationship_label").is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_value_refreshes_endpoints_at_read(pool: PgPool) {
        // The whole point of the endpoint over a direct SELECT: warmth is a
        // derived cache, and an absence must cool it at READ time.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        seed_affinity(&pool, session_id, user_id, instance_id).await;
        sqlx::query(
            "UPDATE engine.companion_affinity \
             SET warmth = 0.9, warmth_grade = 3, intimacy = 0.5, tension = 0.5, \
                 updated_at = now() - interval '30 days' WHERE session_id = $1",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let stored: f64 = sqlx::query_scalar(
            "SELECT warmth FROM engine.companion_affinity WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, value_req(&token, session_id)).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        let served = body["affinity"]["warmth"].as_f64().unwrap();
        assert!(
            served < stored,
            "a 30-day absence must cool warmth at read: served {served} vs stored {stored}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_value_null_when_no_affinity_row(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        // No affinity row: the session has not had its first turn.
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, value_req(&token, session_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["affinity"].is_null());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_value_403_on_other_users_session(pool: PgPool) {
        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        seed_affinity(&pool, session_id, owner, instance_id).await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(intruder);
        let (status, _b) = send_request(&mut app, value_req(&token, session_id)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_value_404_on_unknown_session(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, _b) = send_request(&mut app, value_req(&token, Uuid::new_v4())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_value_401_without_bearer(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let r = Request::builder()
            .uri(format!("/bff/v1/comp/affinity/{session_id}"))
            .body(Body::empty())
            .unwrap();
        let (status, _b) = send_request(&mut app, r).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn retired_debug_routes_are_gone(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        seed_affinity(&pool, session_id, user_id, instance_id).await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        for path in [
            format!("/comp/affinity/{session_id}"),
            format!("/comp/affinity/{session_id}/event"),
        ] {
            let r = Request::builder()
                .uri(&path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let (status, _b) = send_request(&mut app, r).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path} must be retired");
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_event_omits_state_after_on_legacy_rows(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let aid = seed_affinity(&pool, session_id, user_id, instance_id).await;
        seed_event(&pool, aid, "message", 0.05, 10).await; // no state columns

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, req(&token, session_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["event"].get("state_after").is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_event_serves_state_after(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let aid = seed_affinity(&pool, session_id, user_id, instance_id).await;
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, state_after) \
             VALUES ($1, 'message', '{}'::jsonb, $2, $3)",
        )
        .bind(aid)
        .bind(json!({ "trust": 0.3 }))
        .bind(json!({ "bond": 0.42, "bond_tier": 3, "chem_tier": 1 }))
        .execute(&pool)
        .await
        .unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, req(&token, session_id)).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        assert_eq!(body["event"]["state_after"]["bond_tier"], 3);
        assert!((body["event"]["state_after"]["bond"].as_f64().unwrap() - 0.42).abs() < 1e-9);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_value_fresh_row_serves_endpoint_defaults(pool: PgPool) {
        // Migrated from the retired debug route: a fresh row's derived
        // endpoints are level 2 over empty lines, i.e. 1/3 · B(0).
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Solace").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        seed_affinity(&pool, session_id, user_id, instance_id).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, value_req(&token, session_id)).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        let a = &body["affinity"];
        let expect = (1.0 / 3.0) * (1.0 - 0.35 * 10.0 / 13.0);
        assert!((a["warmth"].as_f64().unwrap() - expect).abs() < 1e-9);
        assert!((a["patience"].as_f64().unwrap() - expect).abs() < 1e-9);
        assert!(a["intrigue"].as_f64().unwrap().abs() < 1e-9);
        assert_eq!(a["bond_tier"], 1);
        assert_eq!(a["chem_tier"], 1);
    }

    // ─── User-scoped list ───────────────────────────────────────────

    fn list_req(token: &str, user_id: Uuid, query: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(format!("/bff/v1/comp/affinities/{user_id}{query}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    /// One companion for `user_id`, its affinity row stamped `mins_ago` minutes
    /// back so list order is deterministic. Returns `(session_id, genome_id)`.
    async fn seed_companion(
        pool: &PgPool,
        user_id: Uuid,
        name: &str,
        mins_ago: i32,
    ) -> (Uuid, Uuid) {
        let genome_id = seed_genome(pool, name).await;
        let instance_id = seed_instance(pool, genome_id, user_id).await;
        let session_id = seed_session(pool, user_id, instance_id).await;
        seed_affinity(pool, session_id, user_id, instance_id).await;
        sqlx::query(
            "UPDATE engine.companion_affinity \
             SET updated_at = now() - make_interval(mins => $2) WHERE session_id = $1",
        )
        .bind(session_id)
        .bind(mins_ago)
        .execute(pool)
        .await
        .unwrap();
        (session_id, genome_id)
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_list_returns_every_companion_newest_first(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (s_old, _g_old) = seed_companion(&pool, user_id, "Aria", 300).await;
        let (s_mid, _g_mid) = seed_companion(&pool, user_id, "Solace", 120).await;
        let (s_new, g_new) = seed_companion(&pool, user_id, "Vesper", 5).await;
        // A stranger's relationship must not leak into the caller's list.
        seed_companion(&pool, Uuid::new_v4(), "Nyx", 1).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, list_req(&token, user_id, "")).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        assert_eq!(body["user_id"], user_id.to_string());
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 3, "body={body}");
        assert_eq!(items[0]["session_id"], s_new.to_string());
        assert_eq!(items[0]["genome_id"], g_new.to_string());
        assert_eq!(items[1]["session_id"], s_mid.to_string());
        assert_eq!(items[2]["session_id"], s_old.to_string());
        // Same snapshot shape as the single-session endpoint.
        assert_eq!(items[0]["affinity"]["bond_tier"], 1);
        assert_eq!(items[0]["affinity"]["bond_label"], "acquaintance");
        assert!(body["next_cursor"].is_null(), "single page → no cursor");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_list_omits_sessions_without_an_affinity_row(pool: PgPool) {
        // A session that has not had its first turn has no relationship to
        // report — it is absent, not present-and-empty.
        let user_id = Uuid::new_v4();
        let (with_row, _g) = seed_companion(&pool, user_id, "Aria", 10).await;
        let genome_id = seed_genome(&pool, "Solace").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        seed_session(&pool, user_id, instance_id).await; // no affinity row

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, list_req(&token, user_id, "")).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "body={body}");
        assert_eq!(items[0]["session_id"], with_row.to_string());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_list_refreshes_endpoints_at_read(pool: PgPool) {
        // The whole point over reading the columns: a row untouched for 30 days
        // must be served cooler than it is stored, on the list too.
        let user_id = Uuid::new_v4();
        let (session_id, _g) = seed_companion(&pool, user_id, "Aria", 0).await;
        sqlx::query(
            "UPDATE engine.companion_affinity \
             SET warmth = 0.9, warmth_grade = 3, intimacy = 0.5, tension = 0.5, \
                 updated_at = now() - interval '30 days' WHERE session_id = $1",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();
        let stored: f64 = sqlx::query_scalar(
            "SELECT warmth FROM engine.companion_affinity WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, list_req(&token, user_id, "")).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        let served = body["items"][0]["affinity"]["warmth"].as_f64().unwrap();
        assert!(
            served < stored,
            "a 30-day absence must cool warmth at read: served {served} vs stored {stored}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_list_pages_through_the_cursor(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (s_old, _a) = seed_companion(&pool, user_id, "Aria", 300).await;
        let (s_mid, _b) = seed_companion(&pool, user_id, "Solace", 120).await;
        let (s_new, _c) = seed_companion(&pool, user_id, "Vesper", 5).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, page1) = send_request(&mut app, list_req(&token, user_id, "?limit=2")).await;
        assert_eq!(status, StatusCode::OK, "body={page1}");
        let items = page1["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["session_id"], s_new.to_string());
        assert_eq!(items[1]["session_id"], s_mid.to_string());
        let cursor = page1["next_cursor"].as_str().expect("more rows → cursor");

        let (status, page2) = send_request(
            &mut app,
            list_req(&token, user_id, &format!("?limit=2&cursor={cursor}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={page2}");
        let items = page2["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "body={page2}");
        assert_eq!(items[0]["session_id"], s_old.to_string());
        assert!(
            page2["next_cursor"].is_null(),
            "last page must end the walk: {page2}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_list_paging_survives_identical_updated_at(pool: PgPool) {
        // Timestamps tie in practice (a batch write, a coarse clock). Without
        // the session_id tiebreak in both the sort and the cursor, a page
        // boundary landing on a tie skips a row or serves it twice.
        let user_id = Uuid::new_v4();
        for name in ["Aria", "Solace", "Vesper"] {
            seed_companion(&pool, user_id, name, 60).await;
        }
        sqlx::query(
            "UPDATE engine.companion_affinity SET updated_at = timestamptz '2026-01-01 00:00:00Z' \
             WHERE user_id = $1",
        )
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let mut seen: Vec<String> = Vec::new();
        let mut query = "?limit=1".to_string();
        for _ in 0..3 {
            let (status, body) = send_request(&mut app, list_req(&token, user_id, &query)).await;
            assert_eq!(status, StatusCode::OK, "body={body}");
            let items = body["items"].as_array().unwrap();
            assert_eq!(items.len(), 1, "body={body}");
            seen.push(items[0]["session_id"].as_str().unwrap().to_string());
            match body["next_cursor"].as_str() {
                Some(c) => query = format!("?limit=1&cursor={c}"),
                None => break,
            }
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 3, "every tied row must be served exactly once");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_list_empty_for_a_user_with_no_rows(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, list_req(&token, user_id, "")).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        assert!(body["items"].as_array().unwrap().is_empty());
        assert!(body["next_cursor"].is_null());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_list_403_for_another_users_id(pool: PgPool) {
        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        seed_companion(&pool, owner, "Aria", 10).await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(intruder);
        let (status, _b) = send_request(&mut app, list_req(&token, owner, "")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_list_400_on_malformed_cursor(pool: PgPool) {
        let user_id = Uuid::new_v4();
        seed_companion(&pool, user_id, "Aria", 10).await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, _b) =
            send_request(&mut app, list_req(&token, user_id, "?cursor=not-a-cursor")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_affinity_list_401_without_bearer(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let state = test_state(pool);
        let mut app = build_router(state);
        let r = Request::builder()
            .uri(format!("/bff/v1/comp/affinities/{user_id}"))
            .body(Body::empty())
            .unwrap();
        let (status, _b) = send_request(&mut app, r).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn page_limit_clamps_both_ends() {
        use super::{page_limit, LIST_DEFAULT_LIMIT, LIST_MAX_LIMIT};
        assert_eq!(page_limit(None), LIST_DEFAULT_LIMIT);
        assert_eq!(page_limit(Some(10)), 10);
        assert_eq!(page_limit(Some(9_999)), LIST_MAX_LIMIT);
        assert_eq!(page_limit(Some(0)), 1);
        assert_eq!(page_limit(Some(-5)), 1);
    }

    #[test]
    fn cursor_round_trips() {
        use super::{decode_cursor, encode_cursor};
        let sid = Uuid::new_v4();
        let ts = chrono::DateTime::parse_from_rfc3339("2026-08-18T04:05:06.123456Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let (got_ts, got_sid) = decode_cursor(&encode_cursor(ts, sid)).unwrap();
        assert_eq!(got_ts, ts);
        assert_eq!(got_sid, sid);
    }
}

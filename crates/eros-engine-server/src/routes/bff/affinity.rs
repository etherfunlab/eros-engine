// SPDX-License-Identifier: AGPL-3.0-only
//! BFF affinity surface (`/bff/v1/comp/affinity/*`).
//!
//! See docs/superpowers/specs/2026-05-20-affinity-event-delta-design.md §4.

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
use crate::routes::dto::{BondChemistryDeltas, TurnLabelChangesDto};
use crate::state::AppState;

// ─── DTOs ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffAffinityDelta {
    /// Stable unique id of this event — the FE freshness/dedup key.
    pub event_id: Uuid,
    pub event_type: String, // "message" | "gift" | "proactive" | "ghost"
    /// Post-EMA effective change of the latest user-turn event. All-zero for
    /// a ghost turn (AI didn't reply; no axis moved).
    pub effective_deltas: AffinityDeltasDto,
    /// Exact per-turn bond/chemistry delta (floored), read from the stored event
    /// column. None on pre-migration rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_deltas_computed: Option<BondChemistryDeltas>,
    /// Engine-authoritative per-turn tier transition; absent when no tier moved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_changes: Option<TurnLabelChangesDto>,
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

/// Latest user-turn affinity delta (post-EMA). For per-turn FE observation.
/// NOT behind EXPOSE_AFFINITY_DEBUG (the FE owns this surface); still JWT +
/// ownership checked. With `?after=<event_id>` the request long-polls (see
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
            created_at: r.created_at,
        })
    });

    Ok(Json(BffAffinityDeltaResponse { session_id, event }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(bff_get_affinity_delta))
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
        // No raw pre-EMA field on the BFF shape.
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
    async fn bff_affinity_present_when_debug_gate_closed(pool: PgPool) {
        // BFF is NOT behind EXPOSE_AFFINITY_DEBUG.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let aid = seed_affinity(&pool, session_id, user_id, instance_id).await;
        seed_event(&pool, aid, "message", 0.05, 10).await;

        let mut state = test_state(pool);
        state.config.expose_affinity_debug = false; // closed — must NOT hide BFF
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, req(&token, session_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["event"]["event_type"], "message");
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
}

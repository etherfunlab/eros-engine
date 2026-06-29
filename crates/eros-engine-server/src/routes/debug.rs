// SPDX-License-Identifier: AGPL-3.0-only
// TODO(T12): handler is wired in by `routes::router` once main.rs
// constructs an AppState. Tests cover the live path before then.
#![allow(dead_code)]

//! Env-gated debug endpoints. Enabled only when
//! `state.config.expose_affinity_debug == true` (env: `EXPOSE_AFFINITY_DEBUG`).

use axum::{
    extract::{Extension, Path, Query, State},
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::affinity::AffinityRepo;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::routes::companion::{require_session_for_user, AffinityDeltasDto};
use crate::routes::dto::{AffinitySnapshot, BondChemistryDeltas, TurnLabelChangesDto};
use crate::state::AppState;

/// Back-compat alias retained for one release so in-crate callers that
/// referenced the old type name don't break instantly. Will be removed
/// in the next minor; new code uses `AffinitySnapshot` directly.
///
/// Note: this alias does NOT help external OpenAPI consumers — utoipa
/// resolves `pub type` transparently so the snapshot already renames
/// to `AffinitySnapshot`. Any OpenAPI codegen will see the rename
/// immediately on upgrade.
#[deprecated(
    since = "0.2.1",
    note = "use `crate::routes::dto::AffinitySnapshot` directly; alias will be removed next minor"
)]
pub type AffinityDebugResponse = AffinitySnapshot;

/// Inspect the live affinity vector for a session. The session must be
/// owned by the JWT user; otherwise 403.
#[utoipa::path(
    get,
    path = "/comp/affinity/{session_id}",
    tag = "debug",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    responses(
        (status = 200, body = AffinitySnapshot),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session has no affinity")
    ),
    security(("bearer" = []))
)]
async fn get_affinity(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Path(session_id): Path<Uuid>,
) -> Result<Json<AffinitySnapshot>, AppError> {
    let repo = AffinityRepo { pool: &state.pool };
    let mut a = repo
        .load(session_id)
        .await?
        .ok_or_else(|| AppError::NotFound("session has no affinity".into()))?;
    if a.user_id != user_id {
        return Err(AppError::Forbidden("not your session".into()));
    }
    a.apply_time_decay();
    Ok(Json(AffinitySnapshot::from(a)))
}

// ---------------------------------------------------------------------------
// Affinity-event log
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct AffinityEventsQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub event_type: Option<String>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct AffinityEventEntry {
    /// Stable unique id of this event (FE freshness/dedup key).
    pub event_id: Uuid,
    pub event_type: String,
    /// Pre-EMA raw delta the pipeline decided.
    pub deltas: AffinityDeltasDto,
    /// Post-EMA effective change (after − before). None for pre-0014 rows.
    pub effective_deltas: Option<AffinityDeltasDto>,
    /// Post-EMA delta folded into the two lines (raw-composite units). None for
    /// pre-0014 rows with no `effective_deltas`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_deltas_computed: Option<BondChemistryDeltas>,
    /// Per-turn tier transition; absent when no tier moved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_changes: Option<TurnLabelChangesDto>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct AffinityEventsResponse {
    pub session_id: Uuid,
    pub events: Vec<AffinityEventEntry>,
    /// Count returned in this page (== events.len()), NOT the grand total.
    pub total: usize,
}

const VALID_EVENT_TYPES: [&str; 5] = ["message", "ghost", "gift", "proactive", "time_decay"];

/// Paginated affinity-event log for a session — pre-EMA + post-EMA deltas.
/// Debug-gated (same router as get_affinity).
#[utoipa::path(
    get,
    path = "/comp/affinity/{session_id}/event",
    tag = "debug",
    params(
        ("session_id" = Uuid, Path, description = "Chat session id"),
        ("limit" = Option<i64>, Query, description = "Max rows (default 20, capped at 100)"),
        ("offset" = Option<i64>, Query, description = "Page offset, default 0"),
        ("event_type" = Option<String>, Query,
            description = "Filter: message|ghost|gift|proactive|time_decay")
    ),
    responses(
        (status = 200, body = AffinityEventsResponse),
        (status = 400, description = "invalid event_type"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session not found")
    ),
    security(("bearer" = []))
)]
async fn get_affinity_events(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Query(query): Query<AffinityEventsQuery>,
) -> Result<Json<AffinityEventsResponse>, AppError> {
    require_session_for_user(&state, session_id, user_id).await?;

    if let Some(et) = query.event_type.as_deref() {
        if !VALID_EVENT_TYPES.contains(&et) {
            return Err(AppError::BadRequest(format!("invalid event_type: {et}")));
        }
    }
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);

    let rows = AffinityRepo { pool: &state.pool }
        .list_events(session_id, limit, offset, query.event_type.as_deref())
        .await?;

    let events: Vec<AffinityEventEntry> = rows
        .into_iter()
        .map(|r| {
            let effective_deltas: Option<AffinityDeltasDto> =
                r.effective_deltas.and_then(|v| serde_json::from_value(v).ok());
            let effective_deltas_computed =
                effective_deltas.as_ref().map(BondChemistryDeltas::from_axis_deltas);
            let label_changes = r
                .label_changes
                .and_then(|v| serde_json::from_value::<TurnLabelChangesDto>(v).ok());
            AffinityEventEntry {
                event_id: r.id,
                event_type: r.event_type,
                deltas: serde_json::from_value(r.deltas).unwrap_or_default(),
                effective_deltas,
                effective_deltas_computed,
                label_changes,
                created_at: r.created_at,
            }
        })
        .collect();
    let total = events.len();

    Ok(Json(AffinityEventsResponse {
        session_id,
        events,
        total,
    }))
}

/// Build the debug router. Returns an empty router when `enabled = false`
/// so the routes are completely absent from the OpenAPI spec / runtime
/// in production.
pub fn router(enabled: bool) -> OpenApiRouter<AppState> {
    if !enabled {
        return OpenApiRouter::new();
    }
    OpenApiRouter::new()
        .routes(routes!(get_affinity))
        .routes(routes!(get_affinity_events))
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

    // Insert an affinity row + one event with explicit effective_deltas.
    async fn seed_affinity_event(
        pool: &PgPool,
        session_id: Uuid,
        user_id: Uuid,
        instance_id: Uuid,
        event_type: &str,
        eff_warmth: f64,
    ) {
        let affinity_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (session_id) DO UPDATE SET user_id = EXCLUDED.user_id \
             RETURNING id",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(affinity_id)
        .bind(event_type)
        .bind(json!({ "warmth": eff_warmth * 2.0 })) // pre-EMA (arbitrary)
        .bind(json!({ "warmth": eff_warmth })) // post-EMA
        .execute(pool)
        .await
        .unwrap();
    }

    fn req(token: &str, sid: Uuid, query: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(format!("/comp/affinity/{sid}/event{query}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn affinity_events_returns_pre_and_post_deltas(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        seed_affinity_event(&pool, session_id, user_id, instance_id, "message", 0.08).await;

        // gate open by default in test_state.
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, body) = send_request(&mut app, req(&token, session_id, "")).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        let ev = &body["events"][0];
        assert_eq!(ev["event_type"], "message");
        assert!(ev["event_id"].is_string());
        assert!((ev["deltas"]["warmth"].as_f64().unwrap() - 0.16).abs() < 1e-9);
        assert!((ev["effective_deltas"]["warmth"].as_f64().unwrap() - 0.08).abs() < 1e-9);
        assert_eq!(body["total"], 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn affinity_events_absent_when_debug_gate_closed(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;

        let mut state = test_state(pool);
        state.config.expose_affinity_debug = false; // gate closed → route not registered
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, _b) = send_request(&mut app, req(&token, session_id, "")).await;
        // Gate closed ⇒ the /event route is not registered, so an authenticated
        // request to it finds no matching route and gets a clean 404 — proving
        // the debug handler never ran.
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "gate-closed event route must be absent (404); got {status}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn affinity_events_invalid_event_type_400(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, _b) =
            send_request(&mut app, req(&token, session_id, "?event_type=bogus")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn affinity_events_owned_empty_session_200(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, req(&token, session_id, "")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["events"].as_array().unwrap().is_empty());
        assert_eq!(body["total"], 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn affinity_events_403_on_other_users_session(pool: PgPool) {
        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(intruder);
        let (status, _b) = send_request(&mut app, req(&token, session_id, "")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn affinity_events_includes_computed_delta_and_label_changes(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let affinity_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, label_changes) \
             VALUES ($1, 'message', '{}'::jsonb, $2, $3)",
        )
        .bind(affinity_id)
        .bind(json!({ "warmth": 0.3, "intimacy": 0.3 }))
        .bind(json!({ "chemistry": { "from": "spark", "to": "flirtation" } }))
        .execute(&pool)
        .await
        .unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(&mut app, req(&token, session_id, "")).await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        let ev = &body["events"][0];
        // fold: bond = (0.3+0+0)/3 = 0.1 ; chemistry = (0.3+0.3+0)/3 = 0.2
        assert!((ev["effective_deltas_computed"]["bond"].as_f64().unwrap() - 0.1).abs() < 1e-9);
        assert!((ev["effective_deltas_computed"]["chemistry"].as_f64().unwrap() - 0.2).abs() < 1e-9);
        assert_eq!(ev["label_changes"]["chemistry"]["to"], "flirtation");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn affinity_events_401_without_bearer(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let state = test_state(pool);
        let mut app = build_router(state);
        let r = Request::builder()
            .uri(format!("/comp/affinity/{session_id}/event"))
            .body(Body::empty())
            .unwrap();
        let (status, _b) = send_request(&mut app, r).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}

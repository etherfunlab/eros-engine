// SPDX-License-Identifier: AGPL-3.0-only
//! BFF affinity surface (`/bff/v1/comp/affinity/*`).
//!
//! See docs/superpowers/specs/2026-05-20-affinity-event-delta-design.md §4.

use axum::{
    extract::{Extension, Path, State},
    Json,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::affinity::AffinityRepo;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::routes::companion::{require_session_for_user, AffinityDeltasDto};
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
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffAffinityDeltaResponse {
    pub session_id: Uuid,
    /// None when no user-turn event yet (brand-new, or only time_decay), or
    /// when the latest user-turn event predates migration 0014.
    pub event: Option<BffAffinityDelta>,
}

/// Latest user-turn affinity delta (post-EMA). For per-turn FE observation.
/// NOT behind EXPOSE_AFFINITY_DEBUG (the FE owns this surface); still JWT +
/// ownership checked.
#[utoipa::path(
    get,
    path = "/bff/v1/comp/affinity/{session_id}/event",
    tag = "bff-companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
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
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<BffAffinityDeltaResponse>, AppError> {
    require_session_for_user(&state, session_id, user_id).await?;

    let event = AffinityRepo { pool: &state.pool }
        .latest_turn_event(session_id)
        .await?
        .and_then(|r| {
            // Pre-0014 rows have NULL effective_deltas → omit (don't fabricate).
            let effective = r.effective_deltas?;
            let effective_deltas: AffinityDeltasDto = serde_json::from_value(effective).ok()?;
            Some(BffAffinityDelta {
                event_id: r.id,
                event_type: r.event_type,
                effective_deltas,
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
    ) {
        sqlx::query(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, created_at) \
             VALUES ($1, $2, '{}'::jsonb, $3, now() - make_interval(secs => $4))",
        )
        .bind(affinity_id)
        .bind(event_type)
        .bind(json!({ "warmth": eff_warmth }))
        .bind(secs_ago as f64)
        .execute(pool)
        .await
        .unwrap();
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

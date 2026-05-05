// SPDX-License-Identifier: AGPL-3.0-only
// TODO(T12): handler is wired in by `routes::router` once main.rs
// constructs an AppState. Tests cover the live path before then.
#![allow(dead_code)]

//! Env-gated debug endpoints. Enabled only when
//! `state.config.expose_affinity_debug == true` (env: `EXPOSE_AFFINITY_DEBUG`).

use axum::{
    extract::{Extension, Path, State},
    Json,
};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::affinity::AffinityRepo;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct AffinityDebugResponse {
    pub warmth: f64,
    pub trust: f64,
    pub intrigue: f64,
    pub intimacy: f64,
    pub patience: f64,
    pub tension: f64,
    pub ghost_streak: i32,
    pub total_ghosts: i32,
    pub relationship_label: Option<String>,
    pub updated_at: String,
}

/// Inspect the live affinity vector for a session. The session must be
/// owned by the JWT user; otherwise 403.
#[utoipa::path(
    get,
    path = "/comp/affinity/{session_id}",
    tag = "debug",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    responses(
        (status = 200, body = AffinityDebugResponse),
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
) -> Result<Json<AffinityDebugResponse>, AppError> {
    let repo = AffinityRepo { pool: &state.pool };
    let mut a = repo
        .load(session_id)
        .await?
        .ok_or_else(|| AppError::NotFound("session has no affinity".into()))?;
    if a.user_id != user_id {
        return Err(AppError::Forbidden("not your session".into()));
    }
    a.apply_time_decay();

    let label = a.relationship_label.map(|l| {
        use eros_engine_core::affinity::RelationshipLabel as L;
        match l {
            L::Stranger => "stranger",
            L::Romantic => "romantic",
            L::Friend => "friend",
            L::Frenemy => "frenemy",
            L::SlowBurn => "slow_burn",
        }
        .to_string()
    });

    Ok(Json(AffinityDebugResponse {
        warmth: a.warmth,
        trust: a.trust,
        intrigue: a.intrigue,
        intimacy: a.intimacy,
        patience: a.patience,
        tension: a.tension,
        ghost_streak: a.ghost_streak,
        total_ghosts: a.total_ghosts,
        relationship_label: label,
        updated_at: a.updated_at.to_rfc3339(),
    }))
}

/// Build the debug router. Returns an empty router when `enabled = false`
/// so the routes are completely absent from the OpenAPI spec / runtime
/// in production.
pub fn router(enabled: bool) -> OpenApiRouter<AppState> {
    if !enabled {
        return OpenApiRouter::new();
    }
    OpenApiRouter::new().routes(routes!(get_affinity))
}

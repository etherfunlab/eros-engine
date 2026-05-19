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
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::affinity::AffinityRepo;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::routes::dto::AffinitySnapshot;
use crate::state::AppState;

/// Back-compat alias retained for one release so external OpenAPI consumers
/// that referenced the old type name don't break instantly. Will be removed
/// in the next minor; new code uses `AffinitySnapshot` directly.
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

/// Build the debug router. Returns an empty router when `enabled = false`
/// so the routes are completely absent from the OpenAPI spec / runtime
/// in production.
pub fn router(enabled: bool) -> OpenApiRouter<AppState> {
    if !enabled {
        return OpenApiRouter::new();
    }
    OpenApiRouter::new().routes(routes!(get_affinity))
}

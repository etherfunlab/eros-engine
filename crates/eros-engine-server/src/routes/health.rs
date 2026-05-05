// SPDX-License-Identifier: AGPL-3.0-only
use axum::Json;
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

/// Health check response.
#[derive(Serialize, utoipa::ToSchema)]
pub struct HealthResponse {
    pub status: String,
    pub service: String,
    pub version: String,
    pub timestamp: String,
}

/// Liveness probe.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "health",
    responses(
        (status = 200, description = "Service is up", body = HealthResponse)
    )
)]
async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        service: "eros-engine".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    })
}

/// Generic in `S` so the health router can be merged into both the
/// pre-T12 stateless main composition and the T11+ AppState-backed
/// router. The handler takes no state.
pub fn router<S>() -> OpenApiRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    OpenApiRouter::new().routes(routes!(healthz))
}

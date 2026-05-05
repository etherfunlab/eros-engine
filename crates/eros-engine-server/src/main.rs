// SPDX-License-Identifier: AGPL-3.0-only
mod auth;
mod error;
mod openapi;
mod pipeline;
mod prompt;
mod routes;
mod state;

// TODO(T12): Construct AppState (pool + auth + config) and wire the
// `auth::middleware::require_auth` layer onto protected route groups once
// DATABASE_URL handling and the Supabase JWT secret env wiring land.

use axum::Router;
use utoipa_axum::router::OpenApiRouter;
use utoipa_scalar::{Scalar, Servable};

use crate::openapi::ApiDoc;
use crate::state::ServerConfig;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cfg = ServerConfig::from_env();

    // OpenAPI-aware router. Each route module contributes via OpenApiRouter +
    // utoipa::path annotations; the spec is composed at boot.
    let (open_router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(routes::health::router())
        .split_for_parts();

    let app: Router = open_router
        .merge(Scalar::with_url("/docs", api))
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!(addr = %cfg.bind_addr, "eros-engine starting");
    axum::serve(listener, app).await?;
    Ok(())
}

// Bring the OpenApi trait in scope for ApiDoc::openapi() above.
use utoipa::OpenApi;

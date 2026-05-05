// SPDX-License-Identifier: AGPL-3.0-only
//! Top-level OpenAPI document. Each router that wants to be in the spec
//! lives inside `utoipa_axum::router::OpenApiRouter` and gets `.routes(routes!(...))`.

use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "eros-engine API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Open-source AI companion engine — chat + 6-dim affinity + memory. \
                       Source of truth: src/routes/."
    ),
    servers(
        (url = "https://erosnx.etherfun.net", description = "Production (Fly.io Tokyo NRT)"),
        (url = "http://localhost:8080", description = "Local dev")
    ),
    tags(
        (name = "health", description = "Liveness probe"),
        (name = "companion", description = "Chat sessions, messages, affinity, profile"),
        (name = "debug", description = "Env-gated introspection (affinity vector exposure)")
    )
)]
pub struct ApiDoc;

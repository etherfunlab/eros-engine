// SPDX-License-Identifier: AGPL-3.0-only
//! Top-level OpenAPI document. Each router that wants to be in the spec
//! lives inside `utoipa_axum::router::OpenApiRouter` and gets `.routes(routes!(...))`.

use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

/// Registers the `bearer` security scheme referenced by every authenticated
/// handler's `security(("bearer" = []))`. Without this, the generated spec
/// has dangling security requirements and fails standard OpenAPI validators
/// and client codegen.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .get_or_insert_with(utoipa::openapi::Components::new);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some("Supabase JWT bearer token"))
                    .build(),
            ),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "eros-engine API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Open-source AI companion engine — chat + 6-dim affinity + memory. \
                       Source of truth: src/routes/."
    ),
    servers(
        (url = "http://localhost:8080", description = "Local dev")
    ),
    tags(
        (name = "health", description = "Liveness probe"),
        (name = "companion", description = "Chat sessions, messages, affinity, profile"),
        (name = "debug", description = "Env-gated introspection (affinity vector exposure)")
    ),
    modifiers(&SecurityAddon)
)]
pub struct ApiDoc;

// SPDX-License-Identifier: AGPL-3.0-only
//! Top-level OpenAPI document. Each router that wants to be in the spec
//! lives inside `utoipa_axum::router::OpenApiRouter` and gets `.routes(routes!(...))`.

use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

/// Registers the security schemes used by handlers:
///   - `bearer` — Supabase JWT, attached to /comp/* and /bff/* routes
///   - `insights_export_secret` — `x-insights-export-secret` header, /svc/v1/* routes
///
/// Without this declaration the generated spec has dangling
/// `security(...)` references and fails standard OpenAPI validators.
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
        components.add_security_scheme(
            "insights_export_secret",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "x-insights-export-secret",
                "Shared secret for the service sub-tree; INSIGHTS_EXPORT_SECRET on the deployment",
            ))),
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
        (name = "svc", description = "Service-to-service routes, admitted by a shared secret header"),
        (name = "companion", description = "Chat sessions, messages, affinity, profile"),
        (name = "bff-companion", description = "Frontend-shaped mirror of /comp/* for first-party clients; \
                                                shape may diverge from canonical without notice"),
        (name = "group", description = "Group chat: the user and up to three personas, each an existing 1:1 session")
    ),
    modifiers(&SecurityAddon)
)]
pub struct ApiDoc;

// SPDX-License-Identifier: AGPL-3.0-only
//! Top-level OpenAPI document. Each router that wants to be in the spec
//! lives inside `utoipa_axum::router::OpenApiRouter` and gets `.routes(routes!(...))`.

use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::auth::middleware::CALLER_SECRET_HEADER;
use crate::routes::svc::{ERASE_SECRET_HEADER, EXPORT_SECRET_HEADER, FACTS_SECRET_HEADER};

/// Registers the security schemes used by handlers:
///   - `bearer` — Supabase JWT, attached to /comp/* and /bff/* routes
///   - `caller_secret` — `x-caller-secret` header, required on the whole bearer tree when the
///     deployment sets `CALLER_SECRET`; declared once, not per route
///   - `insights_export_secret` — `x-insights-export-secret` header, the insights /svc/v1/* routes
///   - `facts_export_secret` — `x-facts-export-secret` header, the facts /svc/v1/* routes
///   - `user_erase_secret` — `x-user-erase-secret` header, the user-erase /svc/v1/* routes
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
            "caller_secret",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                CALLER_SECRET_HEADER,
                "Deployment-wide caller secret; required on every bearer-authenticated route \
                 when the deployment sets CALLER_SECRET",
            ))),
        );
        components.add_security_scheme(
            "insights_export_secret",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                EXPORT_SECRET_HEADER,
                "Shared secret for the service sub-tree; INSIGHTS_EXPORT_SECRET on the deployment",
            ))),
        );
        components.add_security_scheme(
            "facts_export_secret",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                FACTS_SECRET_HEADER,
                "Shared secret for the facts export sub-tree; FACTS_EXPORT_SECRET on the deployment",
            ))),
        );
        components.add_security_scheme(
            "user_erase_secret",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                ERASE_SECRET_HEADER,
                "Shared secret for the user-erase sub-tree; USER_ERASE_SECRET on the deployment",
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
                       Source of truth: src/routes/. When the deployment sets CALLER_SECRET, \
                       every bearer-authenticated route also requires the x-caller-secret header."
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
        (name = "group", description = "Group chat: the user and up to three personas, each an existing 1:1 session"),
        (name = "byoc", description = "Bring your own character: mint a key, then create an instance under it. \
                                        Admitted by a shared secret header")
    ),
    modifiers(&SecurityAddon)
)]
pub struct ApiDoc;

#[cfg(test)]
mod tests {
    use super::ApiDoc;
    use utoipa::OpenApi;

    #[test]
    fn declares_the_caller_secret_scheme() {
        let doc = serde_json::to_value(ApiDoc::openapi()).unwrap();
        let s = &doc["components"]["securitySchemes"]["caller_secret"];
        assert_eq!(s["type"], "apiKey");
        assert_eq!(s["in"], "header");
        assert_eq!(s["name"], "x-caller-secret");
    }
}

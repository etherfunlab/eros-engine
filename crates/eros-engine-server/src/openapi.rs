// SPDX-License-Identifier: AGPL-3.0-only
//! Top-level OpenAPI document. Each router that wants to be in the spec
//! lives inside `utoipa_axum::router::OpenApiRouter` and gets `.routes(routes!(...))`.

use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

/// Registers the two security schemes used by handlers:
///   - `bearer`         — Supabase JWT, attached to /comp/* routes
///   - `hmac_signature` — server-to-server HMAC, attached to /s2s/* routes
///
/// Without these declarations the generated spec has dangling
/// `security(...)` references and fails standard OpenAPI validators /
/// client codegen. The HMAC scheme is modelled as an API-key-in-header
/// because OpenAPI 3.1 has no first-class "request signature" vocabulary;
/// the canonical signing string layout is documented in `auth::s2s`.
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
            "hmac_signature",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "X-S2S-Signature",
                "HMAC-SHA256 signature over the canonical signing string. \
                 Requires companion `X-S2S-Timestamp` header. \
                 See auth::s2s for the canonical layout.",
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
        (name = "companion", description = "Chat sessions, messages, affinity, profile"),
        (name = "debug", description = "Env-gated introspection (affinity vector exposure)"),
        (name = "s2s", description = "Server-to-server endpoints for the marketplace svc \
                                      (HMAC-signed; do not expose to user-agent traffic)")
    ),
    modifiers(&SecurityAddon)
)]
pub struct ApiDoc;

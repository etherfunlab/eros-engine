// SPDX-License-Identifier: AGPL-3.0-only
//! Top-level route composition.
//!
//! The HTTP surface is split into:
//!   * Public: `/healthz` (no auth)
//!   * Authenticated: `/comp/*` (Supabase JWT bearer required)
//!     - `/comp/personas`, `/comp/chat/*`, `/comp/user/{id}/profile`, ...
//!     - `/comp/affinity/{session_id}` only when
//!       `EXPOSE_AFFINITY_DEBUG=true`.

use axum::middleware::from_fn_with_state;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

pub mod companion;
pub mod debug;
pub mod health;
pub mod s2s;

/// Compose the full app router. The auth middleware is applied to the
/// `/comp/*` subtree only.
///
/// The `#[utoipa::path]` annotations on companion + debug handlers
/// already include the `/comp/...` prefix, so we MERGE rather than
/// NEST: nesting would double the prefix to `/comp/comp/...`. The
/// auth layer attaches only to the comp/debug merged sub-router, so
/// the public `/healthz` route stays unauthenticated even after the
/// outer merge into the parent router.
pub fn router(state: AppState) -> OpenApiRouter<AppState> {
    let comp = OpenApiRouter::new()
        .merge(companion::router())
        .merge(debug::router(state.config.expose_affinity_debug))
        .layer(from_fn_with_state(state.clone(), require_auth));

    OpenApiRouter::new().merge(health::router()).merge(comp)
}

/// Same shape as [`router`] for OpenAPI extraction purposes, minus the auth
/// middleware (which doesn't affect the spec) and minus any need for a real
/// `AppState` (which would otherwise require a live DB pool). Used by the
/// `print-openapi` subcommand and the CI drift check.
pub fn router_for_openapi(expose_affinity_debug: bool) -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(health::router())
        .merge(companion::router())
        .merge(debug::router(expose_affinity_debug))
}

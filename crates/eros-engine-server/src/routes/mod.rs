// SPDX-License-Identifier: AGPL-3.0-only
//! Top-level route composition.
//!
//! The HTTP surface is split into three independently-authed sub-trees:
//!   * Public:        `/healthz` — no auth
//!   * Bearer JWT:    `/comp/*`  — Supabase JWT (see auth::middleware)
//!   * HMAC S2S:      `/s2s/*`   — shared-secret signature (see auth::s2s)
//!
//! Auth layers are applied to the per-subtree merge, NOT the top-level
//! merge. This is load-bearing: stacking `require_auth` then `require_s2s`
//! at the parent would require every request to satisfy both, locking
//! every consumer out of one tree or the other. Keeping them on separate
//! sub-routers makes each authentication scheme independent and reflects
//! the per-route `security(...)` annotations in the OpenAPI spec.

use axum::middleware::from_fn_with_state;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::middleware::require_auth;
use crate::auth::s2s::require_s2s;
use crate::state::AppState;

pub mod companion;
pub mod debug;
pub mod health;
pub mod s2s;

/// Compose the full app router with auth layers attached.
///
/// The `#[utoipa::path]` annotations on companion / debug / s2s handlers
/// already include the full route prefix, so we MERGE rather than NEST:
/// nesting would double the prefix to `/comp/comp/...`. Each auth layer
/// attaches only to its merged sub-router, so the public `/healthz`
/// route stays unauthenticated even after the outer merge.
pub fn router(state: AppState) -> OpenApiRouter<AppState> {
    let comp = OpenApiRouter::new()
        .merge(companion::router())
        .merge(debug::router(state.config.expose_affinity_debug))
        .layer(from_fn_with_state(state.clone(), require_auth));

    let s2s_routes = s2s::router().layer(from_fn_with_state(state.clone(), require_s2s));

    OpenApiRouter::new()
        .merge(health::router())
        .merge(comp)
        .merge(s2s_routes)
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
        .merge(s2s::router())
}

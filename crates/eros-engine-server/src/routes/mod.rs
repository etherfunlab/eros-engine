// SPDX-License-Identifier: AGPL-3.0-only
//! Top-level route composition.
//!
//! The HTTP surface is split into two independently-authed sub-trees:
//!   * Public:        `/healthz` — no auth
//!   * Bearer JWT:    `/comp/*`, `/bff/v1/*`, `/world/*`, `/persona/*` — Supabase JWT (see auth::middleware)
//!
//! The auth layer is applied to the `/comp` + `/bff` merge, NOT the
//! top-level merge, so the public `/healthz` route stays unauthenticated
//! after the outer merge.

use axum::middleware::from_fn_with_state;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::middleware::require_auth;
use crate::state::AppState;

pub mod bff;
pub mod companion;
pub mod companion_async;
pub mod companion_stream;
pub mod dto;
pub mod health;
pub mod image_edit;
pub mod insight;
pub mod persona;
pub mod voice;
pub mod world_town;

/// Compose the full app router with auth layers attached.
///
/// The `#[utoipa::path]` annotations on companion handlers
/// already include the full route prefix, so we MERGE rather than NEST:
/// nesting would double the prefix to `/comp/comp/...`. Each auth layer
/// attaches only to its merged sub-router, so the public `/healthz`
/// route stays unauthenticated even after the outer merge.
pub fn router(state: AppState) -> OpenApiRouter<AppState> {
    let comp = OpenApiRouter::new()
        .merge(companion::router())
        .merge(companion_async::router())
        .merge(companion_stream::router())
        .merge(voice::router())
        .merge(bff::router())
        .merge(world_town::router())
        .merge(persona::router())
        .merge(insight::router())
        .merge(image_edit::router())
        .layer(from_fn_with_state(state.clone(), require_auth));

    OpenApiRouter::new().merge(health::router()).merge(comp)
}

/// Same shape as [`router`] for OpenAPI extraction purposes, minus the auth
/// middleware (which doesn't affect the spec) and minus any need for a real
/// `AppState` (which would otherwise require a live DB pool). Used by the
/// `print-openapi` subcommand and the CI drift check.
pub fn router_for_openapi() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(health::router())
        .merge(companion::router())
        .merge(companion_async::router())
        .merge(companion_stream::router())
        .merge(voice::router())
        .merge(bff::router())
        .merge(world_town::router())
        .merge(persona::router())
        .merge(insight::router())
        .merge(image_edit::router())
}

/// The generated OpenAPI document as JSON, for tests that assert on the wire
/// contract itself (path presence, `deprecated` flags) rather than on handler
/// behaviour.
#[cfg(test)]
pub(crate) fn openapi_for_tests() -> serde_json::Value {
    let (_router, api) = router_for_openapi().split_for_parts();
    serde_json::to_value(api).expect("openapi serialises")
}

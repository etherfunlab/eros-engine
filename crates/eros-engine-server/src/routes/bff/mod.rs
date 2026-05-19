// SPDX-License-Identifier: AGPL-3.0-only
#![allow(dead_code)] // wired into routes::router in Task 5.
//! BFF (backend-for-frontend) routes — `/bff/v1/<area>/*`.
//!
//! Frontend-shaped mirror of the canonical engine routes. See
//! `docs/superpowers/specs/2026-05-20-history-latency-cuts-design.md`
//! §0.1 for the layer convention.
//!
//! Convention quick-reference:
//!   * `/bff/v1/<area>/*` mirrors `/<area>/*` path-for-path; shape diverges.
//!   * Canonical routes are NEVER edited to satisfy the FE; add a BFF
//!     route instead.
//!   * BFF handlers never call other BFF handlers — they reach down to
//!     repos / shared helpers directly.
//!   * Each BFF handler has a distinct Rust function name (e.g.
//!     `bff_get_history`) so utoipa-axum emits a unique `operationId`.

use utoipa_axum::router::OpenApiRouter;

use crate::state::AppState;

pub mod companion;

/// Compose all `/bff/v1/*` handlers into one router. Auth is applied
/// at the call site in `routes::router` (the merged `comp` subtree's
/// `require_auth` layer covers this).
pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().merge(companion::router())
}

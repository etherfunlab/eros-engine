// SPDX-License-Identifier: AGPL-3.0-only
//! BFF mirror of `/comp/*` (`/bff/v1/comp/*`).
//!
//! See `docs/superpowers/specs/2026-05-20-history-latency-cuts-design.md`
//! §0.1 (convention), §2 (history endpoint), §3 (start endpoint).

#![allow(dead_code)] // handlers wired into routes::router in Task 5.

use utoipa_axum::router::OpenApiRouter;

use crate::state::AppState;

/// Build the BFF companion router. Handlers are added in the implementation
/// order specified by the plan: `bff_get_history` first (Task 5), then
/// `bff_start_chat` (Task 10).
pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
}

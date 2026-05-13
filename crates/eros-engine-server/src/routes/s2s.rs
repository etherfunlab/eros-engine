// SPDX-License-Identifier: AGPL-3.0-only
//! Server-to-server endpoints called by eros-marketplace-svc. Mounted at
//! /s2s/* with HMAC auth (see auth::s2s); deliberately outside the
//! Supabase JWT layer that gates /comp/*.
//!
//! Wire shape mirrors svc's expected /since cursor pagination and stale-
//! write-protected /upsert semantics. Inputs are validated at the API
//! boundary (base58 32-byte for pubkeys/asset_ids) so non-canonical
//! representations cannot create logical duplicates downstream.

// Tasks 12-14 wire the store imports into handler bodies and the
// router into routes::router() composition; until then everything in
// this module is dead from the binary's POV (the integration tests
// added alongside the real handlers will exercise it directly, same
// pattern as companion.rs).
#![allow(dead_code, unused_imports)]

use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use eros_engine_store::ownership::OwnershipRepo;
use eros_engine_store::pubkey::validate_solana_pubkey;
use eros_engine_store::wallets::WalletLinkRepo;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Deserialize, ToSchema)]
pub struct WalletUpsertRequest {
    pub user_id: Uuid,
    pub wallet_pubkey: String,
    pub linked: bool,
    pub source_updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct OwnershipUpsertRequest {
    pub asset_id: String,
    pub persona_id: String,
    pub owner_wallet: String,
    pub source_updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SinceCursor {
    pub ts: DateTime<Utc>,
    pub pk: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WalletsSinceResponse {
    // `WalletLink` lives in eros-engine-store which (intentionally) does
    // not derive `ToSchema` to avoid a utoipa dep in the store crate.
    // The schema is rendered as an opaque object array here; the real
    // wire shape is fully described by `WalletUpsertRequest` (same
    // fields). Task 14 may revisit if the marketplace svc consumer
    // needs a stricter spec.
    #[schema(value_type = Vec<Object>)]
    pub rows: Vec<eros_engine_store::wallets::WalletLink>,
    pub next_cursor: Option<SinceCursor>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OwnershipSinceResponse {
    #[schema(value_type = Vec<Object>)]
    pub rows: Vec<eros_engine_store::ownership::Ownership>,
    pub next_cursor: Option<SinceCursor>,
}

/// Stub — implemented in Task 12.
#[utoipa::path(
    post,
    path = "/s2s/wallets/upsert",
    tag = "s2s",
    security(("hmac_signature" = [])),
    request_body = WalletUpsertRequest,
    responses(
        (status = 204, description = "applied"),
        (status = 401, description = "missing or invalid HMAC"),
        (status = 409, description = "stale event (source_updated_at older than existing)")
    )
)]
async fn wallets_upsert(
    State(state): State<AppState>,
    Json(req): Json<WalletUpsertRequest>,
) -> Result<StatusCode, AppError> {
    let pubkey = validate_solana_pubkey(&req.wallet_pubkey)
        .map_err(|e| AppError::BadRequest(format!("invalid wallet_pubkey: {e}")))?;
    let applied = WalletLinkRepo { pool: &state.pool }
        .upsert(req.user_id, &pubkey, req.linked, req.source_updated_at)
        .await
        .map_err(AppError::from)?;
    if applied {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::CONFLICT)
    }
}

/// Stub — implemented in Task 14.
#[utoipa::path(
    get,
    path = "/s2s/wallets/since",
    tag = "s2s",
    security(("hmac_signature" = [])),
    params(
        ("cursor_ts" = Option<DateTime<Utc>>, Query, description = "compound cursor: source_updated_at"),
        ("cursor_pk" = Option<String>, Query, description = "compound cursor: user_id:wallet_pubkey"),
        ("limit" = Option<i64>, Query, description = "1..1000, default 100"),
    ),
    responses(
        (status = 200, body = WalletsSinceResponse),
        (status = 401, description = "missing or invalid HMAC"),
    )
)]
async fn wallets_since(
    State(_state): State<AppState>,
) -> Result<Json<WalletsSinceResponse>, AppError> {
    Err(AppError::Internal("not implemented".into()))
}

/// Stub — implemented in Task 13.
#[utoipa::path(
    post,
    path = "/s2s/ownership/upsert",
    tag = "s2s",
    security(("hmac_signature" = [])),
    request_body = OwnershipUpsertRequest,
    responses(
        (status = 204, description = "applied"),
        (status = 401, description = "missing or invalid HMAC"),
        (status = 409, description = "stale event"),
    )
)]
async fn ownership_upsert(
    State(_state): State<AppState>,
    Json(_req): Json<OwnershipUpsertRequest>,
) -> Result<StatusCode, AppError> {
    Err(AppError::Internal("not implemented".into()))
}

/// Stub — implemented in Task 14.
#[utoipa::path(
    get,
    path = "/s2s/ownership/since",
    tag = "s2s",
    security(("hmac_signature" = [])),
    params(
        ("cursor_ts" = Option<DateTime<Utc>>, Query),
        ("cursor_pk" = Option<String>, Query),
        ("limit" = Option<i64>, Query),
    ),
    responses(
        (status = 200, body = OwnershipSinceResponse),
        (status = 401, description = "missing or invalid HMAC"),
    )
)]
async fn ownership_since(
    State(_state): State<AppState>,
) -> Result<Json<OwnershipSinceResponse>, AppError> {
    Err(AppError::Internal("not implemented".into()))
}

/// Build the /s2s/* subrouter. The HMAC layer is applied at router
/// composition time (routes/mod.rs), not here.
pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(wallets_upsert))
        .routes(routes!(wallets_since))
        .routes(routes!(ownership_upsert))
        .routes(routes!(ownership_since))
}

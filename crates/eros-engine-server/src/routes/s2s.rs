// SPDX-License-Identifier: AGPL-3.0-only
//! Server-to-server endpoints called by eros-marketplace-svc. Mounted at
//! /s2s/* with HMAC auth (see auth::s2s); deliberately outside the
//! Supabase JWT layer that gates /comp/*.
//!
//! Wire shape mirrors svc's expected /since cursor pagination and stale-
//! write-protected /upsert semantics. Inputs are validated at the API
//! boundary (base58 32-byte for pubkeys/asset_ids) so non-canonical
//! representations cannot create logical duplicates downstream.

// Handlers are reachable in the production binary as of Task 14 (routes
// composed into the public router via routes::router). The `dead_code`
// allow remains because some DTO fields (e.g. SinceCursor) are only
// touched through serde at the network boundary and aren't otherwise
// read by Rust code; same convention as companion.rs.
#![allow(dead_code)]

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
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

/// Query params for both /since endpoints. All three fields are
/// optional: missing `cursor_ts` starts from the unix epoch, missing
/// `cursor_pk` starts before the lexicographically-smallest pk, and
/// missing `limit` defaults to 100 (clamped to 1..=1000).
#[derive(Debug, Deserialize)]
pub struct SinceParams {
    pub cursor_ts: Option<DateTime<Utc>>,
    pub cursor_pk: Option<String>,
    pub limit: Option<i64>,
}

impl SinceParams {
    fn resolved(self) -> (DateTime<Utc>, String, i64) {
        let ts = self
            .cursor_ts
            .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
        let pk = self.cursor_pk.unwrap_or_default();
        let limit = self.limit.unwrap_or(100).clamp(1, 1000);
        (ts, pk, limit)
    }
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

/// Apply a single wallet-link change (link or unlink). Idempotent under
/// stale-write protection (older source_updated_at → 409).
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

/// Cursor-paginated read of wallet links since a given (ts, pk) point.
/// `next_cursor` is `None` when fewer rows than `limit` were returned,
/// signalling that the consumer has caught up. The compound cursor uses
/// `"{user_id}:{wallet_pubkey}"` as the pk component to break ties
/// among rows sharing a `source_updated_at`.
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
    State(state): State<AppState>,
    Query(params): Query<SinceParams>,
) -> Result<Json<WalletsSinceResponse>, AppError> {
    let (ts, pk, limit) = params.resolved();
    let rows = WalletLinkRepo { pool: &state.pool }
        .since(ts, &pk, limit)
        .await
        .map_err(AppError::from)?;
    let next_cursor = if (rows.len() as i64) < limit {
        None
    } else {
        rows.last().map(|last| SinceCursor {
            ts: last.source_updated_at,
            pk: format!("{}:{}", last.user_id, last.wallet_pubkey),
        })
    };
    Ok(Json(WalletsSinceResponse { rows, next_cursor }))
}

/// Apply a single ownership change (NFT bought / sold / transferred).
/// Idempotent under stale-write protection (older source_updated_at → 409).
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
    State(state): State<AppState>,
    Json(req): Json<OwnershipUpsertRequest>,
) -> Result<StatusCode, AppError> {
    let asset = validate_solana_pubkey(&req.asset_id)
        .map_err(|e| AppError::BadRequest(format!("invalid asset_id: {e}")))?;
    let owner = validate_solana_pubkey(&req.owner_wallet)
        .map_err(|e| AppError::BadRequest(format!("invalid owner_wallet: {e}")))?;
    let applied = OwnershipRepo { pool: &state.pool }
        .upsert(&asset, &req.persona_id, &owner, req.source_updated_at)
        .await
        .map_err(AppError::from)?;
    if applied {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::CONFLICT)
    }
}

/// Cursor-paginated read of ownership rows since a given (ts, asset_id)
/// point. Same `next_cursor` semantics as `wallets_since`. The pk
/// component is `asset_id` (it's already globally unique within the
/// table, no compound key needed).
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
    State(state): State<AppState>,
    Query(params): Query<SinceParams>,
) -> Result<Json<OwnershipSinceResponse>, AppError> {
    let (ts, pk, limit) = params.resolved();
    let rows = OwnershipRepo { pool: &state.pool }
        .since(ts, &pk, limit)
        .await
        .map_err(AppError::from)?;
    let next_cursor = if (rows.len() as i64) < limit {
        None
    } else {
        rows.last().map(|last| SinceCursor {
            ts: last.source_updated_at,
            pk: last.asset_id.clone(),
        })
    };
    Ok(Json(OwnershipSinceResponse { rows, next_cursor }))
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

// End-to-end /s2s/* integration tests. These exercise the full middleware
// stack (`require_s2s` layered on the merged sub-router via
// `routes::router`) against a real Postgres, so a signed upsert really
// does land a row in `engine.wallet_links`.
#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use chrono::Utc;
    use sqlx::PgPool;

    /// Build a test app with `marketplace_s2s_secret` pre-set so the HMAC
    /// middleware accepts signed requests. The companion test_state
    /// helper is reused so the JWT validator is wired identically to the
    /// /comp/* tests — keeps the two test surfaces in lock-step.
    fn build_app_with_secret(pool: PgPool, secret: &str) -> axum::Router {
        let mut state = crate::routes::companion::test_state(pool);
        state.marketplace_s2s_secret = Some(secret.to_string());
        let (open_router, _api) = crate::routes::router(state.clone()).split_for_parts();
        open_router.with_state(state)
    }

    /// Build a canonical signing string + sign it, mirroring exactly what
    /// the marketplace-svc client will do at runtime.
    fn sign_request(
        secret: &str,
        method: &str,
        path: &str,
        body: &[u8],
        timestamp: &str,
    ) -> String {
        use sha2::Digest;
        let body_hash = sha2::Sha256::digest(body);
        let body_hex = hex::encode(body_hash);
        let canonical =
            crate::auth::s2s::canonical_signing_string(method, path, "", timestamp, &body_hex);
        crate::auth::s2s::sign(secret.as_bytes(), &canonical)
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn rejects_request_without_signature(pool: PgPool) {
        let app = build_app_with_secret(pool, "test-secret");
        let req = Request::builder()
            .method("POST")
            .uri("/s2s/wallets/upsert")
            .body(Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn signed_upsert_persists(pool: PgPool) {
        let user = uuid::Uuid::new_v4();
        let body = serde_json::json!({
            "user_id": user,
            "wallet_pubkey": "11111111111111111111111111111111",
            "linked": true,
            "source_updated_at": Utc::now(),
        });
        let body_bytes = serde_json::to_vec(&body).unwrap();
        let ts = Utc::now().to_rfc3339();
        let sig = sign_request(
            "test-secret",
            "POST",
            "/s2s/wallets/upsert",
            &body_bytes,
            &ts,
        );

        let app = build_app_with_secret(pool.clone(), "test-secret");
        let req = Request::builder()
            .method("POST")
            .uri("/s2s/wallets/upsert")
            .header("content-type", "application/json")
            .header("x-s2s-timestamp", ts)
            .header("x-s2s-signature", sig)
            .body(Body::from(body_bytes))
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), 204);

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM engine.wallet_links WHERE user_id = $1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1);
    }
}

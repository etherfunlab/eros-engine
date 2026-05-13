// SPDX-License-Identifier: AGPL-3.0-only
//! Self-heal pull: when MARKETPLACE_SVC_URL is configured, periodically
//! call svc's /s2s/{ownership,wallets}/since to pick up any pushes
//! the engine missed. Cursors persisted in engine.sync_cursors.

use chrono::Utc;
use eros_engine_store::ownership::{Ownership, OwnershipRepo};
use eros_engine_store::sync_cursors::{Cursor, SyncCursorRepo};
use eros_engine_store::wallets::{WalletLink, WalletLinkRepo};
use std::time::Duration;
use tracing::{info, warn};

use crate::auth::s2s::{build_outbound_signature, canonicalize_query};
use crate::state::AppState;

const TICK_SECS: u64 = 5 * 60;
const PAGE_LIMIT: i64 = 500;

// `next_cursor` is still part of svc's wire shape, but we no longer
// trust it for cursor advancement — svc returns `None` on the catch-up
// page (rows < limit), which would otherwise leave our cursor stuck at
// the previous page boundary and re-pull the tail slice every tick.
// We advance off `rows.last()` instead (see `tick_*` below).
#[derive(serde::Deserialize)]
struct OwnershipSinceResp {
    rows: Vec<Ownership>,
    #[allow(dead_code)]
    next_cursor: Option<SinceCursorWire>,
}
#[derive(serde::Deserialize)]
struct WalletsSinceResp {
    rows: Vec<WalletLink>,
    #[allow(dead_code)]
    next_cursor: Option<SinceCursorWire>,
}
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct SinceCursorWire {
    ts: chrono::DateTime<chrono::Utc>,
    pk: String,
}

/// Spawn the loop. Returns immediately if marketplace coordination is
/// unconfigured (MARKETPLACE_SVC_URL empty).
pub async fn run(state: AppState) {
    let Some(svc_url) = state.marketplace_svc_url.clone() else {
        info!("self-heal task disabled: MARKETPLACE_SVC_URL unset");
        return;
    };
    let Some(secret) = state.marketplace_s2s_secret.clone() else {
        warn!("self-heal task disabled: secret unset (boot validation should have caught this)");
        return;
    };

    loop {
        if let Err(e) = tick_ownership(&state, &svc_url, &secret).await {
            warn!(error = %e, "self-heal ownership tick failed");
        }
        if let Err(e) = tick_wallets(&state, &svc_url, &secret).await {
            warn!(error = %e, "self-heal wallets tick failed");
        }
        tokio::time::sleep(Duration::from_secs(TICK_SECS)).await;
    }
}

async fn tick_ownership(state: &AppState, svc_url: &str, secret: &str) -> anyhow::Result<()> {
    let cursor = SyncCursorRepo { pool: &state.pool }
        .get("ownership")
        .await?;
    let path = "/s2s/ownership/since";
    let query_raw = format!(
        "cursor_pk={}&cursor_ts={}&limit={}",
        urlencoding::encode(&cursor.cursor_pk),
        urlencoding::encode(&cursor.cursor_ts.to_rfc3339()),
        PAGE_LIMIT,
    );
    let query = canonicalize_query(&query_raw);
    let (ts, sig) =
        build_outbound_signature("GET", path, &query, b"", secret.as_bytes(), Utc::now());
    let url = format!("{}{}?{}", svc_url.trim_end_matches('/'), path, query);
    let resp = state
        .http_client
        .get(&url)
        .header("x-s2s-timestamp", ts)
        .header("x-s2s-signature", sig)
        .send()
        .await?
        .error_for_status()?
        .json::<OwnershipSinceResp>()
        .await?;

    let repo = OwnershipRepo { pool: &state.pool };
    for row in &resp.rows {
        repo.upsert(
            &row.asset_id,
            &row.persona_id,
            &row.owner_wallet,
            row.source_updated_at,
        )
        .await?;
    }

    // Advance the cursor whenever we received any rows. Don't depend on
    // svc's next_cursor being Some — svc returns next_cursor=None on the
    // catch-up page (fewer rows than limit), and we still need to remember
    // where we got to.
    if let Some(last) = resp.rows.last() {
        SyncCursorRepo { pool: &state.pool }
            .set(
                "ownership",
                &Cursor {
                    cursor_ts: last.source_updated_at,
                    cursor_pk: last.asset_id.clone(),
                },
            )
            .await?;
    }
    Ok(())
}

async fn tick_wallets(state: &AppState, svc_url: &str, secret: &str) -> anyhow::Result<()> {
    let cursor = SyncCursorRepo { pool: &state.pool }.get("wallets").await?;
    let path = "/s2s/wallets/since";
    let query_raw = format!(
        "cursor_pk={}&cursor_ts={}&limit={}",
        urlencoding::encode(&cursor.cursor_pk),
        urlencoding::encode(&cursor.cursor_ts.to_rfc3339()),
        PAGE_LIMIT,
    );
    let query = canonicalize_query(&query_raw);
    let (ts, sig) =
        build_outbound_signature("GET", path, &query, b"", secret.as_bytes(), Utc::now());
    let url = format!("{}{}?{}", svc_url.trim_end_matches('/'), path, query);
    let resp = state
        .http_client
        .get(&url)
        .header("x-s2s-timestamp", ts)
        .header("x-s2s-signature", sig)
        .send()
        .await?
        .error_for_status()?
        .json::<WalletsSinceResp>()
        .await?;

    let repo = WalletLinkRepo { pool: &state.pool };
    for row in &resp.rows {
        repo.upsert(
            row.user_id,
            &row.wallet_pubkey,
            row.linked,
            row.source_updated_at,
        )
        .await?;
    }

    // Same self-heal advancement rule as tick_ownership: key off the last
    // row we saw, not next_cursor (which is None on the catch-up page).
    if let Some(last) = resp.rows.last() {
        SyncCursorRepo { pool: &state.pool }
            .set(
                "wallets",
                &Cursor {
                    cursor_ts: last.source_updated_at,
                    cursor_pk: format!("{}:{}", last.user_id, last.wallet_pubkey),
                },
            )
            .await?;
    }
    Ok(())
}

// SPDX-License-Identifier: AGPL-3.0-only
//! HMAC-SHA256 server-to-server authentication for /s2s/* routes.
//!
//! Canonical signing string is a deterministic five-line ASCII layout:
//!     method + "\n"
//!   + path + "\n"
//!   + canonical_query + "\n"
//!   + timestamp + "\n"
//!   + body_sha256_hex
//!
//! Method + path + canonical_query bind the signature to a specific
//! request; signing the body alone would be replayable across endpoints.
//! Body is buffered up to 1 MiB before HMAC computation; oversized bodies
//! return 413 without computing the hash, blocking memory-DoS.
//!
//! Two secrets supported for rolling rotation:
//!   - MARKETPLACE_SVC_S2S_SECRET           (active, used to sign outbound)
//!   - MARKETPLACE_SVC_S2S_SECRET_PREVIOUS  (verify-only, accepted for inbound)

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::state::AppState;

pub const MAX_BODY_BYTES: usize = 1024 * 1024; // 1 MiB
pub const TIMESTAMP_SKEW_SECS: i64 = 5 * 60;

type HmacSha256 = Hmac<Sha256>;

/// Build the canonical signing string from request parts.
pub fn canonical_signing_string(
    method: &str,
    path: &str,
    canonical_query: &str,
    timestamp: &str,
    body_sha256_hex: &str,
) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}",
        method.to_ascii_uppercase(),
        path,
        canonical_query,
        timestamp,
        body_sha256_hex
    )
}

/// Canonicalize a query string: split on `&`, sort by name+value, re-join.
/// Empty input → empty output.
pub fn canonicalize_query(q: &str) -> String {
    if q.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<&str> = q.split('&').collect();
    pairs.sort();
    pairs.join("&")
}

/// Compute HMAC-SHA256 over `canonical` with `secret`, returning hex.
pub fn sign(secret: &[u8], canonical: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(canonical.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn verify_against(secret: &[u8], canonical: &str, provided_hex: &str) -> bool {
    let provided = match hex::decode(provided_hex) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(canonical.as_bytes());
    let expected = mac.finalize().into_bytes();
    expected.ct_eq(&provided).into()
}

/// Axum middleware: verifies the incoming HMAC and passes the buffered
/// body through to the handler. Mount only on /s2s/*.
pub async fn require_s2s(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Pull headers up-front so we don't borrow req across body read.
    let headers = req.headers().clone();
    let timestamp = headers
        .get("x-s2s-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_string();
    let signature = headers
        .get("x-s2s-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?
        .to_string();

    // Reject malformed or skewed timestamp before doing any work.
    let ts_parsed: DateTime<Utc> = timestamp.parse().map_err(|_| StatusCode::UNAUTHORIZED)?;
    let skew = (Utc::now() - ts_parsed).num_seconds().abs();
    if skew > TIMESTAMP_SKEW_SECS {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Buffer body with size cap.
    let method = req.method().clone();
    let uri = req.uri().clone();
    let body = std::mem::replace(req.body_mut(), Body::empty());
    let bytes: Bytes = match to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return Err(StatusCode::PAYLOAD_TOO_LARGE),
    };

    // Build the canonical signing string.
    let body_hash = Sha256::digest(&bytes);
    let body_sha256_hex = hex::encode(body_hash);
    let canonical = canonical_signing_string(
        method.as_str(),
        uri.path(),
        &canonicalize_query(uri.query().unwrap_or("")),
        &timestamp,
        &body_sha256_hex,
    );

    // Try active + previous secret. Both unset → reject.
    let mut any_secret = false;
    let mut matched = false;
    if let Some(secret) = state.marketplace_s2s_secret.as_deref() {
        any_secret = true;
        if verify_against(secret.as_bytes(), &canonical, &signature) {
            matched = true;
        }
    }
    if !matched {
        if let Some(secret) = state.marketplace_s2s_secret_previous.as_deref() {
            any_secret = true;
            if verify_against(secret.as_bytes(), &canonical, &signature) {
                matched = true;
            }
        }
    }
    if !any_secret || !matched {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // Hand the buffered body to the inner handler.
    *req.body_mut() = Body::from(bytes);
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_string_layout_is_stable() {
        let s = canonical_signing_string(
            "POST",
            "/s2s/ownership/upsert",
            "",
            "2026-05-13T08:00:00Z",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_eq!(
            s,
            "POST\n/s2s/ownership/upsert\n\n2026-05-13T08:00:00Z\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn canonicalize_query_sorts() {
        assert_eq!(
            canonicalize_query("limit=100&cursor_ts=2026-05-13T00:00:00Z&cursor_pk="),
            "cursor_pk=&cursor_ts=2026-05-13T00:00:00Z&limit=100"
        );
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let secret = b"test-secret";
        let canonical = "GET\n/s2s/wallets/since\ncursor_pk=&cursor_ts=&limit=10\n2026-05-13T00:00:00Z\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let sig = sign(secret, canonical);
        assert!(verify_against(secret, canonical, &sig));
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let canonical = "GET\n/p\n\n2026-01-01T00:00:00Z\ne3b0...";
        let sig = sign(b"k1", canonical);
        assert!(!verify_against(b"k2", canonical, &sig));
    }

    #[test]
    fn verify_rejects_non_hex_signature() {
        assert!(!verify_against(b"k", "anything", "not-hex"));
    }
}

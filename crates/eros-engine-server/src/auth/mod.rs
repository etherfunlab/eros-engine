// SPDX-License-Identifier: AGPL-3.0-only
//! Pluggable JWT validation. Default impl is Supabase HS256; self-hosters
//! can swap in any IdP by implementing AuthValidator.
//!
//! TODO(T11/T12): the items below are unused until companion routes (T11)
//! attach `middleware::require_auth` and `main` (T12) constructs the
//! validator into `AppState`. Allow-dead-code lifts after T12.
#![allow(dead_code)]

use async_trait::async_trait;
use uuid::Uuid;

pub mod middleware;
pub mod supabase;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing bearer token")]
    MissingToken,
    #[error("malformed token")]
    Malformed,
    #[error("expired token")]
    Expired,
    #[error("signature mismatch")]
    BadSignature,
    #[error("missing sub claim")]
    MissingSub,
}

#[async_trait]
pub trait AuthValidator: Send + Sync + 'static {
    async fn validate(&self, bearer: &str) -> Result<Uuid, AuthError>;
}

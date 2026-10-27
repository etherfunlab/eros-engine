// SPDX-License-Identifier: AGPL-3.0-only
use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::Response,
};
use uuid::Uuid;

use crate::byok::{constant_time_eq, pre_error};
use crate::error::AppError;
use crate::state::AppState;

/// Request header carrying the deployment's caller secret (`CALLER_SECRET`).
pub(crate) const CALLER_SECRET_HEADER: &str = "x-caller-secret";

#[derive(Clone, Copy, Debug)]
pub struct AuthUser(pub Uuid);

pub async fn require_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let bearer = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let user_id = state
        .auth
        .validate(bearer)
        .await
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    req.extensions_mut().insert(AuthUser(user_id));
    Ok(next.run(req).await)
}

/// The caller-secret gate: with `CALLER_SECRET` set, only a caller sending
/// one of its keys reaches the authenticated tree. Layered outside
/// [`require_auth`], so it decides before the JWT is looked at.
pub async fn require_caller_secret(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let keys = &state.config.caller_secret.secrets;
    if keys.is_empty() {
        return Ok(next.run(req).await);
    }
    let sent = req
        .headers()
        .get(CALLER_SECRET_HEADER)
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    let admitted = keys
        .iter()
        .fold(false, |ok, k| ok | constant_time_eq(sent, k.as_bytes()));
    if !admitted {
        return Err(pre_error(
            StatusCode::FORBIDDEN,
            "caller_forbidden",
            "caller secret missing or wrong".into(),
        ));
    }
    Ok(next.run(req).await)
}

// SPDX-License-Identifier: AGPL-3.0-only

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),
    // The auth middleware returns StatusCode::UNAUTHORIZED directly rather
    // than going through AppError, so this variant is reserved for future
    // route-level 401 use (e.g. expired-token reauth handlers).
    #[allow(dead_code)]
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    // Reserved for handler-level 500s; constructed nowhere right now (its only
    // user, the legacy event_gift route, was removed). Still mapped to a 500 via
    // the `_` arm in IntoResponse.
    #[allow(dead_code)]
    #[error("internal: {0}")]
    Internal(String),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Streaming-specific pre-stream error. Renders the spec §1.3 body
    /// schema (code / message / user_message [+ optional extras]).
    #[error("stream pre-error: {0}")]
    StreamPre(StreamPreError),
}

#[derive(Debug, Clone)]
pub struct StreamPreError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub user_message: String,
    pub original_user_message_id: Option<uuid::Uuid>,
}

impl std::fmt::Display for StreamPreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let AppError::StreamPre(e) = &self {
            let mut body = serde_json::Map::new();
            body.insert("code".into(), json!(e.code));
            body.insert("message".into(), json!(e.message));
            body.insert("user_message".into(), json!(e.user_message));
            if let Some(id) = e.original_user_message_id {
                body.insert("original_user_message_id".into(), json!(id.to_string()));
            }
            return (e.status, Json(serde_json::Value::Object(body))).into_response();
        }
        let (status, code) = match &self {
            AppError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            AppError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized"),
            AppError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            AppError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        (
            status,
            Json(json!({ "error": code, "message": self.to_string() })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stream_pre_error_renders_spec_body_with_original_message_id() {
        let id = uuid::Uuid::new_v4();
        let err = AppError::StreamPre(StreamPreError {
            status: StatusCode::CONFLICT,
            code: "duplicate_in_progress",
            message: "same client_msg_id still generating".into(),
            user_message: "请稍后再试".into(),
            original_user_message_id: Some(id),
        });
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(v["code"], "duplicate_in_progress");
        assert_eq!(v["original_user_message_id"], id.to_string());
    }
}

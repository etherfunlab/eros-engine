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
    /// A provider failure surfaced with the provider's own status. Used by the
    /// standalone compose endpoint, whose caller needs "the provider failed my
    /// call" distinguishable from "the engine broke". Scoped to that endpoint:
    /// the chat path's provider failures keep their own handling (fallback
    /// chain, pseudo-ghost) and must not be rerouted here.
    #[error("upstream failure: {0}")]
    Upstream(Box<eros_engine_llm::failure::AttemptFailure>),
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
        if let AppError::Upstream(f) = &self {
            use eros_engine_llm::failure::{
                is_retryable_status, response_status_for, AttemptFailure,
            };
            // The provider's own status passes through verbatim (`from_u16`
            // accepts 100-999); a failure with no status of its own maps by
            // kind — a gateway timeout to 504, everything else to 502.
            let raw_status = response_status_for(f);
            let status = StatusCode::from_u16(raw_status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut body = serde_json::Map::new();
            body.insert("error".into(), json!("upstream"));
            body.insert("message".into(), json!(self.to_string()));
            let retry_after = match f.as_ref() {
                AttemptFailure::Upstream(a) => {
                    body.insert("upstream_status".into(), json!(a.http_status));
                    if let Some(c) = &a.provider_code {
                        body.insert("provider_code".into(), json!(c));
                    }
                    if let Some(t) = &a.error_type {
                        body.insert("error_type".into(), json!(t));
                    }
                    a.retry_after_s
                }
                AttemptFailure::Gateway(g) => {
                    body.insert("gateway_kind".into(), json!(g.kind));
                    None
                }
            };
            // Derived from the same status, not hardcoded per-variant — the
            // gateway arm and the upstream arm agree by construction.
            body.insert("retryable".into(), json!(is_retryable_status(raw_status)));
            let mut resp = (status, Json(serde_json::Value::Object(body))).into_response();
            if let Some(secs) = retry_after {
                if let Ok(v) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                    resp.headers_mut()
                        .insert(axum::http::header::RETRY_AFTER, v);
                }
            }
            return resp;
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

    #[tokio::test]
    async fn upstream_error_passes_the_provider_status_through_verbatim() {
        use eros_engine_llm::failure::{AttemptFailure, UpstreamAttempt};
        let resp = AppError::Upstream(Box::new(AttemptFailure::Upstream(UpstreamAttempt {
            task: "chat_image_prompt_compose".into(),
            model: "some/model".into(),
            http_status: 529,
            provider_code: Some("529".into()),
            error_type: Some("overloaded".into()),
            upstream_provider_code: None,
            retry_after_s: Some(30),
            message: "code=529: Overloaded".into(),
        })))
        .into_response();

        assert_eq!(resp.status().as_u16(), 529, "not flattened to 502");
        assert_eq!(
            resp.headers().get("retry-after").unwrap(),
            "30",
            "Retry-After is forwarded verbatim"
        );
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "upstream");
        assert_eq!(v["upstream_status"], 529);
        assert_eq!(v["provider_code"], "529");
    }

    #[tokio::test]
    async fn gateway_timeout_renders_504_and_transport_renders_502() {
        use eros_engine_llm::failure::{AttemptFailure, GatewayError, GatewayKind};
        let mk = |kind| {
            AppError::Upstream(Box::new(AttemptFailure::Gateway(GatewayError {
                task: "chat_image_prompt_compose".into(),
                model: Some("m".into()),
                kind,
                message: "x".into(),
            })))
            .into_response()
        };
        assert_eq!(mk(GatewayKind::TotalTimeout).status().as_u16(), 504);
        assert_eq!(mk(GatewayKind::Transport).status().as_u16(), 502);
    }
}

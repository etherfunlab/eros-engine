// SPDX-License-Identifier: AGPL-3.0-only
//! HTTP-layer middleware.
//!
//! Currently:
//!   - `SseHeadersLayer` — injects `X-Accel-Buffering: no` +
//!     `Cache-Control: no-cache, no-transform, private` on responses whose
//!     `Content-Type` starts with `text/event-stream`. Required by spec §1.1
//!     so CDNs / reverse-proxies do not buffer the SSE body.

use axum::http::{HeaderValue, Request, Response};
use std::task::{Context, Poll};
use tower::{Layer, Service};

#[derive(Clone, Default)]
pub struct SseHeadersLayer;

impl<S> Layer<S> for SseHeadersLayer {
    type Service = SseHeadersService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        SseHeadersService { inner }
    }
}

#[derive(Clone)]
pub struct SseHeadersService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for SseHeadersService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let fut = self.inner.call(req);
        Box::pin(async move {
            let mut resp = fut.await?;
            let is_sse = resp
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.starts_with("text/event-stream"))
                .unwrap_or(false);
            if is_sse {
                resp.headers_mut()
                    .insert("X-Accel-Buffering", HeaderValue::from_static("no"));
                resp.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    HeaderValue::from_static("no-cache, no-transform, private"),
                );
            }
            Ok(resp)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    async fn sse_handler() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
            .body(Body::empty())
            .unwrap()
    }

    async fn json_handler() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn injects_headers_for_sse_response() {
        let app = Router::new()
            .route("/sse", get(sse_handler))
            .layer(SseHeadersLayer);
        let resp = app
            .oneshot(Request::builder().uri("/sse").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.headers().get("X-Accel-Buffering").unwrap(), "no");
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CACHE_CONTROL)
                .unwrap(),
            "no-cache, no-transform, private"
        );
    }

    #[tokio::test]
    async fn leaves_non_sse_response_alone() {
        let app = Router::new()
            .route("/json", get(json_handler))
            .layer(SseHeadersLayer);
        let resp = app
            .oneshot(Request::builder().uri("/json").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(resp.headers().get("X-Accel-Buffering").is_none());
    }
}

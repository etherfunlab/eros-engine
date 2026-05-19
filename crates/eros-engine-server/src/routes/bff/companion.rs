// SPDX-License-Identifier: AGPL-3.0-only
//! BFF mirror of `/comp/*` (`/bff/v1/comp/*`).
//!
//! See `docs/superpowers/specs/2026-05-20-history-latency-cuts-design.md`
//! §0.1 (convention), §2 (history endpoint), §3 (start endpoint).

use axum::{
    extract::{Extension, Path, Query, State},
    Json,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::chat::ChatRepo;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::routes::companion::{require_session_for_user, HistoryQuery};
use crate::state::AppState;

// ─── DTOs ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffHistoryEntry {
    /// "user" | "assistant" | "gift_user" | "system_error"
    pub role: String,
    pub content: String,
    pub sent_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffHistoryResponse {
    pub session_id: Uuid,
    pub messages: Vec<BffHistoryEntry>,
    /// Count of `messages` in this response (== `messages.len()`). NOT the
    /// total row count for the session — pagination doesn't know how many
    /// rows remain. Mirrors `HistoryResponse::total`.
    pub total: usize,
}

// ─── Handler ────────────────────────────────────────────────────────

/// Slim chat history for the chat-screen mount. Same auth, same ownership
/// check, same `limit ∈ [1, 50]` clamp as `/comp/.../history`. **Intentional
/// divergence:** default `limit=50` (canonical defaults to 20). Reason:
/// BFF exists for cold-mount where the FE wants a full backscroll in one
/// round-trip.
#[utoipa::path(
    get,
    path = "/bff/v1/comp/chat/{session_id}/history",
    tag = "bff-companion",
    params(
        ("session_id" = Uuid, Path, description = "Chat session id"),
        ("limit" = Option<i64>, Query, description = "Max rows (default 50, capped at 50)"),
        ("offset" = Option<i64>, Query, description = "Page offset, default 0")
    ),
    responses(
        (status = 200, body = BffHistoryResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session not found")
    ),
    security(("bearer" = []))
)]
async fn bff_get_history(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<BffHistoryResponse>, AppError> {
    require_session_for_user(&state, session_id, user_id).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 50);
    let offset = query.offset.unwrap_or(0).max(0);

    let rows = ChatRepo { pool: &state.pool }
        .history_slim(session_id, limit, offset)
        .await?;

    let messages: Vec<BffHistoryEntry> = rows
        .into_iter()
        .map(|r| BffHistoryEntry {
            role: r.role,
            content: r.content,
            sent_at: r.sent_at,
        })
        .collect();
    let total = messages.len();

    Ok(Json(BffHistoryResponse {
        session_id,
        messages,
        total,
    }))
}

// ─── Router ─────────────────────────────────────────────────────────

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(bff_get_history))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
    };
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        build_router, mint_test_jwt, seed_genome, seed_instance, seed_session, send_request,
    };

    fn bff_history_request(token: &str, session_id: Uuid, query: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(format!("/bff/v1/comp/chat/{session_id}/history{query}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_history_returns_slim_messages_in_order(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;

        // Seed three rows directly so we don't depend on pipeline::run.
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, extracted_facts) \
             VALUES ($1, 'user', 'alpha', '{\"facts\":[\"x\"]}'::jsonb),
                    ($1, 'assistant', 'beta', NULL),
                    ($1, 'user', 'gamma', '{\"facts\":[\"y\"]}'::jsonb)",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, body) =
            send_request(&mut app, bff_history_request(&token, session_id, "")).await;
        assert_eq!(status, StatusCode::OK, "got body: {body}");

        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "alpha");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["content"], "gamma");

        // No extracted_facts on any row — pure projection.
        for m in messages {
            assert!(
                m.get("extracted_facts").is_none(),
                "BFF slim DTO must not expose extracted_facts; got {m}"
            );
        }

        // `total` reflects page count, not grand total.
        assert_eq!(body["total"], 3);
        assert_eq!(body["session_id"], json!(session_id.to_string()));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_history_default_limit_is_50(pool: PgPool) {
        // Intentional divergence from canonical /comp/.../history which
        // defaults to 20. Spec §2.2.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;

        // Insert 60 rows; BFF default should return 50.
        for n in 0..60 {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content) \
                 VALUES ($1, 'user', $2)",
            )
            .bind(session_id)
            .bind(format!("m{n}"))
            .execute(&pool)
            .await
            .unwrap();
        }

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, body) =
            send_request(&mut app, bff_history_request(&token, session_id, "")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["messages"].as_array().unwrap().len(), 50);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_history_clamps_limit_to_50(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        for n in 0..60 {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content) \
                         VALUES ($1, 'user', $2)",
            )
            .bind(session_id)
            .bind(format!("m{n}"))
            .execute(&pool)
            .await
            .unwrap();
        }

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, body) = send_request(
            &mut app,
            bff_history_request(&token, session_id, "?limit=999"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["messages"].as_array().unwrap().len(), 50);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_history_401_without_bearer(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;

        let state = test_state(pool);
        let mut app = build_router(state);

        let req = Request::builder()
            .uri(format!("/bff/v1/comp/chat/{session_id}/history"))
            .body(Body::empty())
            .unwrap();
        let (status, _body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_history_403_on_other_users_session(pool: PgPool) {
        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(intruder);

        let (status, _body) =
            send_request(&mut app, bff_history_request(&token, session_id, "")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_history_404_on_missing_session(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let missing = Uuid::new_v4();
        let (status, _body) =
            send_request(&mut app, bff_history_request(&token, missing, "")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

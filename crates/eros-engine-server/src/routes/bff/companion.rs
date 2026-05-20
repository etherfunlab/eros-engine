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
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::chat::ChatRepo;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::routes::companion::resolve_or_create_session;
use crate::routes::companion::{require_session_for_user, HistoryQuery, StartChatRequest};
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

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct BffStartRequest {
    pub instance_id: Option<Uuid>,
    pub genome_id: Option<Uuid>,
    #[serde(default)]
    pub is_demo: Option<bool>,
    /// History page size for the bundled history. Default 50, capped at 50.
    /// BFF-only field; not present in the canonical /comp/chat/start body.
    #[serde(default)]
    pub history_limit: Option<i64>,
}

impl From<&BffStartRequest> for StartChatRequest {
    fn from(b: &BffStartRequest) -> Self {
        StartChatRequest {
            instance_id: b.instance_id,
            genome_id: b.genome_id,
            is_demo: b.is_demo,
        }
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffStartResponse {
    pub session_id: Uuid,
    pub instance_id: Uuid,
    pub persona_name: String,
    pub is_new: bool,
    /// Most-recent N messages, oldest-first. Empty for brand-new sessions.
    pub history: Vec<BffHistoryEntry>,
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

/// Cold-mount bundle: resolves (or creates) the session and returns its slim
/// history in one round-trip (collapses the FE's start + history calls).
///
/// Affinity is intentionally NOT bundled here: the FE reads affinity on its
/// own (full values via its DB middleware; per-turn deltas via
/// `/bff/v1/comp/affinity/{sid}/event`). This keeps bootstrap independent of
/// `EXPOSE_AFFINITY_DEBUG` — turning that gate off does not change this shape.
#[utoipa::path(
    post,
    path = "/bff/v1/comp/chat/start",
    tag = "bff-companion",
    request_body = BffStartRequest,
    responses(
        (status = 200, body = BffStartResponse),
        (status = 400, description = "missing genome_id and no existing instance"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your instance / nft gate"),
        (status = 404, description = "instance/genome not found")
    ),
    security(("bearer" = []))
)]
async fn bff_start_chat(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<BffStartRequest>,
) -> Result<Json<BffStartResponse>, AppError> {
    let canonical_req = StartChatRequest::from(&req);
    let resolved = resolve_or_create_session(&state, user_id, &canonical_req).await?;
    let history_limit = req.history_limit.unwrap_or(50).clamp(1, 50);

    let rows = ChatRepo { pool: &state.pool }
        .history_slim(resolved.session_id, history_limit, 0)
        .await?;

    let history = rows
        .into_iter()
        .map(|r| BffHistoryEntry {
            role: r.role,
            content: r.content,
            sent_at: r.sent_at,
        })
        .collect();

    Ok(Json(BffStartResponse {
        session_id: resolved.session_id,
        instance_id: resolved.instance_id,
        persona_name: resolved.persona_name,
        is_new: resolved.is_new,
        history,
    }))
}

// ─── Router ─────────────────────────────────────────────────────────

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(bff_get_history))
        .routes(routes!(bff_start_chat))
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
        // Explicit, strictly-increasing sent_at: a single multi-row INSERT
        // shares one now() across all rows, so ORDER BY sent_at would tie and
        // the result order would be undefined.
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, extracted_facts, sent_at) \
             VALUES ($1, 'user', 'alpha', '{\"facts\":[\"x\"]}'::jsonb, now() - interval '2 seconds'),
                    ($1, 'assistant', 'beta', NULL, now() - interval '1 second'),
                    ($1, 'user', 'gamma', '{\"facts\":[\"y\"]}'::jsonb, now())",
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

    // ─── Plan C: POST /bff/v1/comp/chat/start tests ─────────────────

    fn bff_start_request(token: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/bff/v1/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_start_brand_new_session_returns_empty_history(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;

        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, body) = send_request(
            &mut app,
            bff_start_request(&token, json!({ "genome_id": genome_id })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={body}");

        assert!(body["is_new"].as_bool().unwrap());
        assert_eq!(body["persona_name"], "Aria");
        assert!(body["session_id"].is_string());
        assert!(body["instance_id"].is_string());
        assert!(body["history"].as_array().unwrap().is_empty());
        // Bootstrap no longer bundles affinity — the field must be absent.
        assert!(body.get("affinity").is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_start_resumed_session_returns_history(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        // Explicit, strictly-increasing sent_at so the two rows order
        // deterministically — a single multi-row INSERT shares one now().
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             VALUES ($1, 'user', 'hello', now() - interval '1 second'), \
                    ($1, 'assistant', 'hi back', now())",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, body) = send_request(
            &mut app,
            bff_start_request(&token, json!({ "genome_id": genome_id })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        assert!(!body["is_new"].as_bool().unwrap());
        assert_eq!(body["session_id"], json!(session_id.to_string()));
        let history = body["history"].as_array().expect("history array");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0]["role"], "user");
        assert_eq!(history[0]["content"], "hello");
        assert_eq!(history[1]["role"], "assistant");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_start_history_limit_clamped_to_50(pool: PgPool) {
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
            bff_start_request(
                &token,
                json!({ "genome_id": genome_id, "history_limit": 999 }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["history"].as_array().unwrap().len(), 50);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_start_does_not_bundle_affinity_even_with_debug_open(pool: PgPool) {
        // Bootstrap is decoupled from affinity: even with a pre-seeded affinity
        // row AND EXPOSE_AFFINITY_DEBUG on (test default), the start response
        // carries no `affinity` field. The FE reads affinity separately.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        sqlx::query(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id, warmth) \
             VALUES ($1, $2, $3, 0.42)",
        )
        .bind(session_id)
        .bind(user_id)
        .bind(instance_id)
        .execute(&pool)
        .await
        .unwrap();

        let state = test_state(pool); // expose_affinity_debug=true by default in tests
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, body) = send_request(
            &mut app,
            bff_start_request(&token, json!({ "genome_id": genome_id })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.get("affinity").is_none(),
            "start must not bundle affinity; got {body}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_start_403_on_nft_unowned_genome(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata, is_active, asset_id) \
             VALUES ('Locked', 'sys', '{}'::jsonb, true, 'asset-x') RETURNING id",
        )
        .fetch_one(&pool).await.unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, _body) = send_request(
            &mut app,
            bff_start_request(&token, json!({ "genome_id": genome_id })),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_start_matches_canonical_start_session_id(pool: PgPool) {
        // Same input on both endpoints should resolve to the same session for the
        // same JWT user. Confirms resolve_or_create_session is the only mover.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        // First: canonical
        let req = Request::builder()
            .method("POST")
            .uri("/comp/chat/start")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({ "genome_id": genome_id })).unwrap(),
            ))
            .unwrap();
        let (status, canon) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK);
        let canonical_session_id = canon["session_id"].as_str().unwrap().to_string();

        // Then: BFF on the same input — must resume the same session.
        let (status, bff) = send_request(
            &mut app,
            bff_start_request(&token, json!({ "genome_id": genome_id })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bff["session_id"].as_str().unwrap(), canonical_session_id);
        assert!(!bff["is_new"].as_bool().unwrap()); // canonical already created it
    }
}

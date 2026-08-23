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
use crate::routes::companion::{
    is_false, require_session_for_user, HistoryQuery, StartChatRequest,
};
use crate::state::AppState;

// ─── DTOs ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BffHistoryEntry {
    /// Stable message id — the `engine.chat_messages` row primary key (UUID).
    pub id: Uuid,
    /// Client-supplied message id forwarded during streaming (the
    /// `client_msg_id` the FE sent on send). NULL for rows that never carried
    /// one, e.g. assistant turns. Lets the FE reconcile an optimistic local
    /// message with its persisted row.
    pub client_msg_id: Option<String>,
    /// "user" | "assistant" | "gift_user" | "system_error"
    pub role: String,
    pub content: String,
    pub sent_at: DateTime<Utc>,
    /// Structured tip amount for `role='gift_user'` rows. Omitted from
    /// response when None (`skip_serializing_if`). Lets clients render tips
    /// at the right point in the timeline without parsing the `(打赏 $X)`
    /// content marker. Spec:
    /// docs/superpowers/specs/2026-05-26-tip-role-and-filter-audit-design.md §3.4.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tips_amount_usd: Option<f64>,
    /// When this message reached the party it was addressed to — set by
    /// `POST /comp/chat/{session_id}/read` on an `assistant` row, and by the
    /// engine itself on a `user` row (the moment the turn handed the message to
    /// its first model; delivery, not that model's acknowledgement). Omitted
    /// while unread; permanently omitted on tips and voice `user` rows, which
    /// have no reader. Absence means "no receipt", not "not yet read".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_at: Option<DateTime<Utc>>,
    /// Conversation-flavor marker: `"product_qa"` = out-of-character product
    /// answer (excluded from companion context). Omitted for normal turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Present (always `true`) only on assistant rows whose turn delegated an
    /// image to the consumer (`metadata.image` marker recorded) — key-presence
    /// contract like `read_at`. Recover the payload via
    /// `GET /comp/chat/{session_id}/messages/{message_id}/image-request`; a 404
    /// there on a flagged row means the composed prompt was never recorded
    /// (fail-open audit) — genuinely unrecoverable. Spec:
    /// docs/superpowers/specs/2026-08-21-image-turn-discovery-design.md.
    #[serde(skip_serializing_if = "is_false")]
    pub image: bool,
    /// The message this `user` row quoted — the `reply_to_message_id` the
    /// caller sent, already validated to belong to this session, so it always
    /// names another row in the same history page or an older one. Omitted on
    /// ordinary turns and on turns whose anchor failed to resolve (there is no
    /// bubble to point at). Lets a cold mount re-render the quote instead of
    /// losing it on refresh.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<Uuid>,
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
    /// Conversation channel ('text' default, or 'voice'). Passed through to
    /// the canonical start; see `StartChatRequest::channel`.
    #[serde(default)]
    pub channel: Option<String>,
    /// Skip resume and always create a fresh session. Passed through to the
    /// canonical start; see `StartChatRequest::force_new`.
    #[serde(default)]
    pub force_new: Option<bool>,
}

impl From<&BffStartRequest> for StartChatRequest {
    fn from(b: &BffStartRequest) -> Self {
        StartChatRequest {
            instance_id: b.instance_id,
            genome_id: b.genome_id,
            is_demo: b.is_demo,
            channel: b.channel.clone(),
            force_new: b.force_new,
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
            id: r.id,
            client_msg_id: r.client_msg_id,
            role: r.role,
            content: r.content,
            sent_at: r.sent_at,
            tips_amount_usd: r.tips_amount_usd,
            channel: r.channel,
            read_at: r.read_at,
            image: r.image,
            reply_to_message_id: r.reply_to_message_id,
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
/// Affinity is intentionally NOT bundled here: a client reads it from the two
/// dedicated routes (`/bff/v1/comp/affinity/{sid}` for the absolute value,
/// `.../event` for the per-turn delta), so a cold mount that does not need a
/// relationship pays nothing for one.
#[utoipa::path(
    post,
    path = "/bff/v1/comp/chat/start",
    tag = "bff-companion",
    request_body = BffStartRequest,
    responses(
        (status = 200, body = BffStartResponse),
        (status = 400, description = "missing genome_id and no existing instance"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your instance"),
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

    // Brand-new sessions have no messages — skip the history round-trip.
    let history = if resolved.is_new {
        Vec::new()
    } else {
        let history_limit = req.history_limit.unwrap_or(50).clamp(1, 50);
        let rows = ChatRepo { pool: &state.pool }
            .history_slim(resolved.session_id, history_limit, 0)
            .await?;
        rows.into_iter()
            .map(|r| BffHistoryEntry {
                id: r.id,
                client_msg_id: r.client_msg_id,
                role: r.role,
                content: r.content,
                sent_at: r.sent_at,
                tips_amount_usd: r.tips_amount_usd,
                channel: r.channel,
                read_at: r.read_at,
                image: r.image,
                reply_to_message_id: r.reply_to_message_id,
            })
            .collect()
    };

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
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             VALUES ($1, 'user', 'alpha', now() - interval '2 seconds'),
                    ($1, 'assistant', 'beta', now() - interval '1 second'),
                    ($1, 'user', 'gamma', now())",
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
    async fn bff_history_entries_carry_row_id_and_client_msg_id(pool: PgPool) {
        // The slim history must expose the engine's chat_messages primary key
        // (`id`, a UUID) plus the stream-time `client_msg_id` (nullable), so the
        // FE can reconcile persisted rows against both the DB key and its own
        // optimistic message ids.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;

        let id_with = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_messages (session_id, role, content, client_msg_id, sent_at) \
             VALUES ($1, 'user', 'alpha', 'c_alpha', now() - interval '1 second') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let id_without = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             VALUES ($1, 'assistant', 'beta', now()) RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, body) =
            send_request(&mut app, bff_history_request(&token, session_id, "")).await;
        assert_eq!(status, StatusCode::OK, "got body: {body}");

        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        // Oldest-first: the user row carrying a client_msg_id.
        assert_eq!(
            messages[0]["id"].as_str(),
            Some(id_with.to_string().as_str()),
            "id must be the raw chat_messages UUID; got {body}"
        );
        assert_eq!(messages[0]["client_msg_id"].as_str(), Some("c_alpha"));
        // Assistant row had no client_msg_id → serialized as null.
        assert_eq!(
            messages[1]["id"].as_str(),
            Some(id_without.to_string().as_str())
        );
        assert!(
            messages[1]["client_msg_id"].is_null(),
            "absent client_msg_id must be null; got {body}"
        );
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
    async fn bff_history_flags_image_turns(pool: PgPool) {
        // `image: true` is present only on rows carrying the `metadata.image`
        // marker — the discovery half of the image-request recovery endpoint
        // (spec 2026-08-21-image-turn-discovery-design.md). Every other row
        // omits the key rather than sending false; a NULL metadata column
        // must decode as omitted, not error.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        sqlx::query(
            "INSERT INTO engine.chat_messages \
                 (session_id, role, content, assistant_action_type, metadata, sent_at) \
             VALUES ($1, 'user', 'draw yourself', NULL, NULL, now() - interval '2 seconds'), \
                    ($1, 'assistant', 'sure', 'reply', NULL, now() - interval '1 second'), \
                    ($1, 'assistant', '', 'reply', \
                     '{\"image\": {\"prompt\": \"dusk\", \"image_ref\": \"previous\"}}', now())",
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
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert!(
            messages[0].get("image").is_none(),
            "user row omits the key rather than sending false: {}",
            messages[0]
        );
        assert!(
            messages[1].get("image").is_none(),
            "plain assistant row omits the key: {}",
            messages[1]
        );
        assert_eq!(messages[2]["image"], json!(true), "image turn is flagged");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_start_bundle_flags_image_turns(pool: PgPool) {
        // The cold-mount start bundle serializes history through the same DTO
        // and must inherit the flag — it is the path a cold mount actually
        // uses (spec 2026-08-21-image-turn-discovery-design.md §3).
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        sqlx::query(
            "INSERT INTO engine.chat_messages \
                 (session_id, role, content, assistant_action_type, metadata, sent_at) \
             VALUES ($1, 'assistant', 'sure', 'reply', NULL, now() - interval '1 second'), \
                    ($1, 'assistant', '', 'reply', \
                     '{\"image\": {\"prompt\": \"dusk\", \"image_ref\": \"previous\"}}', now())",
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
        assert_eq!(status, StatusCode::OK, "got body: {body}");
        assert_eq!(body["session_id"], json!(session_id.to_string()));
        let history = body["history"].as_array().expect("history array");
        assert_eq!(history.len(), 2);
        assert!(
            history[0].get("image").is_none(),
            "plain assistant row omits the key: {}",
            history[0]
        );
        assert_eq!(history[1]["image"], json!(true), "image turn is flagged");
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
    async fn bff_start_does_not_bundle_affinity(pool: PgPool) {
        // Bootstrap is decoupled from affinity: even with a pre-seeded affinity
        // row present, the start response
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

        let state = test_state(pool);
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
    async fn bff_history_exposes_tips_amount_usd_on_gift_user_rows(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;

        sqlx::query(
            "INSERT INTO engine.chat_messages \
                 (session_id, role, content, metadata, sent_at) \
             VALUES ($1, 'gift_user', '(打赏 $20)', \
                     '{\"tips_amount_usd\": 20.0, \"tier\": \"gold\", \"prompt_traits\": [\"nsfw\"]}'::jsonb, \
                     now() - interval '2 seconds'),
                    ($1, 'user', 'hello', '{\"tier\": \"silver\"}'::jsonb, now() - interval '1 second')",
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
        assert_eq!(messages.len(), 2);

        let tip = messages
            .iter()
            .find(|m| m["role"] == "gift_user")
            .expect("tip row");
        assert_eq!(tip["tips_amount_usd"], json!(20.0));

        let normal = messages
            .iter()
            .find(|m| m["role"] == "user")
            .expect("user row");
        assert!(
            normal.get("tips_amount_usd").is_none(),
            "non-tip user row must omit tips_amount_usd; got {normal}"
        );

        // BFF MUST NOT leak any metadata key other than the typed tips_amount_usd
        // extract. Spec §3.4: tier / prompt_traits are audit-only on the DB side.
        for m in messages {
            assert!(
                m.get("metadata").is_none(),
                "BFF must never expose raw metadata; got {m}"
            );
            assert!(
                m.get("tier").is_none(),
                "BFF must not surface tier from metadata; got {m}"
            );
            assert!(
                m.get("prompt_traits").is_none(),
                "BFF must not surface prompt_traits from metadata; got {m}"
            );
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_history_exposes_reply_to_message_id_on_quoted_rows(pool: PgPool) {
        // The anchor is written by the send path onto the user row's
        // `metadata.reply_to_message_id`; history must hand it back so a cold
        // mount can re-render the quote. A turn whose anchor failed to resolve
        // carries `reply_to_error` instead — audit-only, never surfaced.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;

        let anchor: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             VALUES ($1, 'user', 'the earlier plan', now() - interval '3 seconds') \
             RETURNING id",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO engine.chat_messages \
                 (session_id, role, content, metadata, sent_at) \
             VALUES ($1, 'user', 'wait, about that', \
                     jsonb_build_object('reply_to_message_id', $2::text), \
                     now() - interval '2 seconds'),
                    ($1, 'user', 'stale quote', \
                     '{\"reply_to_error\": \"not_found\"}'::jsonb, \
                     now() - interval '1 second')",
        )
        .bind(session_id)
        .bind(anchor)
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

        let quoting = messages
            .iter()
            .find(|m| m["content"] == "wait, about that")
            .expect("quoting row");
        assert_eq!(
            quoting["reply_to_message_id"],
            json!(anchor.to_string()),
            "the quoting row must carry its anchor; got {quoting}"
        );

        let anchored = messages
            .iter()
            .find(|m| m["content"] == "the earlier plan")
            .expect("anchor row");
        assert!(
            anchored.get("reply_to_message_id").is_none(),
            "an ordinary row must omit reply_to_message_id; got {anchored}"
        );

        let failed = messages
            .iter()
            .find(|m| m["content"] == "stale quote")
            .expect("failed-anchor row");
        assert!(
            failed.get("reply_to_message_id").is_none(),
            "an unresolved anchor has no bubble to point at; got {failed}"
        );
        assert!(
            failed.get("reply_to_error").is_none(),
            "reply_to_error stays audit-only; got {failed}"
        );
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

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_start_voice_channel_is_isolated_from_text(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        // 1. Text start creates the text session.
        let (status, text1) = send_request(
            &mut app,
            bff_start_request(&token, json!({ "genome_id": genome_id })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={text1}");
        assert!(text1["is_new"].as_bool().unwrap());

        // 2. Voice start must NOT resume it — it creates a separate session.
        let (status, voice1) = send_request(
            &mut app,
            bff_start_request(
                &token,
                json!({ "genome_id": genome_id, "channel": "voice" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={voice1}");
        assert!(voice1["is_new"].as_bool().unwrap());
        assert_ne!(voice1["session_id"], text1["session_id"]);

        // 3. A second voice start resumes the SAME voice session.
        let (_status, voice2) = send_request(
            &mut app,
            bff_start_request(
                &token,
                json!({ "genome_id": genome_id, "channel": "voice" }),
            ),
        )
        .await;
        assert!(!voice2["is_new"].as_bool().unwrap());
        assert_eq!(voice2["session_id"], voice1["session_id"]);

        // 4. Text start still resumes the TEXT session, even though the voice
        //    session is more recent.
        let (_status, text2) = send_request(
            &mut app,
            bff_start_request(&token, json!({ "genome_id": genome_id })),
        )
        .await;
        assert!(!text2["is_new"].as_bool().unwrap());
        assert_eq!(text2["session_id"], text1["session_id"]);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_start_force_new_creates_fresh_session(pool: PgPool) {
        // Proves `force_new` survives the wire + the literal
        // `From<&BffStartRequest> for StartChatRequest` impl — with
        // `#[serde(default)]` on the field, a forgotten copy in that impl
        // would silently drop it instead of failing loudly.
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, first) = send_request(
            &mut app,
            bff_start_request(&token, json!({ "genome_id": genome_id })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={first}");
        assert!(first["is_new"].as_bool().unwrap());
        let first_id = first["session_id"].as_str().unwrap().to_string();

        let (status, second) = send_request(
            &mut app,
            bff_start_request(&token, json!({ "genome_id": genome_id, "force_new": true })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={second}");
        assert!(
            second["is_new"].as_bool().unwrap(),
            "force_new must produce is_new: true; got {second}"
        );
        assert_ne!(second["session_id"].as_str().unwrap(), first_id);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_start_rejects_invalid_channel(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let state = test_state(pool.clone());
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, _body) = send_request(
            &mut app,
            bff_start_request(&token, json!({ "genome_id": genome_id, "channel": "sms" })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bff_history_carries_read_at_only_on_stamped_rows(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;

        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at, read_at) \
             VALUES ($1, 'user', 'hello', now() - interval '2 seconds', NULL), \
                    ($1, 'assistant', 'hey', now() - interval '1 second', now())",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut app = build_router(test_state(pool));
        let token = mint_test_jwt(user_id);
        let (status, body) =
            send_request(&mut app, bff_history_request(&token, session_id, "")).await;
        assert_eq!(status, StatusCode::OK, "got body: {body}");

        let messages = body["messages"].as_array().expect("messages array");
        let stamped = messages.iter().find(|m| m["role"] == "assistant").unwrap();
        assert!(
            stamped["read_at"].is_string(),
            "a stamped row carries read_at; got {stamped}"
        );
        let unread = messages.iter().find(|m| m["role"] == "user").unwrap();
        assert!(
            unread.get("read_at").is_none(),
            "an unread row omits the key rather than sending null; got {unread}"
        );
    }
}

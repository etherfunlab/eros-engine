// SPDX-License-Identifier: AGPL-3.0-only
//! Message reactions (spec 2026-10-05-message-reactions-design.md §5): set,
//! clear, and the deployment's allowed range.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::chat::ChatRepo;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::reaction::ReactionAllowlist;
use crate::routes::companion::require_session_for_user;
use crate::state::AppState;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ReactionRequest {
    /// Exactly one standard emoji; skin-tone variants included.
    pub emoji: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ReactionResponse {
    pub message_id: Uuid,
    /// The emoji as stored: fully qualified.
    pub emoji: String,
    pub reacted_at: DateTime<Utc>,
}

/// An assistant row's reaction as the history routes return it.
#[allow(dead_code)] // consumed by the history routes in the next task
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ReactionView {
    pub emoji: String,
    pub reacted_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ReactionAllowlistResponse {
    /// `null`: every standard emoji. Otherwise the allowed base emoji, in the
    /// deployment's order; every skin tone of a listed emoji is allowed.
    pub allowed: Option<Vec<String>>,
}

/// Ownership first, then the message, then the body — a caller never learns
/// whether an emoji is valid on a session it does not own.
pub(crate) async fn set_reaction_for(
    state: &AppState,
    session_id: Uuid,
    message_id: Uuid,
    user_id: Uuid,
    emoji: &str,
) -> Result<ReactionResponse, AppError> {
    let repo = assistant_row(state, session_id, message_id, user_id).await?;
    let emoji = crate::reaction::validate(emoji, state.config.reaction_emoji.as_ref())
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let (emoji, reacted_at) = repo
        .set_reaction(message_id, emoji)
        .await?
        .ok_or_else(|| AppError::Conflict("not an assistant message".into()))?;
    Ok(ReactionResponse {
        message_id,
        emoji,
        reacted_at,
    })
}

pub(crate) async fn clear_reaction_for(
    state: &AppState,
    session_id: Uuid,
    message_id: Uuid,
    user_id: Uuid,
) -> Result<(), AppError> {
    assistant_row(state, session_id, message_id, user_id)
        .await?
        .clear_reaction(message_id)
        .await?;
    Ok(())
}

/// The shared checks: the session is the caller's, the message is in it, and
/// it is an assistant row.
async fn assistant_row<'a>(
    state: &'a AppState,
    session_id: Uuid,
    message_id: Uuid,
    user_id: Uuid,
) -> Result<ChatRepo<'a>, AppError> {
    require_session_for_user(state, session_id, user_id).await?;
    let repo = ChatRepo { pool: &state.pool };
    let row = repo
        .message_by_id_in_session(session_id, message_id)
        .await?
        .ok_or_else(|| AppError::NotFound("no such message".into()))?;
    if row.role != "assistant" {
        return Err(AppError::Conflict("not an assistant message".into()));
    }
    Ok(repo)
}

pub(crate) fn allowlist_response(allow: Option<&ReactionAllowlist>) -> ReactionAllowlistResponse {
    ReactionAllowlistResponse {
        allowed: allow.map(|a| a.listing().iter().map(|s| s.to_string()).collect()),
    }
}

#[utoipa::path(
    put,
    path = "/v2/comp/session/{session_id}/message/{message_id}/reaction",
    tag = "companion",
    params(
        ("session_id" = Uuid, Path, description = "Chat session id"),
        ("message_id" = Uuid, Path, description = "An assistant message in that session")
    ),
    request_body = ReactionRequest,
    responses(
        (status = 200, body = ReactionResponse),
        (status = 400, description = "not exactly one standard emoji, or not allowed on this deployment"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session or message not found"),
        (status = 409, description = "not an assistant message")
    ),
    security(("bearer" = []))
)]
async fn put_reaction(
    State(state): State<AppState>,
    Path((session_id, message_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<ReactionRequest>,
) -> Result<Json<ReactionResponse>, AppError> {
    set_reaction_for(&state, session_id, message_id, user_id, &req.emoji)
        .await
        .map(Json)
}

#[utoipa::path(
    delete,
    path = "/v2/comp/session/{session_id}/message/{message_id}/reaction",
    tag = "companion",
    params(
        ("session_id" = Uuid, Path, description = "Chat session id"),
        ("message_id" = Uuid, Path, description = "An assistant message in that session")
    ),
    responses(
        (status = 204, description = "reaction cleared (or there was none)"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session or message not found"),
        (status = 409, description = "not an assistant message")
    ),
    security(("bearer" = []))
)]
async fn delete_reaction(
    State(state): State<AppState>,
    Path((session_id, message_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<StatusCode, AppError> {
    clear_reaction_for(&state, session_id, message_id, user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/v2/comp/reactions",
    tag = "companion",
    responses(
        (status = 200, body = ReactionAllowlistResponse),
        (status = 401, description = "missing or invalid bearer")
    ),
    security(("bearer" = []))
)]
async fn get_reaction_allowlist(State(state): State<AppState>) -> Json<ReactionAllowlistResponse> {
    Json(allowlist_response(state.config.reaction_emoji.as_ref()))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(put_reaction, delete_reaction))
        .routes(routes!(get_reaction_allowlist))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{seed_genome, seed_instance, seed_session};
    use sqlx::PgPool;

    async fn seeded(pool: &PgPool) -> (Uuid, Uuid, Uuid, Uuid) {
        let user_id = Uuid::new_v4();
        let genome = seed_genome(pool, "Aria").await;
        let instance = seed_instance(pool, genome, user_id).await;
        let session = seed_session(pool, user_id, instance).await;
        let mut ids = Vec::new();
        for role in ["user", "assistant"] {
            ids.push(
                sqlx::query_scalar::<_, Uuid>(
                    "INSERT INTO engine.chat_messages (session_id, role, content) \
                     VALUES ($1, $2, 'x') RETURNING id",
                )
                .bind(session)
                .bind(role)
                .fetch_one(pool)
                .await
                .unwrap(),
            );
        }
        (user_id, session, ids[0], ids[1])
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn set_replace_and_clear(pool: PgPool) {
        let (user, session, _u, a) = seeded(&pool).await;
        let state = test_state(pool.clone());
        let r = set_reaction_for(&state, session, a, user, "❤")
            .await
            .unwrap();
        assert_eq!((r.message_id, r.emoji.as_str()), (a, "❤️"));
        let again = set_reaction_for(&state, session, a, user, "❤️")
            .await
            .unwrap();
        assert_eq!(again.reacted_at, r.reacted_at, "same emoji keeps the time");
        let other = set_reaction_for(&state, session, a, user, "👍🏽")
            .await
            .unwrap();
        assert_eq!(other.emoji, "👍🏽");
        clear_reaction_for(&state, session, a, user).await.unwrap();
        clear_reaction_for(&state, session, a, user).await.unwrap();
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn errors_in_order(pool: PgPool) {
        let (user, session, u, a) = seeded(&pool).await;
        let state = test_state(pool.clone());
        let stranger = Uuid::new_v4();
        assert!(matches!(
            set_reaction_for(&state, session, a, stranger, "👍").await,
            Err(AppError::Forbidden(_))
        ));
        assert!(matches!(
            set_reaction_for(&state, Uuid::new_v4(), a, user, "👍").await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            set_reaction_for(&state, session, Uuid::new_v4(), user, "👍").await,
            Err(AppError::NotFound(_))
        ));
        assert!(matches!(
            set_reaction_for(&state, session, u, user, "👍").await,
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            clear_reaction_for(&state, session, u, user).await,
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            set_reaction_for(&state, session, a, user, "ok").await,
            Err(AppError::BadRequest(_))
        ));
        // Ownership before body: a stranger with a bad emoji still gets 403.
        assert!(matches!(
            set_reaction_for(&state, session, a, stranger, "ok").await,
            Err(AppError::Forbidden(_))
        ));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn disallowed_emoji_is_400(pool: PgPool) {
        let (user, session, _u, a) = seeded(&pool).await;
        let mut state = test_state(pool.clone());
        state.config.reaction_emoji =
            crate::reaction::ReactionAllowlist::parse(Some("👍")).unwrap();
        assert!(matches!(
            set_reaction_for(&state, session, a, user, "🔥").await,
            Err(AppError::BadRequest(_))
        ));
        assert_eq!(
            set_reaction_for(&state, session, a, user, "👍🏿")
                .await
                .unwrap()
                .emoji,
            "👍🏿"
        );
    }

    #[test]
    fn allowlist_response_shapes() {
        assert_eq!(
            serde_json::to_value(allowlist_response(None)).unwrap(),
            serde_json::json!({"allowed": null})
        );
        let a = crate::reaction::ReactionAllowlist::parse(Some("👍🏽, ❤")).unwrap();
        assert_eq!(
            serde_json::to_value(allowlist_response(a.as_ref())).unwrap(),
            serde_json::json!({"allowed": ["👍", "❤️"]})
        );
    }
}

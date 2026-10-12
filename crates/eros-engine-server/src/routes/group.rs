// SPDX-License-Identifier: AGPL-3.0-only

//! Group chat management (spec 2026-10-12-group-chat-design.md §4.1). The send
//! route lives here too (§4.2).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::group::{normalize_title, GROUP_TITLE_MAX_CHARS, MAX_GROUP_MEMBERS};
use eros_engine_store::group::{Group, GroupRepo, MemberRefusal};

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateGroupRequest {
    #[serde(default)]
    #[schema(max_length = 64)]
    pub title: Option<String>,
    /// 1..=3 of the caller's own 1:1 text sessions, in join order.
    pub member_session_ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RenameGroupRequest {
    /// `null` clears the title.
    #[schema(max_length = 64)]
    pub title: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AddMemberRequest {
    pub member_session_id: Uuid,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GroupMemberDto {
    pub member_session_id: Uuid,
    pub instance_id: Uuid,
    pub joined_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GroupDto {
    pub group_id: Uuid,
    pub title: Option<String>,
    pub members: Vec<GroupMemberDto>,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct GroupListResponse {
    pub groups: Vec<GroupDto>,
}

pub(crate) fn group_dto(g: &Group) -> GroupDto {
    GroupDto {
        group_id: g.id,
        title: g.title.clone(),
        members: g
            .members
            .iter()
            .map(|m| GroupMemberDto {
                member_session_id: m.member_session_id,
                instance_id: m.instance_id,
                joined_at: m.joined_at,
            })
            .collect(),
        created_at: g.created_at,
        last_active_at: g.last_active_at,
    }
}

/// Every refusal on the group routes carries the stream routes' coded body:
/// `{code, message, user_message}`.
fn pre(status: StatusCode, code: &'static str, message: &str, user_message: &str) -> AppError {
    AppError::StreamPre(crate::error::StreamPreError {
        status,
        code,
        message: message.into(),
        user_message: user_message.into(),
        original_user_message_id: None,
    })
}

/// The group is unknown, dissolved or not the caller's.
fn group_not_found() -> AppError {
    pre(
        StatusCode::NOT_FOUND,
        "group_not_found",
        "group not found",
        "群聊不存在",
    )
}

fn refusal(r: MemberRefusal) -> AppError {
    match r {
        MemberRefusal::NotFound => pre(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "member session not found",
            "会话不存在",
        ),
        MemberRefusal::WrongChannel => pre(
            StatusCode::CONFLICT,
            "wrong_channel",
            "member session is not a text session",
            "会话类型不匹配",
        ),
        MemberRefusal::Archived => pre(
            StatusCode::CONFLICT,
            "session_archived",
            "member session is archived",
            "这段关系已归档",
        ),
        MemberRefusal::InstanceAlreadyMember => pre(
            StatusCode::CONFLICT,
            "instance_already_member",
            "the persona is already in the group",
            "这个角色已经在群里了",
        ),
        MemberRefusal::GroupFull => pre(
            StatusCode::CONFLICT,
            "group_full",
            &format!("the group already has {MAX_GROUP_MEMBERS} members"),
            &format!("群里最多 {MAX_GROUP_MEMBERS} 位角色"),
        ),
    }
}

fn title_arg(raw: Option<&str>) -> Result<Option<String>, AppError> {
    match raw {
        None => Ok(None),
        Some(t) => normalize_title(t).map(Some).ok_or_else(|| {
            pre(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_title",
                &format!(
                    "title must be 1 to {GROUP_TITLE_MAX_CHARS} characters after trimming, \
                     with no control character"
                ),
                "群名称无效",
            )
        }),
    }
}

async fn owned(state: &AppState, group_id: Uuid, user_id: Uuid) -> Result<Group, AppError> {
    GroupRepo { pool: &state.pool }
        .get(group_id, user_id)
        .await?
        .ok_or_else(group_not_found)
}

#[utoipa::path(post, path = "/v2/comp/group", tag = "group",
    request_body = CreateGroupRequest,
    responses(
        (status = 200, body = GroupDto),
        (status = 404, description = "`session_not_found`: a member session is unknown or not the caller's", body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 409, description = "`group_full`, `wrong_channel`, `session_archived` or `instance_already_member`", body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 422, description = "`invalid_title`, or `unprocessable` for an empty member list", body = crate::routes::companion_stream::StreamPreErrorBody),
    ),
    security(("bearer" = [])))]
async fn create_group(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<CreateGroupRequest>,
) -> Result<Json<GroupDto>, AppError> {
    if req.member_session_ids.is_empty() {
        return Err(pre(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            "member_session_ids must not be empty",
            "请至少选择一位角色",
        ));
    }
    let title = title_arg(req.title.as_deref())?;
    let g = GroupRepo { pool: &state.pool }
        .create(user_id, title.as_deref(), &req.member_session_ids)
        .await?
        .map_err(refusal)?;
    Ok(Json(group_dto(&g)))
}

#[utoipa::path(get, path = "/v2/comp/groups", tag = "group",
    responses((status = 200, body = GroupListResponse)), security(("bearer" = [])))]
async fn list_groups(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<GroupListResponse>, AppError> {
    let groups = GroupRepo { pool: &state.pool }.list(user_id).await?;
    Ok(Json(GroupListResponse {
        groups: groups.iter().map(group_dto).collect(),
    }))
}

#[utoipa::path(get, path = "/v2/comp/group/{group_id}", tag = "group",
    params(("group_id" = Uuid, Path)),
    responses(
        (status = 200, body = GroupDto),
        (status = 404, description = "`group_not_found`", body = crate::routes::companion_stream::StreamPreErrorBody),
    ),
    security(("bearer" = [])))]
async fn get_group(
    State(state): State<AppState>,
    Path(group_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<GroupDto>, AppError> {
    Ok(Json(group_dto(&owned(&state, group_id, user_id).await?)))
}

#[utoipa::path(patch, path = "/v2/comp/group/{group_id}", tag = "group",
    params(("group_id" = Uuid, Path)), request_body = RenameGroupRequest,
    responses(
        (status = 200, body = GroupDto),
        (status = 404, description = "`group_not_found`", body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 422, description = "`invalid_title`", body = crate::routes::companion_stream::StreamPreErrorBody),
    ),
    security(("bearer" = [])))]
async fn rename_group(
    State(state): State<AppState>,
    Path(group_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<RenameGroupRequest>,
) -> Result<Json<GroupDto>, AppError> {
    let title = title_arg(req.title.as_deref())?;
    let repo = GroupRepo { pool: &state.pool };
    if !repo.rename(group_id, user_id, title.as_deref()).await? {
        return Err(group_not_found());
    }
    Ok(Json(group_dto(&owned(&state, group_id, user_id).await?)))
}

#[utoipa::path(post, path = "/v2/comp/group/{group_id}/members", tag = "group",
    params(("group_id" = Uuid, Path)), request_body = AddMemberRequest,
    responses(
        (status = 200, body = GroupDto),
        (status = 404, description = "`group_not_found`, or `session_not_found` for the member session", body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 409, description = "`group_full`, `wrong_channel`, `session_archived` or `instance_already_member`", body = crate::routes::companion_stream::StreamPreErrorBody),
    ),
    security(("bearer" = [])))]
async fn add_member(
    State(state): State<AppState>,
    Path(group_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<AddMemberRequest>,
) -> Result<Json<GroupDto>, AppError> {
    owned(&state, group_id, user_id).await?;
    GroupRepo { pool: &state.pool }
        .add_member(group_id, user_id, req.member_session_id)
        .await?
        .map_err(refusal)?;
    Ok(Json(group_dto(&owned(&state, group_id, user_id).await?)))
}

#[utoipa::path(delete, path = "/v2/comp/group/{group_id}/members/{member_session_id}", tag = "group",
    params(("group_id" = Uuid, Path), ("member_session_id" = Uuid, Path)),
    responses(
        (status = 204),
        (status = 404, description = "`group_not_found`, or `member_not_found` when the session is not a member", body = crate::routes::companion_stream::StreamPreErrorBody),
    ),
    security(("bearer" = [])))]
async fn remove_member(
    State(state): State<AppState>,
    Path((group_id, member_session_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<StatusCode, AppError> {
    owned(&state, group_id, user_id).await?;
    let repo = GroupRepo { pool: &state.pool };
    if !repo
        .remove_member(group_id, user_id, member_session_id)
        .await?
    {
        return Err(pre(
            StatusCode::NOT_FOUND,
            "member_not_found",
            "the session is not a member of this group",
            "这个角色不在群里",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(delete, path = "/v2/comp/group/{group_id}", tag = "group",
    params(("group_id" = Uuid, Path)),
    responses(
        (status = 204),
        (status = 404, description = "`group_not_found`", body = crate::routes::companion_stream::StreamPreErrorBody),
    ),
    security(("bearer" = [])))]
async fn archive_group(
    State(state): State<AppState>,
    Path(group_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<StatusCode, AppError> {
    let repo = GroupRepo { pool: &state.pool };
    if !repo.archive(group_id, user_id).await? {
        return Err(group_not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct GroupSendRequest {
    pub content: String,
    pub client_msg_id: String,
    #[serde(default)]
    pub prompt_traits: Option<Vec<crate::routes::companion::PromptTraitDto>>,
    /// The 1:1 OpenRouter audit passthrough, with the same caps; it rides
    /// each speaker's reply call.
    #[serde(default)]
    pub audit: Option<crate::routes::companion::LlmAuditDto>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub image: Option<crate::routes::companion_stream::ImageReplyParams>,
    #[serde(default)]
    pub user_timezone: Option<String>,
    #[serde(default)]
    pub user_country: Option<String>,
    #[serde(default)]
    pub user_region: Option<String>,
    /// Member sessions the user @-mentioned; each must be seated.
    #[serde(default)]
    pub mentions: Vec<Uuid>,
    /// Refused: group turns run on the deployment's key only.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub byok: Option<serde_json::Value>,
}

fn duplicate_in_progress(user_message_id: Uuid) -> AppError {
    AppError::StreamPre(crate::error::StreamPreError {
        status: StatusCode::CONFLICT,
        code: "duplicate_in_progress",
        message: "same (session_id, client_msg_id) is still generating".into(),
        user_message: "上一条消息正在处理中，请稍候".into(),
        original_user_message_id: Some(user_message_id),
    })
}

#[utoipa::path(post, path = "/v2/comp/group/{group_id}/message/stream", tag = "group",
    params(("group_id" = Uuid, Path)), request_body = GroupSendRequest,
    responses(
        (status = 200, description = "SSE: director_reasoning* → cast → (meta → delta* → done → image_request? | meta → done{truncated:true} → speaker_error | speaker_error)×N → final. \
            A failed speaker emits `meta → done{truncated:true}` for each attempt and then `speaker_error` when its chain is exhausted, \
            or a bare `speaker_error` when its request could not be built or its image row could not be persisted. \
            A terminal `error` can replace anything after the first frame.", content_type = "text/event-stream"),
        (status = 400, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 404, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 409, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 422, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 429, body = crate::routes::companion_stream::StreamPreErrorBody),
    ),
    security(("bearer" = [])))]
async fn send_group_message_stream(
    State(state): State<AppState>,
    Path(group_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<GroupSendRequest>,
) -> Result<
    axum::response::sse::Sse<
        impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    AppError,
> {
    use crate::pipeline::stream::ProtocolFrame;
    use crate::routes::companion_stream::{
        validate_image_params, validate_send_basics, CONCURRENT_STREAMS_PER_USER,
    };
    use axum::response::sse::{Event, KeepAlive, Sse};
    use eros_engine_store::chat::ChatRepo;
    use eros_engine_store::chat_queue::{ChatQueueRepo, ClaimedEnqueueOutcome};

    if req.content.trim().is_empty() {
        return Err(pre(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            "content must not be empty",
            "请输入一条消息",
        ));
    }
    validate_send_basics(&req.content, &req.client_msg_id, req.tier.as_deref())?;
    validate_image_params(req.image.as_ref())?;
    if req.byok.is_some() {
        return Err(pre(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            "byok is not available on group turns",
            "请求无效",
        ));
    }
    if req.image.as_ref().is_some_and(|i| i.force) {
        return Err(pre(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            "image.force has no single persona to force on a group turn",
            "请求无效",
        ));
    }
    let prompt_traits = crate::routes::companion::validate_prompt_traits(
        req.prompt_traits.as_deref().unwrap_or(&[]),
    )
    .map_err(|e| {
        pre(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            &e.to_string(),
            "请求无效",
        )
    })?;
    let audit = crate::routes::companion::validate_llm_audit(req.audit.clone()).map_err(|e| {
        pre(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            &e.to_string(),
            "请求无效",
        )
    })?;

    let session = ChatRepo { pool: &state.pool }
        .get_session(group_id)
        .await?
        .filter(|s| s.user_id == user_id)
        .ok_or_else(group_not_found)?;
    if session.channel != "group" {
        return Err(pre(
            StatusCode::CONFLICT,
            "wrong_channel",
            &format!("session is a {}-channel session", session.channel),
            "会话类型不匹配",
        ));
    }
    let members = GroupRepo { pool: &state.pool }
        .active_members(group_id)
        .await?;
    if members.is_empty() {
        return Err(pre(
            StatusCode::CONFLICT,
            "group_empty",
            "the group has no members",
            "群里还没有角色",
        ));
    }
    if req
        .mentions
        .iter()
        .any(|m| !members.iter().any(|x| x.member_session_id == *m))
    {
        return Err(pre(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            "a mention is not a seated member",
            "点名的角色不在群里",
        ));
    }

    let guard = state
        .stream_slots
        .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
        .ok_or_else(|| {
            pre(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                &format!("per-user stream cap reached ({CONCURRENT_STREAMS_PER_USER})"),
                "请求过多，请稍后再试",
            )
        })?;

    let metadata =
        (!req.mentions.is_empty()).then(|| serde_json::json!({ "mentions": req.mentions }));
    let params = serde_json::to_value(crate::routes::companion_async::QueuedTurnParams {
        tier: req.tier.clone(),
        prompt_traits: req.prompt_traits.clone(),
        audit: req.audit.clone(),
        image: req.image.clone(),
        user_timezone: crate::holiday::bounded_raw(req.user_timezone.as_deref()),
        user_country: crate::holiday::bounded_raw(req.user_country.as_deref()),
        user_region: crate::holiday::bounded_raw(req.user_region.as_deref()),
        ..Default::default()
    })
    .expect("QueuedTurnParams serializes");
    let outcome = ChatQueueRepo { pool: &state.pool }
        .enqueue_user_message_claimed(
            group_id,
            &req.content,
            &req.client_msg_id,
            "user",
            metadata.as_ref(),
            user_id,
            &params,
        )
        .await?;

    let proto: std::pin::Pin<Box<dyn futures_util::Stream<Item = ProtocolFrame> + Send>> =
        match outcome {
            ClaimedEnqueueOutcome::Claimed {
                user_message_id,
                queue_id,
            } => {
                let turn = crate::pipeline::group::GroupTurn {
                    group_session_id: group_id,
                    user_id,
                    user_message_id,
                    content: req.content.clone(),
                    title: session.title.clone(),
                    members,
                    mentions: req.mentions.clone(),
                    prompt_traits,
                    audit,
                    tier: req.tier.clone(),
                    user_locale: crate::holiday::UserLocale::resolve(
                        req.user_timezone.as_deref(),
                        req.user_country.as_deref(),
                        req.user_region.as_deref(),
                    ),
                    image: req.image.clone(),
                };
                let claimed = eros_engine_store::chat_queue::ClaimedTurn {
                    queue_id,
                    session_id: group_id,
                    user_message_id,
                    user_id,
                    attempts: 1,
                    params: Some(params),
                };
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProtocolFrame>();
                let st = state.clone();
                // The detached task owns the turn and the stream slot; the SSE
                // body is only a tap, so a disconnect never cancels the turn.
                tokio::spawn(async move {
                    let _guard = guard;
                    // `tx` moves into the drive and drops with it when the
                    // budget expires; this clone carries the timeout frame.
                    let timeout_tx = tx.clone();
                    let timeout = st.config.chat_queue.generation_timeout;
                    let outcome = match tokio::time::timeout(
                        timeout,
                        crate::pipeline::group::drive_group_to_exhaustion(
                            std::sync::Arc::new(st.clone()),
                            turn,
                            tx,
                        ),
                    )
                    .await
                    {
                        Ok(o) => o,
                        Err(_) => {
                            let _ = timeout_tx.send(ProtocolFrame::Error {
                                code: crate::pipeline::stream::StreamErrorCode::GenerationTimeout,
                                retryable: true,
                                message: "generation timeout".into(),
                                user_message: "生成超时，请稍后再试".into(),
                                upstream_status: None,
                                provider_code: None,
                            });
                            crate::pipeline::chat_queue::TurnOutcome::Failure(
                                "generation timeout".into(),
                            )
                        }
                    };
                    // Close the SSE body before the settle write.
                    drop(timeout_tx);
                    crate::pipeline::chat_queue::settle_turn(
                        &st,
                        &claimed,
                        outcome,
                        crate::pipeline::chat_queue::SettleMode::TerminalOnFailure,
                    )
                    .await;
                    st.chat_queue_notify.notify_one();
                });
                Box::pin(async_stream::stream! {
                    while let Some(f) = rx.recv().await {
                        yield f;
                    }
                })
            }
            ClaimedEnqueueOutcome::Replay {
                user_message_id,
                ghost,
                assistant_chain,
            } => {
                drop(guard);
                // An earlier speaker's row makes the turn look served while
                // later speakers are still running; replay only a settled turn.
                let status: Option<String> = sqlx::query_scalar(
                    "SELECT status FROM engine.chat_turn_queue WHERE user_message_id = $1",
                )
                .bind(user_message_id)
                .fetch_optional(&state.pool)
                .await?;
                if status.as_deref() == Some("claimed") {
                    return Err(duplicate_in_progress(user_message_id));
                }
                // The chain captured above may predate later speakers; reload
                // it now that the turn is settled.
                let assistant_chain = if assistant_chain.is_empty() {
                    assistant_chain
                } else {
                    eros_engine_store::chat::ChatRepo { pool: &state.pool }
                        .replay_chain(user_message_id)
                        .await?
                };
                Box::pin(crate::pipeline::stream::replay_stream(
                    std::sync::Arc::new(state.clone()),
                    group_id,
                    user_id,
                    ghost,
                    assistant_chain,
                ))
            }
            ClaimedEnqueueOutcome::DuplicateInProgress { user_message_id } => {
                return Err(duplicate_in_progress(user_message_id));
            }
            ClaimedEnqueueOutcome::DuplicateFailed { user_message_id } => {
                return Err(AppError::StreamPre(crate::error::StreamPreError {
                    status: StatusCode::CONFLICT,
                    code: "duplicate_failed",
                    message: "same (session_id, client_msg_id) terminally failed; \
                              a same-id retry can never succeed"
                        .into(),
                    user_message: "这条消息发送失败，请重新发送".into(),
                    original_user_message_id: Some(user_message_id),
                }));
            }
        };

    let sse = futures_util::StreamExt::map(proto, |frame: ProtocolFrame| {
        let json =
            serde_json::to_string(&frame).expect("ProtocolFrame serialization is infallible");
        Ok::<_, std::convert::Infallible>(Event::default().data(json))
    });
    Ok(Sse::new(sse).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("ping"),
    ))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_group))
        .routes(routes!(list_groups))
        .routes(routes!(get_group, rename_group, archive_group))
        .routes(routes!(add_member))
        .routes(routes!(remove_member))
        .routes(routes!(send_group_message_stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::companion::testutil::{
        build_router, mint_test_jwt, seed_persona_instance, seed_session, send_request,
    };
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use serde_json::{json, Value};
    use sqlx::PgPool;

    fn req(method: &str, uri: &str, user: Uuid, body: Option<Value>) -> Request<Body> {
        let b = Request::builder().method(method).uri(uri).header(
            header::AUTHORIZATION,
            format!("Bearer {}", mint_test_jwt(user)),
        );
        match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&v).unwrap()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        }
    }

    async fn member(pool: &PgPool, user: Uuid) -> Uuid {
        let instance = seed_persona_instance(pool, user).await;
        seed_session(pool, user, instance).await
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn create_patch_add_kick_archive(pool: PgPool) {
        let user = Uuid::new_v4();
        let a = member(&pool, user).await;
        let b = member(&pool, user).await;
        let mut app = build_router(crate::routes::companion::test_state(pool.clone()));

        let (s, body) = send_request(
            &mut app,
            req(
                "POST",
                "/v2/comp/group",
                user,
                Some(json!({"title": " 周末爬山 ", "member_session_ids": [a]})),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(body["title"], "周末爬山");
        let gid = body["group_id"].as_str().unwrap().to_string();

        let (s, body) = send_request(
            &mut app,
            req(
                "POST",
                &format!("/v2/comp/group/{gid}/members"),
                user,
                Some(json!({"member_session_id": b})),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(body["members"].as_array().unwrap().len(), 2);

        let (s, body) = send_request(
            &mut app,
            req(
                "PATCH",
                &format!("/v2/comp/group/{gid}"),
                user,
                Some(json!({"title": null})),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(body["title"].is_null());

        let (s, _) = send_request(
            &mut app,
            req(
                "DELETE",
                &format!("/v2/comp/group/{gid}/members/{a}"),
                user,
                None,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);

        let (s, body) = send_request(&mut app, req("GET", "/v2/comp/groups", user, None)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["groups"][0]["members"].as_array().unwrap().len(), 1);

        let (s, _) = send_request(
            &mut app,
            req("DELETE", &format!("/v2/comp/group/{gid}"), user, None),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _) = send_request(
            &mut app,
            req("GET", &format!("/v2/comp/group/{gid}"), user, None),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn create_with_foreign_session_is_404(pool: PgPool) {
        let user = Uuid::new_v4();
        let foreign = member(&pool, Uuid::new_v4()).await;
        let mut app = build_router(crate::routes::companion::test_state(pool.clone()));
        let (s, body) = send_request(
            &mut app,
            req(
                "POST",
                "/v2/comp/group",
                user,
                Some(json!({"member_session_ids": [foreign]})),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "session_not_found", "{body}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn add_member_refusals_carry_their_codes(pool: PgPool) {
        let user = Uuid::new_v4();
        let a = member(&pool, user).await;
        let mut app = build_router(crate::routes::companion::test_state(pool.clone()));
        let (_, body) = send_request(
            &mut app,
            req(
                "POST",
                "/v2/comp/group",
                user,
                Some(json!({"member_session_ids": [a]})),
            ),
        )
        .await;
        let gid = body["group_id"].as_str().unwrap().to_string();

        let instance = seed_persona_instance(&pool, user).await;
        let voice = eros_engine_store::chat::ChatRepo { pool: &pool }
            .create_session_with_metadata(user, instance, json!({}), "voice")
            .await
            .unwrap()
            .id;
        let archived = member(&pool, user).await;
        sqlx::query("UPDATE engine.chat_sessions SET archived = true WHERE id = $1")
            .bind(archived)
            .execute(&pool)
            .await
            .unwrap();
        let foreign = member(&pool, Uuid::new_v4()).await;
        let cases = [
            (voice, StatusCode::CONFLICT, "wrong_channel"),
            (archived, StatusCode::CONFLICT, "session_archived"),
            (a, StatusCode::CONFLICT, "instance_already_member"),
            (foreign, StatusCode::NOT_FOUND, "session_not_found"),
        ];
        for (session, status, code) in cases {
            let (s, body) = send_request(
                &mut app,
                req(
                    "POST",
                    &format!("/v2/comp/group/{gid}/members"),
                    user,
                    Some(json!({"member_session_id": session})),
                ),
            )
            .await;
            assert_eq!((s, body["code"].as_str()), (status, Some(code)), "{body}");
            assert!(body["user_message"].is_string(), "{body}");
        }

        let (s, body) = send_request(
            &mut app,
            req(
                "DELETE",
                &format!("/v2/comp/group/{gid}/members/{voice}"),
                user,
                None,
            ),
        )
        .await;
        assert_eq!(
            (s, body["code"].as_str()),
            (StatusCode::NOT_FOUND, Some("member_not_found")),
            "{body}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn full_group_and_empty_create_are_refused(pool: PgPool) {
        let user = Uuid::new_v4();
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(member(&pool, user).await);
        }
        let mut app = build_router(crate::routes::companion::test_state(pool.clone()));
        let (s, body) = send_request(
            &mut app,
            req(
                "POST",
                "/v2/comp/group",
                user,
                Some(json!({"member_session_ids": ids})),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(body["code"], "group_full", "{body}");
        let (s, body) = send_request(
            &mut app,
            req(
                "POST",
                "/v2/comp/group",
                user,
                Some(json!({"member_session_ids": []})),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["code"], "unprocessable", "{body}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn patch_title_rejects_blank_unicode(pool: PgPool) {
        let user = Uuid::new_v4();
        let a = member(&pool, user).await;
        let mut app = build_router(crate::routes::companion::test_state(pool.clone()));
        let (_, body) = send_request(
            &mut app,
            req(
                "POST",
                "/v2/comp/group",
                user,
                Some(json!({"member_session_ids": [a]})),
            ),
        )
        .await;
        let gid = body["group_id"].as_str().unwrap().to_string();
        for bad in ["　　", "a\u{0007}b"] {
            let (s, body) = send_request(
                &mut app,
                req(
                    "PATCH",
                    &format!("/v2/comp/group/{gid}"),
                    user,
                    Some(json!({"title": bad})),
                ),
            )
            .await;
            assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad:?}");
            assert_eq!(body["code"], "invalid_title", "{body}");
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn history_carries_the_speaker(pool: PgPool) {
        let user = Uuid::new_v4();
        let a = member(&pool, user).await;
        let g = eros_engine_store::group::GroupRepo { pool: &pool }
            .create(user, None, &[a])
            .await
            .unwrap()
            .unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, speaker_session_id) \
             VALUES ($1, 'assistant', '嗨', $2)",
        )
        .bind(g.id)
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
        let mut app = build_router(crate::routes::companion::test_state(pool.clone()));
        let (s, body) = send_request(
            &mut app,
            req("GET", &format!("/comp/chat/{}/history", g.id), user, None),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let entry = &body["messages"][0];
        assert_eq!(entry["speaker_session_id"], a.to_string());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_stranger_gets_404_and_changes_nothing(pool: PgPool) {
        let owner = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        let a = member(&pool, owner).await;
        let mut app = build_router(crate::routes::companion::test_state(pool.clone()));
        let (_, body) = send_request(
            &mut app,
            req(
                "POST",
                "/v2/comp/group",
                owner,
                Some(json!({"title": "keep", "member_session_ids": [a]})),
            ),
        )
        .await;
        let gid = body["group_id"].as_str().unwrap().to_string();

        let g = format!("/v2/comp/group/{gid}");
        let attempts = [
            req("GET", &g, stranger, None),
            req("PATCH", &g, stranger, Some(json!({"title": "x"}))),
            req(
                "POST",
                &format!("{g}/members"),
                stranger,
                Some(json!({"member_session_id": a})),
            ),
            req("DELETE", &format!("{g}/members/{a}"), stranger, None),
            req("DELETE", &g, stranger, None),
        ];
        for r in attempts {
            let (s, body) = send_request(&mut app, r).await;
            assert_eq!(s, StatusCode::NOT_FOUND);
            assert_eq!(body["code"], "group_not_found", "{body}");
        }
        let (s, body) = send_request(&mut app, req("GET", &g, owner, None)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["title"], "keep");
        assert_eq!(body["members"].as_array().unwrap().len(), 1);
    }

    struct Fixture {
        app: axum::Router,
        pool: PgPool,
        user: Uuid,
        group: Uuid,
        a: Uuid,
        b: Uuid,
        _server: wiremock::MockServer,
    }

    async fn name_member(pool: &PgPool, session: Uuid, name: &str) {
        sqlx::query(
            "UPDATE engine.persona_genomes g SET name = $2, tip_personality = '爱聊天' \
             FROM engine.persona_instances pi, engine.chat_sessions s \
             WHERE s.id = $1 AND pi.id = s.instance_id AND g.id = pi.genome_id",
        )
        .bind(session)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    }

    fn sse(frames: &[String]) -> String {
        let mut s: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
        s.push_str("data: [DONE]\n\n");
        s
    }

    async fn fixture(
        pool: PgPool,
        director_body: Option<String>,
        chat_status: u16,
        gen_timeout: Option<std::time::Duration>,
    ) -> Fixture {
        fixture_with(pool, director_body, chat_status, gen_timeout, "", |_| {}).await
    }

    /// [`fixture`] with extra `[tasks.*]` TOML and a last word on the state.
    async fn fixture_with(
        pool: PgPool,
        director_body: Option<String>,
        chat_status: u16,
        gen_timeout: Option<std::time::Duration>,
        extra_toml: &str,
        tweak: impl FnOnce(&mut AppState),
    ) -> Fixture {
        use wiremock::matchers::{body_partial_json, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let director_body_present = director_body.is_some();
        let user = Uuid::new_v4();
        let a = member(&pool, user).await;
        let b = member(&pool, user).await;
        name_member(&pool, a, "小美").await;
        name_member(&pool, b, "阿哲").await;
        let g = eros_engine_store::group::GroupRepo { pool: &pool }
            .create(user, Some("周末爬山"), &[a, b])
            .await
            .unwrap()
            .unwrap();

        let server = MockServer::start().await;
        if let Some(body) = director_body {
            Mock::given(path("/api/v1/chat/completions"))
                .and(body_partial_json(json!({"model": "dir-model"})))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_raw(body, "text/event-stream"),
                )
                .mount(&server)
                .await;
        }
        let chat = if chat_status == 200 {
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    sse(&[
                        r#"{"choices":[{"delta":{"content":"好呀"}}],"id":"gen-chat"}"#.to_string(),
                    ]),
                    "text/event-stream",
                )
        } else {
            ResponseTemplate::new(chat_status)
        };
        Mock::given(path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({"model": "chat-model"})))
            .respond_with(chat)
            .mount(&server)
            .await;

        let director_section = if director_body_present {
            "[tasks.group_director]\nmodel = \"dir-model\"\nfallback = []\nfilter_prompt = \"direct\"\n"
        } else {
            ""
        };
        let cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(&format!(
            "[tasks.chat_companion]\nmodel = \"chat-model\"\nfallback = []\n{director_section}{extra_toml}"
        ))
        .unwrap();
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(cfg);
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", server.uri()),
            ),
        );
        if let Some(t) = gen_timeout {
            state.config.chat_queue.generation_timeout = t;
        }
        tweak(&mut state);
        Fixture {
            app: build_router(state),
            pool,
            user,
            group: g.id,
            a,
            b,
            _server: server,
        }
    }

    async fn send(f: &mut Fixture, body: Value) -> (StatusCode, String) {
        use tower::Service;
        let req = req(
            "POST",
            &format!("/v2/comp/group/{}/message/stream", f.group),
            f.user,
            Some(body),
        );
        let resp = f.app.call(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn director_sse(verdict: &str) -> String {
        let escaped = verdict.replace('"', "\\\"");
        sse(&[
            r#"{"choices":[{"delta":{"reasoning":"阿哲被点名了"}}]}"#.to_string(),
            format!(r#"{{"choices":[{{"delta":{{"content":"{escaped}"}}}}],"id":"gen-dir"}}"#),
        ])
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_directed_turn_streams_in_order_and_writes_no_affinity(pool: PgPool) {
        let verdict = r#"{"speakers":[{"member":"m2","action":"reply_text","inner_state":"有点兴奋","reply_mode":null,"clothing":null},{"member":"m1","action":"reply_text","inner_state":null,"reply_mode":null,"clothing":null}],"reason":null}"#;
        let mut f = fixture(pool, Some(director_sse(verdict)), 200, None).await;
        let (s, body) = send(
            &mut f,
            json!({"content":"阿哲你去不去","client_msg_id":"01J7777777777777777777777G"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");

        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}: {body}"))
        };
        let reasoning = at(r#""type":"director_reasoning""#);
        let cast = at(r#""type":"cast""#);
        let first_meta = at(&format!(r#""speaker_session_id":"{}""#, f.b));
        let second_meta = at(&format!(r#""speaker_session_id":"{}""#, f.a));
        let fin = at(r#""type":"final""#);
        assert!(
            reasoning < cast && cast < first_meta && first_meta < second_meta && second_meta < fin,
            "{body}"
        );

        let speakers: Vec<Option<Uuid>> = sqlx::query_scalar(
            "SELECT speaker_session_id FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' ORDER BY sent_at, id",
        )
        .bind(f.group)
        .fetch_all(&f.pool)
        .await
        .unwrap();
        assert_eq!(speakers, vec![Some(f.b), Some(f.a)]);

        let affinity: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.companion_affinity WHERE user_id = $1")
                .bind(f.user)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(affinity, 0);

        let (status, has_reasoning): (String, bool) = sqlx::query_as(
            "SELECT status, payload ? 'reasoning' FROM engine.companion_decision_events WHERE session_id = $1",
        )
        .bind(f.group)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!((status.as_str(), has_reasoning), ("ok", true));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_later_speaker_hears_the_earlier_one(pool: PgPool) {
        use wiremock::matchers::body_string_contains;
        let verdict = r#"{"speakers":[{"member":"m1","action":"reply_text","inner_state":null,"reply_mode":null,"clothing":null},{"member":"m2","action":"reply_text","inner_state":null,"reply_mode":null,"clothing":null}],"reason":null}"#;
        let mut f = fixture(pool, Some(director_sse(verdict)), 200, None).await;
        // The second speaker's request carries m1's persisted line, prefixed.
        wiremock::Mock::given(wiremock::matchers::path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                json!({"model": "chat-model"}),
            ))
            .and(body_string_contains("小美：好呀"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[r#"{"choices":[{"delta":{"content":"我也去"}}]}"#.to_string()]),
                        "text/event-stream",
                    ),
            )
            .with_priority(1)
            .expect(1)
            .mount(&f._server)
            .await;
        let (s, body) = send(
            &mut f,
            json!({"content":"去爬山吗","client_msg_id":"01J8888888888888888888888G"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("我也去"), "{body}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_director_means_everyone_answers_in_join_order(pool: PgPool) {
        let mut f = fixture(pool, None, 200, None).await;
        let (_, body) = send(
            &mut f,
            json!({"content":"大家好","client_msg_id":"01J9999999999999999999999G"}),
        )
        .await;
        let a = body
            .find(&format!(r#""member_session_id":"{}""#, f.a))
            .unwrap();
        let b = body
            .find(&format!(r#""member_session_id":"{}""#, f.b))
            .unwrap();
        assert!(a < b, "cast in join order: {body}");
        assert!(!body.contains("director_reasoning"));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_unparseable_director_falls_back(pool: PgPool) {
        let mut f = fixture(
            pool,
            Some(sse(&[
                r#"{"choices":[{"delta":{"content":"m2 先说"}}]}"#.to_string()
            ])),
            200,
            None,
        )
        .await;
        let (_, body) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JAAAAAAAAAAAAAAAAAAAAAAG"}),
        )
        .await;
        assert_eq!(
            body.matches(r#""type":"meta""#).count(),
            2,
            "both members answer: {body}"
        );
        let status: String = sqlx::query_scalar(
            "SELECT status FROM engine.companion_decision_events WHERE session_id = $1",
        )
        .bind(f.group)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(status, "parse_error");
    }

    /// The group turn's queue status once the detached task has settled it.
    async fn settled_status(pool: &PgPool, group: Uuid) -> String {
        let mut status = String::new();
        for _ in 0..100 {
            status = sqlx::query_scalar(
                "SELECT q.status FROM engine.chat_turn_queue q JOIN engine.chat_messages m ON m.id = q.user_message_id \
                 WHERE m.session_id = $1",
            )
            .bind(group)
            .fetch_one(pool)
            .await
            .unwrap();
            if status != "claimed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        status
    }

    /// No fallback phrase: an exhausted chat chain ends the speaker's burst in
    /// an error instead of serving the canned line.
    async fn clear_fallback_phrases(pool: &PgPool) {
        sqlx::query(
            "UPDATE engine.error_handling_config SET payload = '[]'::jsonb \
             WHERE kind = 'chat_stream_failure_fallback_phrases'",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn every_speaker_failing_ends_in_error(pool: PgPool) {
        let mut f = fixture(pool, None, 500, None).await;
        clear_fallback_phrases(&f.pool).await;
        let (_, body) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JBBBBBBBBBBBBBBBBBBBBBBG"}),
        )
        .await;
        assert_eq!(
            body.matches(r#""type":"speaker_error""#).count(),
            2,
            "{body}"
        );
        assert!(body.contains(r#""type":"error""#), "{body}");
        // Each failed attempt left a truncated row, so the turn counts as
        // served: it settles `done` and writes no notice.
        assert_eq!(settled_status(&f.pool, f.group).await, "done");
        let notices: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages WHERE session_id = $1 AND role = 'system_error'",
        )
        .bind(f.group)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(notices, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_seatable_member_ends_in_error_and_a_notice(pool: PgPool) {
        let mut f = fixture(pool, None, 200, None).await;
        sqlx::query(
            "UPDATE engine.persona_instances SET status = 'archived' \
             WHERE id IN (SELECT instance_id FROM engine.chat_sessions WHERE id = ANY($1))",
        )
        .bind(vec![f.a, f.b])
        .execute(&f.pool)
        .await
        .unwrap();
        let (_, body) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JBBBBBBBBBBBBBBBBBBBBBBG"}),
        )
        .await;
        assert!(body.contains(r#""type":"error""#), "{body}");
        assert!(!body.contains(r#""type":"cast""#), "{body}");
        // settle runs on the detached task; poll for it.
        let mut notices = 0i64;
        for _ in 0..100 {
            notices = sqlx::query_scalar(
                "SELECT count(*) FROM engine.chat_messages WHERE session_id = $1 AND role = 'system_error'",
            )
            .bind(f.group)
            .fetch_one(&f.pool)
            .await
            .unwrap();
            if notices > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(notices, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn one_failing_speaker_does_not_stop_the_relay(pool: PgPool) {
        use wiremock::matchers::body_string_contains;
        // Order is 小美 then 阿哲 (no director). Swap it so 阿哲 fails FIRST
        // and 小美 still answers after him: kick and re-add 小美.
        let mut f = fixture(pool, None, 200, None).await;
        clear_fallback_phrases(&f.pool).await;
        let repo = eros_engine_store::group::GroupRepo { pool: &f.pool };
        repo.remove_member(f.group, f.user, f.a).await.unwrap();
        repo.add_member(f.group, f.user, f.a)
            .await
            .unwrap()
            .unwrap();
        // 阿哲 speaks first now; his request is the one WITHOUT 小美's line.
        // Fail every chat call whose body names 阿哲's own block line …
        wiremock::Mock::given(wiremock::matchers::path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                json!({"model": "chat-model"}),
            ))
            .and(body_string_contains("在场的还有：\\n- 小美"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .with_priority(1)
            .mount(&f._server)
            .await;
        let (_, body) = send(
            &mut f,
            json!({"content":"在吗","client_msg_id":"01JMMMMMMMMMMMMMMMMMMMMMMG"}),
        )
        .await;
        let err = body
            .find(r#""type":"speaker_error""#)
            .unwrap_or_else(|| panic!("{body}"));
        let later = body
            .find(&format!(r#""speaker_session_id":"{}""#, f.a))
            .unwrap_or_else(|| panic!("{body}"));
        assert!(err < later, "the relay continued after the failure: {body}");
        assert!(body.contains(r#""type":"final""#), "{body}");
        // The failed attempt leaves a truncated bubble, as on a 1:1 turn.
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' AND NOT truncated",
        )
        .bind(f.group)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(rows, 1);
        assert_eq!(settled_status(&f.pool, f.group).await, "done");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn inactive_persona_is_not_seated(pool: PgPool) {
        let mut f = fixture(pool, None, 200, None).await;
        sqlx::query(
            "UPDATE engine.persona_instances SET status = 'archived' \
                     WHERE id = (SELECT instance_id FROM engine.chat_sessions WHERE id = $1)",
        )
        .bind(f.a)
        .execute(&f.pool)
        .await
        .unwrap();
        let (_, body) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JCCCCCCCCCCCCCCCCCCCCCCG"}),
        )
        .await;
        assert_eq!(body.matches(r#""type":"meta""#).count(), 1, "{body}");
        assert!(!body.contains(&f.a.to_string()), "{body}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn director_reads_the_members_1to1_affinity(pool: PgPool) {
        let verdict = r#"{"speakers":[{"member":"m1","action":"reply_text","inner_state":null,"reply_mode":null,"clothing":null}],"reason":null}"#;
        let mut f = fixture(pool, Some(director_sse(verdict)), 200, None).await;
        // A close 1:1 relationship for 小美 (bond score 0.95 ⇒ tier 5, clear of the 0.90 edge after drift).
        let instance: Uuid =
            sqlx::query_scalar("SELECT instance_id FROM engine.chat_sessions WHERE id = $1")
                .bind(f.a)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO engine.companion_affinity (session_id, user_id, instance_id, trust, intrigue) \
             VALUES ($1, $2, $3, 0.95, 0.95)",
        )
        .bind(f.a)
        .bind(f.user)
        .bind(instance)
        .execute(&f.pool)
        .await
        .unwrap();
        let before: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "SELECT updated_at FROM engine.companion_affinity WHERE session_id = $1",
        )
        .bind(f.a)
        .fetch_one(&f.pool)
        .await
        .unwrap();

        let (s, _) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JKKKKKKKKKKKKKKKKKKKKKKG"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let bond: String = sqlx::query_scalar(
            "SELECT inputs->'members'->0->>'bond' FROM engine.companion_decision_events WHERE session_id = $1",
        )
        .bind(f.group)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(bond, "soulmate", "the director sees the member's 1:1 tier");
        let after: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "SELECT updated_at FROM engine.companion_affinity WHERE session_id = $1",
        )
        .bind(f.a)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(before, after, "the group read it and never wrote it");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_timeout_mid_relay_keeps_the_finished_speaker(pool: PgPool) {
        use wiremock::matchers::body_string_contains;
        // No director: 小美 then 阿哲. 阿哲's request (it carries 小美's line)
        // hangs past the whole-turn budget.
        let mut f = fixture(pool, None, 200, Some(std::time::Duration::from_secs(2))).await;
        wiremock::Mock::given(wiremock::matchers::path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                json!({"model": "chat-model"}),
            ))
            .and(body_string_contains("小美：好呀"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[r#"{"choices":[{"delta":{"content":"迟到"}}]}"#.to_string()]),
                        "text/event-stream",
                    )
                    .set_delay(std::time::Duration::from_secs(10)),
            )
            .with_priority(1)
            .mount(&f._server)
            .await;
        let (_, body) = send(
            &mut f,
            json!({"content":"在吗","client_msg_id":"01JLLLLLLLLLLLLLLLLLLLLLLG"}),
        )
        .await;
        assert!(body.contains(r#""code":"generation_timeout""#), "{body}");
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(f.group)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(rows, 1, "小美's line stays persisted");
        // A served row makes the terminal settle `done`, with no notice.
        let mut status = String::new();
        for _ in 0..100 {
            status = sqlx::query_scalar(
                "SELECT q.status FROM engine.chat_turn_queue q JOIN engine.chat_messages m ON m.id = q.user_message_id \
                 WHERE m.session_id = $1",
            )
            .bind(f.group)
            .fetch_one(&f.pool)
            .await
            .unwrap();
            if status != "claimed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(status, "done");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn send_refusals(pool: PgPool) {
        let mut f = fixture(pool, None, 200, None).await;
        // The body checks the 1:1 route makes apply here too.
        let (s, _) = send(&mut f, json!({"content":"嗨","client_msg_id":"short"})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JNNNNNNNNNNNNNNNNNNNNNNG","image":{"aspect_ratio":"7:5"}}),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        let (s, _) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JDDDDDDDDDDDDDDDDDDDDDDG","mentions":[Uuid::new_v4()]}),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        let (s, _) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JEEEEEEEEEEEEEEEEEEEEEEG","image":{"force":true}}),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JFFFFFFFFFFFFFFFFFFFFFFG","byok":{}}),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        // A text session is the wrong channel; a stranger's group is 404.
        use tower::Service;
        let r = req(
            "POST",
            &format!("/v2/comp/group/{}/message/stream", f.a),
            f.user,
            Some(json!({"content":"嗨","client_msg_id":"01JGGGGGGGGGGGGGGGGGGGGGGG"})),
        );
        assert_eq!(f.app.call(r).await.unwrap().status(), StatusCode::CONFLICT);
        let r = req(
            "POST",
            &format!("/v2/comp/group/{}/message/stream", f.group),
            Uuid::new_v4(),
            Some(json!({"content":"嗨","client_msg_id":"01JHHHHHHHHHHHHHHHHHHHHHHG"})),
        );
        assert_eq!(f.app.call(r).await.unwrap().status(), StatusCode::NOT_FOUND);
        // Kick everyone: group_empty.
        for m in [f.a, f.b] {
            eros_engine_store::group::GroupRepo { pool: &f.pool }
                .remove_member(f.group, f.user, m)
                .await
                .unwrap();
        }
        let (s, _) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JJJJJJJJJJJJJJJJJJJJJJJG"}),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_dissolved_group_is_404(pool: PgPool) {
        let mut f = fixture(pool, None, 200, None).await;
        eros_engine_store::group::GroupRepo { pool: &f.pool }
            .archive(f.group, f.user)
            .await
            .unwrap();
        let (s, body) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JQQQQQQQQQQQQQQQQQQQQQQG"}),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["code"], "group_not_found", "{body}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_resent_message_replays_the_speakers(pool: PgPool) {
        let mut f = fixture(pool, None, 200, None).await;
        let msg = json!({"content":"嗨","client_msg_id":"01JPPPPPPPPPPPPPPPPPPPPPPG"});
        let (_, first) = send(&mut f, msg.clone()).await;
        assert_eq!(first.matches(r#""type":"meta""#).count(), 2, "{first}");
        // The resend replays only once the turn has settled.
        for _ in 0..100 {
            let status: String = sqlx::query_scalar(
                "SELECT q.status FROM engine.chat_turn_queue q JOIN engine.chat_messages m ON m.id = q.user_message_id \
                 WHERE m.session_id = $1",
            )
            .bind(f.group)
            .fetch_one(&f.pool)
            .await
            .unwrap();
            if status != "claimed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let (s, replay) = send(&mut f, msg).await;
        assert_eq!(s, StatusCode::OK, "{replay}");
        let a = replay
            .find(&format!(r#""speaker_session_id":"{}""#, f.a))
            .unwrap_or_else(|| panic!("{replay}"));
        let b = replay
            .find(&format!(r#""speaker_session_id":"{}""#, f.b))
            .unwrap_or_else(|| panic!("{replay}"));
        assert!(a < b, "{replay}");
        assert!(replay.contains(r#""type":"final""#), "{replay}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_resend_mid_relay_is_still_in_progress(pool: PgPool) {
        use tower::Service;
        use wiremock::matchers::body_string_contains;
        // No director: 小美 then 阿哲. 阿哲's request (it carries 小美's line)
        // is held back, so the turn is still claimed once 小美's row lands.
        let mut f = fixture(pool, None, 200, None).await;
        wiremock::Mock::given(wiremock::matchers::path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                json!({"model": "chat-model"}),
            ))
            .and(body_string_contains("小美：好呀"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[r#"{"choices":[{"delta":{"content":"我也在"}}]}"#.to_string()]),
                        "text/event-stream",
                    )
                    .set_delay(std::time::Duration::from_secs(3)),
            )
            .with_priority(1)
            .mount(&f._server)
            .await;
        let msg = json!({"content":"在吗","client_msg_id":"01JVVVVVVVVVVVVVVVVVVVVVVG"});
        let mut app = f.app.clone();
        let first_req = req(
            "POST",
            &format!("/v2/comp/group/{}/message/stream", f.group),
            f.user,
            Some(msg.clone()),
        );
        let first = tokio::spawn(async move {
            let resp = app.call(first_req).await.unwrap();
            let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        });
        let mut landed = 0i64;
        for _ in 0..100 {
            landed = sqlx::query_scalar(
                "SELECT count(*) FROM engine.chat_messages WHERE session_id = $1 AND role = 'assistant'",
            )
            .bind(f.group)
            .fetch_one(&f.pool)
            .await
            .unwrap();
            if landed > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(landed, 1, "小美's row landed while 阿哲 is held back");

        let (s, body) = send(&mut f, msg).await;
        assert_eq!(s, StatusCode::CONFLICT, "{body}");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["code"], "duplicate_in_progress", "{body}");
        assert!(body["original_user_message_id"].is_string(), "{body}");

        let first = first.await.unwrap();
        assert!(first.contains("我也在"), "{first}");
        assert!(first.contains(r#""type":"final""#), "{first}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_audit_block_rides_each_speaker_and_is_validated(pool: PgPool) {
        let mut f = fixture(pool, None, 200, None).await;
        wiremock::Mock::given(wiremock::matchers::path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                json!({"model": "chat-model", "user": "u_group"}),
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(&[r#"{"choices":[{"delta":{"content":"嗯嗯"}}]}"#.to_string()]),
                        "text/event-stream",
                    ),
            )
            .with_priority(1)
            .expect(2)
            .mount(&f._server)
            .await;
        let (s, body) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JWWWWWWWWWWWWWWWWWWWWWWG","audit":{"user":"u_group"}}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(body.matches("嗯嗯").count(), 2, "{body}");

        let (s, body) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JXXXXXXXXXXXXXXXXXXXXXXG","audit":{"user":"u".repeat(257)}}),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["code"], "invalid_payload", "{body}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn each_speaker_logs_its_own_prompt(pool: PgPool) {
        let dir = std::env::temp_dir().join(format!("eros-group-promptlog-{}", Uuid::new_v4()));
        let d = dir.clone();
        let mut f = fixture_with(pool, None, 200, None, "", move |s| {
            s.config.prompt_log_dir = Some(d);
        })
        .await;
        let (s, _) = send(
            &mut f,
            json!({"content":"嗨","client_msg_id":"01JSSSSSSSSSSSSSSSSSSSSSSG"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        // The writes are fire-and-forget on the blocking pool; poll for them.
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        for _ in 0..100 {
            files = std::fs::read_dir(&dir)
                .map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).collect())
                .unwrap_or_default();
            if files.len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(files.len(), 2, "one file per speaker: {files:?}");
        let mut speakers: Vec<String> = files
            .iter()
            .map(|p| {
                let text = std::fs::read_to_string(p).unwrap();
                assert!(
                    text.contains(&format!("# session:  {}\n", f.group)),
                    "{text}"
                );
                text.lines()
                    .find_map(|l| l.strip_prefix("# speaker:  "))
                    .unwrap_or_else(|| panic!("no speaker line: {text}"))
                    .to_string()
            })
            .collect();
        speakers.sort();
        let mut want = vec![f.a.to_string(), f.b.to_string()];
        want.sort();
        assert_eq!(speakers, want);
        std::fs::remove_dir_all(&dir).ok();
    }

    const COMPOSE_TASK: &str =
        "[tasks.chat_image_prompt_compose]\nmodel = \"composer-model\"\nfilter_prompt = \"COMPOSE\"\n";

    async fn mount_composer(server: &wiremock::MockServer) {
        wiremock::Mock::given(wiremock::matchers::path("/api/v1/chat/completions"))
            .and(wiremock::matchers::body_partial_json(
                json!({"model": "composer-model"}),
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-compose",
                "model": "composer-model",
                "choices": [{"message": {"content":
                    "{\"prompt\":\"she waves from the summit\",\"caption\":\"山顶挥手\"}"}}],
                "usage": {"total_tokens": 10}
            })))
            .mount(server)
            .await;
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_text_and_image_speaker_sends_the_picture_after_its_reply(pool: PgPool) {
        let verdict = r#"{"speakers":[{"member":"m1","action":"reply_text_image","inner_state":null,"reply_mode":null,"clothing":null}],"reason":null}"#;
        let mut f = fixture_with(
            pool,
            Some(director_sse(verdict)),
            200,
            None,
            COMPOSE_TASK,
            |_| {},
        )
        .await;
        mount_composer(&f._server).await;
        let (s, body) = send(
            &mut f,
            json!({"content":"拍张照","client_msg_id":"01JTTTTTTTTTTTTTTTTTTTTTTG","image":{}}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");

        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}: {body}"))
        };
        let meta = at(&format!(r#""speaker_session_id":"{}""#, f.a));
        let delta = at(r#""type":"delta""#);
        let done = at(r#""type":"done""#);
        let image = at(r#""type":"image_request""#);
        let fin = at(r#""type":"final""#);
        assert!(
            meta < delta && delta < done && done < image && image < fin,
            "{body}"
        );
        let meta_line = body
            .lines()
            .find(|l| l.contains(r#""type":"meta""#))
            .unwrap();
        assert!(
            meta_line.contains(r#""action_type":"reply_text_image""#),
            "the meta frame is a text+image reply: {body}"
        );

        let (speaker, content, has_image): (Option<Uuid>, String, bool) = sqlx::query_as(
            "SELECT speaker_session_id, content, metadata ? 'image' FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant' AND NOT truncated",
        )
        .bind(f.group)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(
            (speaker, content.as_str(), has_image),
            (Some(f.a), "好呀", true)
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_image_only_speaker_sends_meta_done_then_the_picture(pool: PgPool) {
        let verdict = r#"{"speakers":[{"member":"m1","action":"reply_image","inner_state":null,"reply_mode":null,"clothing":null}],"reason":null}"#;
        let mut f = fixture_with(
            pool,
            Some(director_sse(verdict)),
            200,
            None,
            COMPOSE_TASK,
            |_| {},
        )
        .await;
        mount_composer(&f._server).await;
        let (s, body) = send(
            &mut f,
            json!({"content":"拍张照","client_msg_id":"01JUUUUUUUUUUUUUUUUUUUUUUG","image":{}}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");

        let at = |needle: &str| {
            body.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}: {body}"))
        };
        let meta = at(&format!(
            r#""action_type":"reply_image","speaker_session_id":"{}""#,
            f.a
        ));
        let done = at(r#""type":"done""#);
        let image = at(r#""type":"image_request""#);
        let fin = at(r#""type":"final""#);
        assert!(meta < done && done < image && image < fin, "{body}");
        assert!(!body.contains(r#""type":"delta""#), "{body}");

        let (speaker, content, has_image): (Option<Uuid>, String, bool) = sqlx::query_as(
            "SELECT speaker_session_id, content, metadata ? 'image' FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(f.group)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(
            (speaker, content.as_str(), has_image),
            (Some(f.a), "", true)
        );
    }
}

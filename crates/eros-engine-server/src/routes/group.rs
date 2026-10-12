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

use eros_engine_core::group::normalize_title;
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

fn refusal(r: MemberRefusal) -> AppError {
    match r {
        MemberRefusal::NotFound => AppError::NotFound("no such session".into()),
        MemberRefusal::WrongChannel => AppError::Conflict("wrong_channel".into()),
        MemberRefusal::Archived => AppError::Conflict("session_archived".into()),
        MemberRefusal::InstanceAlreadyMember => {
            AppError::Conflict("instance_already_member".into())
        }
        MemberRefusal::GroupFull => AppError::Conflict("group_full".into()),
    }
}

fn title_arg(raw: Option<&str>) -> Result<Option<String>, AppError> {
    match raw {
        None => Ok(None),
        Some(t) => normalize_title(t)
            .map(Some)
            .ok_or_else(|| AppError::Unprocessable("invalid_title".into())),
    }
}

async fn owned(state: &AppState, group_id: Uuid, user_id: Uuid) -> Result<Group, AppError> {
    GroupRepo { pool: &state.pool }
        .get(group_id, user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("no such group".into()))
}

#[utoipa::path(post, path = "/v2/comp/group", tag = "group",
    request_body = CreateGroupRequest,
    responses((status = 200, body = GroupDto), (status = 404), (status = 409), (status = 422)),
    security(("bearer" = [])))]
async fn create_group(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<CreateGroupRequest>,
) -> Result<Json<GroupDto>, AppError> {
    if req.member_session_ids.is_empty() {
        return Err(AppError::Unprocessable(
            "member_session_ids must not be empty".into(),
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
    responses((status = 200, body = GroupDto), (status = 404)), security(("bearer" = [])))]
async fn get_group(
    State(state): State<AppState>,
    Path(group_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<GroupDto>, AppError> {
    Ok(Json(group_dto(&owned(&state, group_id, user_id).await?)))
}

#[utoipa::path(patch, path = "/v2/comp/group/{group_id}", tag = "group",
    params(("group_id" = Uuid, Path)), request_body = RenameGroupRequest,
    responses((status = 200, body = GroupDto), (status = 404), (status = 422)),
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
        return Err(AppError::NotFound("no such group".into()));
    }
    Ok(Json(group_dto(&owned(&state, group_id, user_id).await?)))
}

#[utoipa::path(post, path = "/v2/comp/group/{group_id}/members", tag = "group",
    params(("group_id" = Uuid, Path)), request_body = AddMemberRequest,
    responses((status = 200, body = GroupDto), (status = 404), (status = 409)),
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
    responses((status = 204), (status = 404)), security(("bearer" = [])))]
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
        return Err(AppError::NotFound("not a member".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(delete, path = "/v2/comp/group/{group_id}", tag = "group",
    params(("group_id" = Uuid, Path)),
    responses((status = 204), (status = 404)), security(("bearer" = [])))]
async fn archive_group(
    State(state): State<AppState>,
    Path(group_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<StatusCode, AppError> {
    let repo = GroupRepo { pool: &state.pool };
    if !repo.archive(group_id, user_id).await? {
        return Err(AppError::NotFound("no such group".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_group))
        .routes(routes!(list_groups))
        .routes(routes!(get_group, rename_group, archive_group))
        .routes(routes!(add_member))
        .routes(routes!(remove_member))
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
        let (s, _) = send_request(
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
        assert!(
            body["message"].as_str().unwrap().contains("group_full"),
            "{body}"
        );
        let (s, _) = send_request(
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
            assert!(body["message"].as_str().unwrap().contains("invalid_title"));
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
            let (s, _) = send_request(&mut app, r).await;
            assert_eq!(s, StatusCode::NOT_FOUND);
        }
        let (s, body) = send_request(&mut app, req("GET", &g, owner, None)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["title"], "keep");
        assert_eq!(body["members"].as_array().unwrap().len(), 1);
    }
}

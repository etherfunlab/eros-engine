// SPDX-License-Identifier: AGPL-3.0-only
//! World Town feed routes (town spec §4). Same JWT contract as
//! `companion.rs`: the path `user_id` MUST equal the JWT `sub`.
//! Rendering is downstream's job — these endpoints only move data.

use axum::{
    extract::{Extension, Path, Query, State},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::world_town::{FeedComment, WorldTownRepo};

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::state::AppState;

const FEED_LIMIT_DEFAULT: i64 = 20;
const FEED_LIMIT_MAX: i64 = 50;
const COMMENT_MAX_CHARS: usize = 1000;

#[derive(Debug, Deserialize)]
pub struct FeedQuery {
    pub limit: Option<i64>,
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct FeedCommentDto {
    pub comment_id: Uuid,
    /// NULL = the user themselves.
    pub author_instance_id: Option<Uuid>,
    pub author_name: Option<String>,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct FeedPostDto {
    pub post_id: Uuid,
    pub instance_id: Uuid,
    pub author_name: String,
    pub content: String,
    pub published_at: DateTime<Utc>,
    pub comments: Vec<FeedCommentDto>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct FeedResponse {
    pub user_id: Uuid,
    pub posts: Vec<FeedPostDto>,
    /// Present when another page may exist; feed it back as `cursor`.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct CreateCommentRequest {
    pub content: String,
}

fn comment_dto(c: FeedComment) -> FeedCommentDto {
    FeedCommentDto {
        comment_id: c.comment_id,
        author_instance_id: c.author_instance_id,
        author_name: c.author_name,
        content: c.content,
        created_at: c.created_at,
    }
}

/// Cursor wire format: `<published_at RFC3339>|<post uuid>`.
fn parse_cursor(raw: &str) -> Option<(DateTime<Utc>, Uuid)> {
    let (ts, id) = raw.rsplit_once('|')?;
    let ts = DateTime::parse_from_rfc3339(ts).ok()?.with_timezone(&Utc);
    let id = Uuid::parse_str(id).ok()?;
    Some((ts, id))
}

/// Published town feed for the user's world, newest first. Unenrolled or
/// town-disabled users get an empty feed, not an error (spec §4).
#[utoipa::path(
    get,
    path = "/world/town/{user_id}/feed",
    tag = "world_town",
    params(
        ("user_id" = Uuid, Path, description = "Owner user id (must equal JWT sub)"),
        ("limit" = Option<i64>, Query, description = "Page size, default 20, max 50"),
        ("cursor" = Option<String>, Query,
            description = "Opaque keyset cursor from the previous page's next_cursor")
    ),
    responses(
        (status = 200, body = FeedResponse),
        (status = 400, description = "malformed cursor"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "user_id does not match JWT")
    ),
    security(("bearer" = []))
)]
async fn get_feed(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    Query(q): Query<FeedQuery>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<Json<FeedResponse>, AppError> {
    if user_id != jwt_user {
        return Err(AppError::Forbidden("not your data".into()));
    }
    let limit = q
        .limit
        .unwrap_or(FEED_LIMIT_DEFAULT)
        .clamp(1, FEED_LIMIT_MAX);
    let cursor = match q.cursor.as_deref() {
        None => None,
        Some(raw) => {
            Some(parse_cursor(raw).ok_or_else(|| AppError::BadRequest("malformed cursor".into()))?)
        }
    };
    let repo = WorldTownRepo { pool: &state.pool };
    let posts = repo.feed_page(user_id, limit, cursor).await?;
    let next_cursor = (posts.len() as i64 == limit).then(|| {
        let last = posts.last().expect("non-empty when len == limit");
        format!("{}|{}", last.published_at.to_rfc3339(), last.post_id)
    });
    let ids: Vec<Uuid> = posts.iter().map(|p| p.post_id).collect();
    let mut comments = repo.list_comments_for_posts(&ids).await?;
    let posts = posts
        .into_iter()
        .map(|p| {
            let thread: Vec<FeedCommentDto> = comments
                .extract_if(.., |c| c.post_id == p.post_id)
                .map(comment_dto)
                .collect();
            FeedPostDto {
                post_id: p.post_id,
                instance_id: p.instance_id,
                author_name: p.author_name,
                content: p.content,
                published_at: p.published_at,
                comments: thread,
            }
        })
        .collect();
    Ok(Json(FeedResponse {
        user_id,
        posts,
        next_cursor,
    }))
}

/// Add a user comment to a published post in the user's own town.
#[utoipa::path(
    post,
    path = "/world/town/{user_id}/posts/{post_id}/comments",
    tag = "world_town",
    params(
        ("user_id" = Uuid, Path, description = "Owner user id (must equal JWT sub)"),
        ("post_id" = Uuid, Path, description = "Target post id")
    ),
    request_body = CreateCommentRequest,
    responses(
        (status = 200, body = FeedCommentDto),
        (status = 400, description = "empty content or over 1000 chars"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "user_id does not match JWT"),
        (status = 404, description = "post not visible to this user")
    ),
    security(("bearer" = []))
)]
async fn create_comment(
    State(state): State<AppState>,
    Path((user_id, post_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
    Json(body): Json<CreateCommentRequest>,
) -> Result<Json<FeedCommentDto>, AppError> {
    if user_id != jwt_user {
        return Err(AppError::Forbidden("not your data".into()));
    }
    let content = body.content.trim();
    if content.is_empty() {
        return Err(AppError::BadRequest("content is empty".into()));
    }
    if content.chars().count() > COMMENT_MAX_CHARS {
        return Err(AppError::BadRequest(format!(
            "content exceeds {COMMENT_MAX_CHARS} chars"
        )));
    }
    let repo = WorldTownRepo { pool: &state.pool };
    let created = repo
        .insert_user_comment(user_id, post_id, content)
        .await?
        .ok_or_else(|| AppError::NotFound("post not found".into()))?;
    Ok(Json(comment_dto(created)))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_feed))
        .routes(routes!(create_comment))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        build_router, mint_test_jwt, seed_genome, seed_instance, send_request,
    };
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
    };
    use sqlx::PgPool;

    async fn seed_town(pool: &PgPool, owner: Uuid) -> Uuid {
        let genome = seed_genome(pool, "Aria").await;
        let inst = seed_instance(pool, genome, owner).await;
        sqlx::query(
            "INSERT INTO engine.world_enrollments (owner_uid, town_enabled) VALUES ($1, true)",
        )
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
        inst
    }

    async fn seed_published_post(pool: &PgPool, owner: Uuid, inst: Uuid, n: i32) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.world_posts \
                 (owner_uid, instance_id, content, scheduled_at, published_at) \
             VALUES ($1, $2, 'post ' || $3::text, now(), \
                     now() - ($3::text || ' minutes')::interval) \
             RETURNING id",
        )
        .bind(owner)
        .bind(inst)
        .bind(n)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn feed_requires_matching_jwt(pool: PgPool) {
        let owner = Uuid::new_v4();
        let mut app = build_router(test_state(pool));
        let req = Request::builder()
            .uri(format!("/world/town/{owner}/feed"))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", mint_test_jwt(Uuid::new_v4())),
            )
            .body(Body::empty())
            .unwrap();
        let (status, _) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn feed_pages_with_cursor_and_embeds_comments(pool: PgPool) {
        let owner = Uuid::new_v4();
        let inst = seed_town(&pool, owner).await;
        for n in 0..3 {
            seed_published_post(&pool, owner, inst, n).await;
        }
        let first = seed_published_post(&pool, owner, inst, 99).await; // oldest
        sqlx::query(
            "INSERT INTO engine.world_post_comments (post_id, author_instance_id, source, content) \
             VALUES ($1, NULL, NULL, 'hi')",
        )
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();

        let mut app = build_router(test_state(pool));
        let jwt = mint_test_jwt(owner);
        let req = Request::builder()
            .uri(format!("/world/town/{owner}/feed?limit=2"))
            .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
            .body(Body::empty())
            .unwrap();
        let (status, body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["posts"].as_array().unwrap().len(), 2);
        assert_eq!(body["posts"][0]["content"], "post 0", "newest first");
        assert_eq!(body["posts"][0]["author_name"], "Aria");
        let cursor = body["next_cursor"].as_str().expect("full page has cursor");

        // No `urlencoding` dependency in this crate — percent-encode the
        // three cursor separators (`+`, `:`, `|`) inline (RFC3339 offset
        // uses `+`, the timestamp uses `:`, and the wire format uses `|`).
        let encoded_cursor = cursor
            .replace('+', "%2B")
            .replace(':', "%3A")
            .replace('|', "%7C");
        let req = Request::builder()
            .uri(format!(
                "/world/town/{owner}/feed?limit=2&cursor={encoded_cursor}"
            ))
            .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
            .body(Body::empty())
            .unwrap();
        let (status, body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK);
        let page2 = body["posts"].as_array().unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[1]["content"], "post 99");
        assert_eq!(page2[1]["comments"][0]["content"], "hi");
        assert!(page2[1]["comments"][0]["author_instance_id"].is_null());

        // Malformed cursor ⇒ 400.
        let req = Request::builder()
            .uri(format!("/world/town/{owner}/feed?cursor=garbage"))
            .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
            .body(Body::empty())
            .unwrap();
        let (status, _) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn feed_empty_for_unenrolled_user(pool: PgPool) {
        let owner = Uuid::new_v4();
        let mut app = build_router(test_state(pool));
        let req = Request::builder()
            .uri(format!("/world/town/{owner}/feed"))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", mint_test_jwt(owner)),
            )
            .body(Body::empty())
            .unwrap();
        let (status, body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["posts"].as_array().unwrap().is_empty());
        assert!(body["next_cursor"].is_null());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn comment_validates_length_and_visibility(pool: PgPool) {
        let owner = Uuid::new_v4();
        let inst = seed_town(&pool, owner).await;
        let post = seed_published_post(&pool, owner, inst, 0).await;
        let mut app = build_router(test_state(pool.clone()));
        let jwt = mint_test_jwt(owner);

        let post_req = |uri: String, body: serde_json::Value, jwt: &str| {
            Request::builder()
                .method("POST")
                .uri(uri)
                .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };

        // Happy path returns the created comment.
        let req = post_req(
            format!("/world/town/{owner}/posts/{post}/comments"),
            serde_json::json!({"content": "好看！"}),
            &jwt,
        );
        let (status, body) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["content"], "好看！");
        assert!(body["author_instance_id"].is_null());

        // Over 1000 chars ⇒ 400.
        let req = post_req(
            format!("/world/town/{owner}/posts/{post}/comments"),
            serde_json::json!({"content": "字".repeat(1001)}),
            &jwt,
        );
        let (status, _) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Foreign post ⇒ 404.
        let stranger = Uuid::new_v4();
        let s_inst = seed_town(&pool, stranger).await;
        let s_post = seed_published_post(&pool, stranger, s_inst, 0).await;
        let req = post_req(
            format!("/world/town/{owner}/posts/{s_post}/comments"),
            serde_json::json!({"content": "x"}),
            &jwt,
        );
        let (status, _) = send_request(&mut app, req).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

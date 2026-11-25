// SPDX-License-Identifier: AGPL-3.0-only
//! The owner's ledger facts and extracted profile memories: list them, and
//! delete one at a time (spec
//! 2026-11-10-row-deletion-and-deletion-stream-design.md §3); and the facts
//! as fact groups, listed and deleted a group at a time (spec
//! 2026-11-25-fact-groups-design.md §5.4–5.5).

use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::human_fact::{FactGroup, HumanFactRepo, HumanFactRow};
use eros_engine_store::memory::{MemoryRepo, ProfileMemoryChange};

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;

/// A page position: the timestamp the list is ordered by, and the row id.
type Cursor = (DateTime<Utc>, Uuid);

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct OwnedRowsQuery {
    /// Default 100, clamped to 1..=500.
    pub limit: Option<i64>,
    /// RFC 3339; with `before_id`, the `next` of the previous page. Absent:
    /// start at the newest row.
    pub before: Option<DateTime<Utc>>,
    /// Comes with `before`.
    pub before_id: Option<Uuid>,
}

impl OwnedRowsQuery {
    /// The page size and the cursor; half a cursor is a 400.
    fn page(&self) -> Result<(i64, Option<Cursor>), AppError> {
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        match (self.before, self.before_id) {
            (Some(at), Some(id)) => Ok((limit, Some((at, id)))),
            (None, None) => Ok((limit, None)),
            _ => Err(AppError::BadRequest(
                "before and before_id come together".into(),
            )),
        }
    }
}

fn owner_only(user_id: Uuid, jwt_user: Uuid) -> Result<(), AppError> {
    if user_id != jwt_user {
        return Err(AppError::Forbidden("not your data".into()));
    }
    Ok(())
}

/// One ledger fact, as `GET /svc/v1/facts/changes` exports it, without the
/// owner's id.
#[derive(Serialize, ToSchema)]
pub struct OwnedFact {
    pub id: Uuid,
    pub content: String,
    pub category: String,
    pub domain: String,
    pub evidence_type: String,
    pub temporality: String,
    pub persistence: String,
    pub confidence: String,
    pub occurrences: i32,
    pub session_count: i32,
    /// `once`, `repeated_in_session` or `repeated_across_sessions`.
    pub recurrence: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    /// The fact's group, shared with the user's facts that state the same
    /// thing. A fact not grouped yet is a group of its own, whose id changes
    /// if it later joins another group.
    pub group_id: Uuid,
}

impl From<HumanFactRow> for OwnedFact {
    fn from(r: HumanFactRow) -> Self {
        let recurrence = r.recurrence().as_str().to_string();
        Self {
            id: r.id,
            content: r.content,
            category: r.category,
            domain: r.domain,
            evidence_type: r.evidence_type,
            temporality: r.temporality,
            persistence: r.persistence,
            confidence: r.confidence,
            occurrences: r.occurrences,
            session_count: r.session_count,
            recurrence,
            first_seen_at: r.first_seen_at,
            last_seen_at: r.last_seen_at,
            group_id: r.group_id,
        }
    }
}

/// One fact group: the user's facts that state the same thing, as one item.
#[derive(Serialize, ToSchema)]
pub struct OwnedFactGroup {
    pub group_id: Uuid,
    /// The representative's text: the member seen most often, ties going to
    /// the earliest.
    pub content: String,
    /// Whether the group's facts are roleplay expressions; a group never
    /// mixes roleplay with real facts.
    pub roleplay: bool,
    /// Summed over the members.
    pub occurrences: i32,
    /// Summed over the members, so a session that saw two of them counts twice.
    pub session_count: i32,
    /// `once`, `repeated_in_session` or `repeated_across_sessions`, from the
    /// summed counters.
    pub recurrence: String,
    /// The earliest member's.
    pub first_seen_at: DateTime<Utc>,
    /// The latest member's.
    pub last_seen_at: DateTime<Utc>,
    /// Every member, the representative first.
    pub facts: Vec<OwnedFact>,
}

impl From<FactGroup> for OwnedFactGroup {
    fn from(g: FactGroup) -> Self {
        Self {
            group_id: g.group_id,
            content: g.facts[0].content.clone(),
            roleplay: g.roleplay(),
            occurrences: g.occurrences(),
            session_count: g.session_count(),
            recurrence: g.recurrence().as_str().to_string(),
            first_seen_at: g.first_seen_at(),
            last_seen_at: g.last_seen_at(),
            facts: g.facts.into_iter().map(OwnedFact::from).collect(),
        }
    }
}

/// Pass `first_seen_at` back as `before` and `group_id` as `before_id`.
#[derive(Serialize, ToSchema)]
pub struct OwnedFactGroupsCursor {
    pub first_seen_at: DateTime<Utc>,
    pub group_id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct OwnedFactGroupsResponse {
    pub groups: Vec<OwnedFactGroup>,
    /// Present iff the page is full; `null` means the walk is complete.
    pub next: Option<OwnedFactGroupsCursor>,
}

/// Pass both fields back as `before` + `before_id`.
#[derive(Serialize, ToSchema)]
pub struct OwnedFactsCursor {
    pub first_seen_at: DateTime<Utc>,
    pub id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct OwnedFactsResponse {
    pub facts: Vec<OwnedFact>,
    /// Present iff the page is full; `null` means the walk is complete.
    pub next: Option<OwnedFactsCursor>,
}

/// One extracted profile memory, as `GET /svc/v1/memories/changes` exports
/// it, without the owner's id.
#[derive(Serialize, ToSchema)]
pub struct OwnedMemory {
    pub id: Uuid,
    pub content: String,
    pub category: String,
    /// The extraction metadata, verbatim; `null` when the extraction wrote none.
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

impl From<ProfileMemoryChange> for OwnedMemory {
    fn from(r: ProfileMemoryChange) -> Self {
        Self {
            id: r.id,
            content: r.content,
            category: r.category,
            metadata: r.metadata,
            created_at: r.created_at,
        }
    }
}

/// Pass both fields back as `before` + `before_id`.
#[derive(Serialize, ToSchema)]
pub struct OwnedMemoriesCursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct OwnedMemoriesResponse {
    pub memories: Vec<OwnedMemory>,
    /// Present iff the page is full; `null` means the walk is complete.
    pub next: Option<OwnedMemoriesCursor>,
}

/// The user's ledger facts, newest first by when each was first seen. These
/// are the rows `GET /svc/v1/facts/changes` exports.
#[utoipa::path(
    get,
    path = "/v2/comp/user/{user_id}/facts",
    tag = "companion",
    params(("user_id" = Uuid, Path, description = "Owner user id (must equal JWT sub)"), OwnedRowsQuery),
    responses(
        (status = 200, body = OwnedFactsResponse),
        (status = 400, description = "`before` without `before_id`, or the reverse"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "user_id does not match JWT")
    ),
    security(("bearer" = []))
)]
async fn list_facts(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
    Query(q): Query<OwnedRowsQuery>,
) -> Result<Json<OwnedFactsResponse>, AppError> {
    owner_only(user_id, jwt_user)?;
    let (limit, before) = q.page()?;
    let rows = HumanFactRepo { pool: &state.pool }
        .list_owned(user_id, before, limit)
        .await?;
    let next = if rows.len() as i64 == limit {
        rows.last().map(|r| OwnedFactsCursor {
            first_seen_at: r.first_seen_at,
            id: r.id,
        })
    } else {
        None
    };
    Ok(Json(OwnedFactsResponse {
        facts: rows.into_iter().map(OwnedFact::from).collect(),
        next,
    }))
}

/// The user's ledger facts as fact groups, one item per group with the facts
/// behind it, newest first by the group's earliest fact. A group's
/// representative is the member seen most often (ties to the earliest), and
/// its counters are sums over the members. A fact not grouped yet is a group
/// of its own. Page with `before` = `next.first_seen_at` and `before_id` =
/// `next.group_id`.
#[utoipa::path(
    get,
    path = "/v2/comp/user/{user_id}/fact-groups",
    tag = "companion",
    params(("user_id" = Uuid, Path, description = "Owner user id (must equal JWT sub)"), OwnedRowsQuery),
    responses(
        (status = 200, body = OwnedFactGroupsResponse),
        (status = 400, description = "`before` without `before_id`, or the reverse"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "user_id does not match JWT")
    ),
    security(("bearer" = []))
)]
async fn list_fact_groups(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
    Query(q): Query<OwnedRowsQuery>,
) -> Result<Json<OwnedFactGroupsResponse>, AppError> {
    owner_only(user_id, jwt_user)?;
    let (limit, before) = q.page()?;
    let groups = HumanFactRepo { pool: &state.pool }
        .list_groups_owned(user_id, before, limit)
        .await?;
    let next = if groups.len() as i64 == limit {
        groups.last().map(|g| OwnedFactGroupsCursor {
            first_seen_at: g.first_seen_at(),
            group_id: g.group_id,
        })
    } else {
        None
    };
    Ok(Json(OwnedFactGroupsResponse {
        groups: groups.into_iter().map(OwnedFactGroup::from).collect(),
        next,
    }))
}

/// The user's extracted profile memories, newest first. These are the rows
/// `GET /svc/v1/memories/changes` exports.
#[utoipa::path(
    get,
    path = "/v2/comp/user/{user_id}/memories",
    tag = "companion",
    params(("user_id" = Uuid, Path, description = "Owner user id (must equal JWT sub)"), OwnedRowsQuery),
    responses(
        (status = 200, body = OwnedMemoriesResponse),
        (status = 400, description = "`before` without `before_id`, or the reverse"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "user_id does not match JWT")
    ),
    security(("bearer" = []))
)]
async fn list_memories(
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
    Query(q): Query<OwnedRowsQuery>,
) -> Result<Json<OwnedMemoriesResponse>, AppError> {
    owner_only(user_id, jwt_user)?;
    let (limit, before) = q.page()?;
    let rows = MemoryRepo { pool: &state.pool }
        .list_profile_owned(user_id, before, limit)
        .await?;
    let next = if rows.len() as i64 == limit {
        rows.last().map(|r| OwnedMemoriesCursor {
            created_at: r.created_at,
            id: r.id,
        })
    } else {
        None
    };
    Ok(Json(OwnedMemoriesResponse {
        memories: rows.into_iter().map(OwnedMemory::from).collect(),
        next,
    }))
}

/// Delete one ledger fact. Its tombstone goes to `GET /svc/v1/rows/deletions`,
/// and the derived profile is rebuilt without it on the next sweep. The fact's
/// wording in the extraction audit and in past profile snapshots stays until
/// the user is erased.
#[utoipa::path(
    delete,
    path = "/v2/comp/user/{user_id}/facts/{fact_id}",
    tag = "companion",
    params(
        ("user_id" = Uuid, Path, description = "Owner user id (must equal JWT sub)"),
        ("fact_id" = Uuid, Path, description = "The fact's id")
    ),
    responses(
        (status = 204, description = "deleted"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "user_id does not match JWT"),
        (status = 404, description = "no such fact of this user (including one already deleted)")
    ),
    security(("bearer" = []))
)]
async fn delete_fact(
    State(state): State<AppState>,
    Path((user_id, fact_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<StatusCode, AppError> {
    owner_only(user_id, jwt_user)?;
    if (HumanFactRepo { pool: &state.pool })
        .delete_owned(user_id, fact_id)
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("fact not found".into()))
    }
}

/// Delete a fact group: every fact in it as the call runs, including one
/// grouped after the listing was read. Each fact's tombstone goes to
/// `GET /svc/v1/rows/deletions`, and the derived profile is rebuilt without
/// them on the next sweep, as for a single fact.
#[utoipa::path(
    delete,
    path = "/v2/comp/user/{user_id}/fact-groups/{group_id}",
    tag = "companion",
    params(
        ("user_id" = Uuid, Path, description = "Owner user id (must equal JWT sub)"),
        ("group_id" = Uuid, Path, description = "The group's id")
    ),
    responses(
        (status = 204, description = "deleted"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "user_id does not match JWT"),
        (status = 404, description = "no fact of this user has this group id (including a group \
            already deleted, or a one-fact group whose fact has since joined another group)")
    ),
    security(("bearer" = []))
)]
async fn delete_fact_group(
    State(state): State<AppState>,
    Path((user_id, group_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<StatusCode, AppError> {
    owner_only(user_id, jwt_user)?;
    if (HumanFactRepo { pool: &state.pool })
        .delete_group_owned(user_id, group_id)
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("fact group not found".into()))
    }
}

/// Delete one extracted profile memory. Its tombstone goes to
/// `GET /svc/v1/rows/deletions`.
#[utoipa::path(
    delete,
    path = "/v2/comp/user/{user_id}/memories/{memory_id}",
    tag = "companion",
    params(
        ("user_id" = Uuid, Path, description = "Owner user id (must equal JWT sub)"),
        ("memory_id" = Uuid, Path, description = "The memory's id")
    ),
    responses(
        (status = 204, description = "deleted"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "user_id does not match JWT"),
        (status = 404, description = "no such extracted profile memory of this user \
            (including one already deleted)")
    ),
    security(("bearer" = []))
)]
async fn delete_memory(
    State(state): State<AppState>,
    Path((user_id, memory_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<StatusCode, AppError> {
    owner_only(user_id, jwt_user)?;
    if (MemoryRepo { pool: &state.pool })
        .delete_profile_owned(user_id, memory_id)
        .await?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("memory not found".into()))
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_facts))
        .routes(routes!(list_fact_groups))
        .routes(routes!(list_memories))
        .routes(routes!(delete_fact))
        .routes(routes!(delete_fact_group))
        .routes(routes!(delete_memory))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Method, Request, StatusCode};
    use axum::Router;
    use chrono::{DateTime, TimeZone, Utc};
    use serde_json::Value;
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{build_router, mint_test_jwt};

    async fn call(app: &Router, method: Method, uri: &str, as_user: Uuid) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", mint_test_jwt(as_user)),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    async fn seed_fact(
        pool: &PgPool,
        user: Uuid,
        content: &str,
        first_seen: DateTime<Utc>,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.human_facts \
               (user_id, content, category, domain, evidence_type, temporality, \
                persistence, confidence, first_seen_at, last_seen_at) \
             VALUES ($1, $2, 'other', 'career', 'explicit_statement', 'current', 'stable', \
                     'high', $3, $3) RETURNING id",
        )
        .bind(user)
        .bind(content)
        .bind(first_seen)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// An extracted profile memory created at `at`.
    async fn seed_memory(pool: &PgPool, user: Uuid, content: &str, at: DateTime<Utc>) -> Uuid {
        let session: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id) VALUES ($1) RETURNING id",
        )
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap();
        let mut embedding = vec![0.0_f32; 512];
        embedding[0] = 1.0;
        let id = eros_engine_store::memory::MemoryRepo { pool }
            .upsert(
                eros_engine_store::memory::MemoryLayer::Profile,
                session,
                user,
                None,
                content,
                &embedding,
                Some("fact"),
                None,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE engine.companion_memories SET created_at = $2 WHERE id = $1")
            .bind(id)
            .bind(at)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    fn t(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 10, h, 0, 0).unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn facts_list_pages_newest_first_for_the_owner(pool: PgPool) {
        let (user, other) = (Uuid::new_v4(), Uuid::new_v4());
        let old = seed_fact(&pool, user, "是工程师", t(1)).await;
        let new = seed_fact(&pool, user, "喜欢爬山", t(2)).await;
        seed_fact(&pool, other, "别人的事", t(3)).await;
        let app = build_router(test_state(pool.clone()));

        let (status, body) = call(
            &app,
            Method::GET,
            &format!("/v2/comp/user/{user}/facts?limit=1"),
            user,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["facts"].as_array().unwrap().len(), 1);
        assert_eq!(body["facts"][0]["id"], new.to_string());
        assert_eq!(body["facts"][0]["content"], "喜欢爬山");
        assert_eq!(body["facts"][0]["recurrence"], "once");
        assert!(body["facts"][0].get("user_id").is_none());
        let next = &body["next"];
        assert_eq!(next["id"], new.to_string());

        let uri = format!(
            "/v2/comp/user/{user}/facts?limit=1&before={}&before_id={}",
            urlencoding(next["first_seen_at"].as_str().unwrap()),
            next["id"].as_str().unwrap()
        );
        let (status, body) = call(&app, Method::GET, &uri, user).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["facts"][0]["id"], old.to_string());
        assert_eq!(
            body["next"]["id"],
            old.to_string(),
            "a full page has a next"
        );

        let (status, body) = call(
            &app,
            Method::GET,
            &format!("/v2/comp/user/{user}/facts"),
            user,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["facts"].as_array().unwrap().len(), 2);
        assert!(body["next"].is_null(), "a short page ends the walk");
    }

    fn urlencoding(s: &str) -> String {
        s.replace('+', "%2B").replace(':', "%3A")
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn memories_list_pages_newest_first_for_the_owner(pool: PgPool) {
        let user = Uuid::new_v4();
        let old = seed_memory(&pool, user, "养了一只猫", t(1)).await;
        let new = seed_memory(&pool, user, "喜欢爬山", t(2)).await;
        let app = build_router(test_state(pool.clone()));

        let (status, body) = call(
            &app,
            Method::GET,
            &format!("/v2/comp/user/{user}/memories"),
            user,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let ids: Vec<&str> = body["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![new.to_string(), old.to_string()]);
        assert_eq!(body["memories"][0]["category"], "fact");
        assert!(body["next"].is_null());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn lists_refuse_another_user_and_half_a_cursor(pool: PgPool) {
        let user = Uuid::new_v4();
        let app = build_router(test_state(pool.clone()));
        for kind in ["facts", "memories", "fact-groups"] {
            let (status, _) = call(
                &app,
                Method::GET,
                &format!("/v2/comp/user/{user}/{kind}"),
                Uuid::new_v4(),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{kind}");
            let (status, _) = call(
                &app,
                Method::GET,
                &format!("/v2/comp/user/{user}/{kind}?before=2026-11-10T08%3A00%3A00Z"),
                user,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{kind}: before without before_id"
            );
            let (status, _) = call(
                &app,
                Method::GET,
                &format!("/v2/comp/user/{user}/{kind}?before_id={}", Uuid::new_v4()),
                user,
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{kind}: before_id without before"
            );
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn deletes_are_204_once_then_404_and_owner_only(pool: PgPool) {
        let (user, other) = (Uuid::new_v4(), Uuid::new_v4());
        let fact = seed_fact(&pool, user, "是工程师", t(1)).await;
        let memory = seed_memory(&pool, user, "养了一只猫", t(1)).await;
        let theirs = seed_fact(&pool, other, "别人的事", t(1)).await;
        let app = build_router(test_state(pool.clone()));

        for (kind, id) in [("facts", fact), ("memories", memory)] {
            let uri = format!("/v2/comp/user/{user}/{kind}/{id}");
            let (status, _) = call(&app, Method::DELETE, &uri, other).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{kind}: another user's token"
            );
            let (status, _) = call(&app, Method::DELETE, &uri, user).await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{kind}");
            let (status, _) = call(&app, Method::DELETE, &uri, user).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{kind}: already gone");
        }
        let (status, _) = call(
            &app,
            Method::DELETE,
            &format!("/v2/comp/user/{user}/facts/{theirs}"),
            user,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "not the caller's row");

        let tombs: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.row_deletions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tombs, 2);
    }

    /// Put `ids` into one fact group and return its id.
    async fn group_up(pool: &PgPool, ids: &[Uuid]) -> Uuid {
        let group = Uuid::new_v4();
        sqlx::query("UPDATE engine.human_facts SET group_id = $2 WHERE id = ANY($1)")
            .bind(ids)
            .bind(group)
            .execute(pool)
            .await
            .unwrap();
        group
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn fact_groups_list_one_item_per_group_with_members(pool: PgPool) {
        let user = Uuid::new_v4();
        let a = seed_fact(&pool, user, "是工程师", t(1)).await;
        let b = seed_fact(&pool, user, "是一名工程师", t(2)).await;
        seed_fact(&pool, user, "喜欢爬山", t(3)).await;
        let group = group_up(&pool, &[a, b]).await;
        let app = build_router(test_state(pool.clone()));

        let (status, body) = call(
            &app,
            Method::GET,
            &format!("/v2/comp/user/{user}/fact-groups"),
            user,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let groups = body["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        let g = &groups[1];
        assert_eq!(g["group_id"], group.to_string());
        assert_eq!(g["facts"].as_array().unwrap().len(), 2);
        assert_eq!(g["content"], g["facts"][0]["content"]);
        assert_eq!(g["content"], "是工程师", "ties go to the earliest");
        assert_eq!(g["occurrences"], 2);
        assert_eq!(g["session_count"], 2);
        assert_eq!(g["recurrence"], "repeated_across_sessions");
        assert_eq!(g["roleplay"], false);
        assert_eq!(g["facts"][1]["group_id"], group.to_string());
        assert!(!body.to_string().contains("user_id"));
        assert!(body["next"].is_null());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn fact_groups_list_pages_with_a_group_cursor(pool: PgPool) {
        let user = Uuid::new_v4();
        for (i, content) in ["是工程师", "喜欢爬山", "养了一只猫"].iter().enumerate() {
            seed_fact(&pool, user, content, t(i as u32 + 1)).await;
        }
        let app = build_router(test_state(pool.clone()));

        let mut seen = Vec::new();
        let mut uri = format!("/v2/comp/user/{user}/fact-groups?limit=1");
        loop {
            let (status, body) = call(&app, Method::GET, &uri, user).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            for g in body["groups"].as_array().unwrap() {
                seen.push(g["content"].as_str().unwrap().to_string());
            }
            let next = &body["next"];
            if next.is_null() {
                break;
            }
            uri = format!(
                "/v2/comp/user/{user}/fact-groups?limit=1&before={}&before_id={}",
                urlencoding(next["first_seen_at"].as_str().unwrap()),
                next["group_id"].as_str().unwrap()
            );
        }
        assert_eq!(seen, vec!["养了一只猫", "喜欢爬山", "是工程师"]);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn fact_group_delete_is_204_then_404_and_owner_only(pool: PgPool) {
        let (user, other) = (Uuid::new_v4(), Uuid::new_v4());
        let a = seed_fact(&pool, user, "是工程师", t(1)).await;
        let b = seed_fact(&pool, user, "是一名工程师", t(2)).await;
        seed_fact(&pool, user, "喜欢爬山", t(3)).await;
        let group = group_up(&pool, &[a, b]).await;
        let app = build_router(test_state(pool.clone()));
        let uri = format!("/v2/comp/user/{user}/fact-groups/{group}");

        let (status, _) = call(&app, Method::DELETE, &uri, other).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = call(&app, Method::DELETE, &uri, user).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = call(&app, Method::DELETE, &uri, user).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "already gone");

        let (_, body) = call(
            &app,
            Method::GET,
            &format!("/v2/comp/user/{user}/fact-groups"),
            user,
        )
        .await;
        let groups = body["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["content"], "喜欢爬山");
        let tombs: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.row_deletions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tombs, 2, "one tombstone per member");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn flat_facts_list_carries_group_id(pool: PgPool) {
        let user = Uuid::new_v4();
        let a = seed_fact(&pool, user, "是工程师", t(1)).await;
        let group = group_up(&pool, &[a]).await;
        let app = build_router(test_state(pool.clone()));
        let (status, body) = call(
            &app,
            Method::GET,
            &format!("/v2/comp/user/{user}/facts"),
            user,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["facts"][0]["group_id"], group.to_string());
    }
}

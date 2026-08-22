// SPDX-License-Identifier: AGPL-3.0-only
//! `/v2/comp/instance/{instance_id}/insight/{character,user}` — the two sides
//! of one relationship's conversation-derived profile.
//!
//! First endpoints written under the v2 API convention (spec §4): the path
//! segment before the id names the entity the id belongs to, and `insight`
//! replaces v1's overloaded `profile`. The v1 endpoint
//! `GET /comp/instance/{instance_id}/profile` is frozen and unaffected.

use axum::extract::{Extension, Path, State};
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::character_insight::{CharacterInsightRepo, CharacterInsightsRow};
use eros_engine_store::persona::PersonaRepo;
use eros_engine_store::user_insight::{UserInsightRepo, UserInsightsRow};

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::state::AppState;

/// The AI character's side of one relationship's profile.
///
/// Deliberately NOT the v1 `CharacterProfileResponse`, which carries the same
/// fields: that type is the frozen response of `GET
/// /comp/instance/{instance_id}/profile` and stays exactly as shipped. Two
/// structs is the price of freezing v1, and what lets this pair diverge later.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct CharacterInsightResponse {
    #[schema(value_type = String)]
    pub instance_id: Uuid,
    pub location: Option<String>,
    pub occupation: Option<String>,
    pub current_situation: Option<String>,
    pub desires: Option<String>,
    pub vulnerabilities: Option<String>,
    pub habits: Option<String>,
    pub personal_values: Option<String>,
    pub likes: Vec<String>,
    pub dislikes: Vec<String>,
    pub relationships: Vec<String>,
    /// `null` when the character has no insights row yet.
    pub updated_at: Option<DateTime<Utc>>,
}

impl CharacterInsightResponse {
    fn from_row(instance_id: Uuid, row: Option<CharacterInsightsRow>) -> Self {
        match row {
            Some(r) => Self {
                instance_id,
                location: r.location,
                occupation: r.occupation,
                current_situation: r.current_situation,
                desires: r.desires,
                vulnerabilities: r.vulnerabilities,
                habits: r.habits,
                personal_values: r.personal_values,
                likes: r.likes,
                dislikes: r.dislikes,
                relationships: r.relationships,
                updated_at: Some(r.updated_at),
            },
            None => Self {
                instance_id,
                location: None,
                occupation: None,
                current_situation: None,
                desires: None,
                vulnerabilities: None,
                habits: None,
                personal_values: None,
                likes: vec![],
                dislikes: vec![],
                relationships: vec![],
                updated_at: None,
            },
        }
    }
}

/// The real user's side of one relationship's profile — what he has revealed
/// HERE. Not the global `human_insights` profile, which v1 serves at
/// `GET /comp/user/{user_id}/profile`.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct UserInsightResponse {
    #[schema(value_type = String)]
    pub instance_id: Uuid,
    pub location: Option<String>,
    pub occupation: Option<String>,
    pub current_situation: Option<String>,
    pub desires: Option<String>,
    pub vulnerabilities: Option<String>,
    pub habits: Option<String>,
    pub personal_values: Option<String>,
    pub likes: Vec<String>,
    pub dislikes: Vec<String>,
    pub relationships: Vec<String>,
    /// `null` when this relationship has no user-insights row yet.
    pub updated_at: Option<DateTime<Utc>>,
}

impl UserInsightResponse {
    fn from_row(instance_id: Uuid, row: Option<UserInsightsRow>) -> Self {
        match row {
            Some(r) => Self {
                instance_id,
                location: r.location,
                occupation: r.occupation,
                current_situation: r.current_situation,
                desires: r.desires,
                vulnerabilities: r.vulnerabilities,
                habits: r.habits,
                personal_values: r.personal_values,
                likes: r.likes,
                dislikes: r.dislikes,
                relationships: r.relationships,
                updated_at: Some(r.updated_at),
            },
            None => Self {
                instance_id,
                location: None,
                occupation: None,
                current_situation: None,
                desires: None,
                vulnerabilities: None,
                habits: None,
                personal_values: None,
                likes: vec![],
                dislikes: vec![],
                relationships: vec![],
                updated_at: None,
            },
        }
    }
}

/// Ownership gate shared by both handlers: the path key is an instance, not a
/// user, so ownership is read through the instance rather than compared against
/// the path. `None` (unknown, or `status <> 'active'`) is 404.
async fn require_owned_instance(
    state: &AppState,
    instance_id: Uuid,
    jwt_user: Uuid,
) -> Result<(), AppError> {
    let gate = PersonaRepo { pool: &state.pool }
        .load_instance_gate(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("no such instance".into()))?;
    if gate.owner_uid != jwt_user {
        return Err(AppError::Forbidden("not your data".into()));
    }
    Ok(())
}

#[utoipa::path(
    get,
    path = "/v2/comp/instance/{instance_id}/insight/character",
    tag = "insight",
    params(("instance_id" = Uuid, Path, description = "Persona instance id owned by the JWT user")),
    responses(
        (status = 200, body = CharacterInsightResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "instance is not owned by the JWT user"),
        (status = 404, description = "no such active instance")
    ),
    security(("bearer" = []))
)]
async fn get_character_insight(
    State(state): State<AppState>,
    Path(instance_id): Path<Uuid>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<Json<CharacterInsightResponse>, AppError> {
    require_owned_instance(&state, instance_id, jwt_user).await?;
    let row = CharacterInsightRepo { pool: &state.pool }
        .load(instance_id)
        .await?;
    Ok(Json(CharacterInsightResponse::from_row(instance_id, row)))
}

#[utoipa::path(
    get,
    path = "/v2/comp/instance/{instance_id}/insight/user",
    tag = "insight",
    params(("instance_id" = Uuid, Path, description = "Persona instance id owned by the JWT user")),
    responses(
        (status = 200, body = UserInsightResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "instance is not owned by the JWT user"),
        (status = 404, description = "no such active instance")
    ),
    security(("bearer" = []))
)]
async fn get_user_insight(
    State(state): State<AppState>,
    Path(instance_id): Path<Uuid>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<Json<UserInsightResponse>, AppError> {
    require_owned_instance(&state, instance_id, jwt_user).await?;
    let row = UserInsightRepo { pool: &state.pool }
        .load(instance_id)
        .await?;
    Ok(Json(UserInsightResponse::from_row(instance_id, row)))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_character_insight))
        .routes(routes!(get_user_insight))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
    use axum::Router;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{build_router, mint_test_jwt};

    /// The full app router, auth layer included — the same composition the
    /// companion route tests use, so these tests exercise the real 401 path
    /// rather than a hand-assembled stand-in.
    async fn app(pool: sqlx::PgPool) -> Router {
        build_router(test_state(pool))
    }

    fn bearer(user_id: Uuid) -> String {
        mint_test_jwt(user_id)
    }

    async fn seed_instance(pool: &sqlx::PgPool, owner: Uuid) -> Uuid {
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ($1, 'p', '{}'::jsonb) RETURNING id",
        )
        .bind(format!("seed-{}", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1,$2) RETURNING id",
        )
        .bind(genome_id)
        .bind(owner)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn user_insight_returns_the_row(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_instance(&pool, owner).await;
        sqlx::query("INSERT INTO engine.user_insights (instance_id, location) VALUES ($1,$2)")
            .bind(instance_id)
            .bind("深圳南山")
            .execute(&pool)
            .await
            .unwrap();

        let res = app(pool)
            .await
            .oneshot(
                Request::get(format!("/v2/comp/instance/{instance_id}/insight/user"))
                    .header(header::AUTHORIZATION, format!("Bearer {}", bearer(owner)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["location"], "深圳南山");
        assert_eq!(body["instance_id"], instance_id.to_string());
        assert!(body["updated_at"].is_string());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn character_insight_returns_the_row(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_instance(&pool, owner).await;
        sqlx::query("INSERT INTO engine.character_insights (instance_id, location) VALUES ($1,$2)")
            .bind(instance_id)
            .bind("还在公司")
            .execute(&pool)
            .await
            .unwrap();

        let res = app(pool)
            .await
            .oneshot(
                Request::get(format!("/v2/comp/instance/{instance_id}/insight/character"))
                    .header(header::AUTHORIZATION, format!("Bearer {}", bearer(owner)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["location"], "还在公司");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_row_yet_returns_all_nulls_and_null_updated_at(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_instance(&pool, owner).await;

        let res = app(pool)
            .await
            .oneshot(
                Request::get(format!("/v2/comp/instance/{instance_id}/insight/user"))
                    .header(header::AUTHORIZATION, format!("Bearer {}", bearer(owner)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 65536).await.unwrap()).unwrap();
        assert!(body["location"].is_null());
        assert!(body["updated_at"].is_null());
        assert_eq!(body["likes"], serde_json::json!([]));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn foreign_instance_is_403(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_instance(&pool, owner).await;
        let intruder = Uuid::new_v4();

        let res = app(pool)
            .await
            .oneshot(
                Request::get(format!("/v2/comp/instance/{instance_id}/insight/user"))
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {}", bearer(intruder)),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn unknown_instance_is_404(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let res = app(pool)
            .await
            .oneshot(
                Request::get(format!("/v2/comp/instance/{}/insight/user", Uuid::new_v4()))
                    .header(header::AUTHORIZATION, format!("Bearer {}", bearer(owner)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn archived_instance_is_404(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_instance(&pool, owner).await;
        sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
            .bind(instance_id)
            .execute(&pool)
            .await
            .unwrap();

        let res = app(pool)
            .await
            .oneshot(
                Request::get(format!("/v2/comp/instance/{instance_id}/insight/character"))
                    .header(header::AUTHORIZATION, format!("Bearer {}", bearer(owner)))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn no_bearer_is_401(pool: sqlx::PgPool) {
        let instance_id = seed_instance(&pool, Uuid::new_v4()).await;
        let res = app(pool)
            .await
            .oneshot(
                Request::get(format!("/v2/comp/instance/{instance_id}/insight/user"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! `/v2/comp/instance/{instance_id}/items` — the persona's items and wardrobe
//! (spec docs/superpowers/specs/2026-10-12-persona-items-and-gift-design.md §7).
//!
//! Genome presets are listed first and cannot be deleted per instance; every
//! instance row, gift or caller-written, can. The engine trusts the caller
//! here as it trusts `tip` and `gift`: a caller that sells items keeps its
//! clients from reaching POST and DELETE directly.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::persona::preset_items;
use eros_engine_core::types::ItemKind;
use eros_engine_store::persona::{InstanceItemRow, PersonaRepo};

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::routes::companion_stream::{item_name, ItemKindDto};
use crate::state::AppState;

/// Where an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemSourceDto {
    /// The genome's `art_metadata.possessions` / `wardrobe`; not deletable here.
    Preset,
    /// An accepted `gift` action.
    Gift,
    /// Written through `POST`.
    Caller,
}

/// One entry of the persona's lists.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PersonaItemView {
    /// `null` for a preset.
    #[schema(value_type = Option<String>)]
    pub id: Option<Uuid>,
    pub kind: ItemKindDto,
    pub name: String,
    pub source: ItemSourceDto,
    /// The gift turn; `null` for presets, caller rows, and gifts whose
    /// message was deleted.
    #[schema(value_type = Option<String>)]
    pub user_message_id: Option<Uuid>,
    /// `null` for a preset.
    pub created_at: Option<DateTime<Utc>>,
}

impl PersonaItemView {
    fn from_row(r: InstanceItemRow) -> Option<Self> {
        Some(Self {
            id: Some(r.id),
            kind: ItemKind::parse(&r.kind)?.into(),
            name: r.name,
            source: if r.source == "gift" {
                ItemSourceDto::Gift
            } else {
                ItemSourceDto::Caller
            },
            user_message_id: r.user_message_id,
            created_at: Some(r.created_at),
        })
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PersonaItemsResponse {
    /// Presets first, then the instance's rows, newest first. Neither
    /// deduplicated nor capped.
    pub items: Vec<PersonaItemView>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AddItemRequest {
    pub kind: ItemKindDto,
    /// 1–64 chars after trim, no line-break or control characters.
    #[schema(max_length = 64)]
    pub name: String,
}

/// The same gate as the insight routes: 404 for an unknown or archived
/// instance, 403 for another user's.
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
    path = "/v2/comp/instance/{instance_id}/items",
    tag = "persona",
    params(("instance_id" = Uuid, Path, description = "Persona instance id owned by the JWT user")),
    responses(
        (status = 200, body = PersonaItemsResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "instance is not owned by the JWT user"),
        (status = 404, description = "no such active instance")
    ),
    security(("bearer" = []))
)]
async fn list_items(
    State(state): State<AppState>,
    Path(instance_id): Path<Uuid>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<Json<PersonaItemsResponse>, AppError> {
    let repo = PersonaRepo { pool: &state.pool };
    // Same active filter as `load_instance_gate`; read whole for the presets.
    let persona = repo
        .load_companion(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("no such instance".into()))?;
    if persona.instance.owner_uid != jwt_user {
        return Err(AppError::Forbidden("not your data".into()));
    }
    let mut items: Vec<PersonaItemView> = preset_items(&persona.genome.art_metadata)
        .into_iter()
        .map(|(kind, name)| PersonaItemView {
            id: None,
            kind: kind.into(),
            name,
            source: ItemSourceDto::Preset,
            user_message_id: None,
            created_at: None,
        })
        .collect();
    items.extend(
        repo.list_items(instance_id)
            .await?
            .into_iter()
            .filter_map(PersonaItemView::from_row),
    );
    Ok(Json(PersonaItemsResponse { items }))
}

#[utoipa::path(
    post,
    path = "/v2/comp/instance/{instance_id}/items",
    tag = "persona",
    params(("instance_id" = Uuid, Path, description = "Persona instance id owned by the JWT user")),
    request_body = AddItemRequest,
    responses(
        (status = 201, body = PersonaItemView),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "instance is not owned by the JWT user"),
        (status = 404, description = "no such active instance"),
        (status = 422, description = "name out of bounds or with control characters")
    ),
    security(("bearer" = []))
)]
async fn add_item(
    State(state): State<AppState>,
    Path(instance_id): Path<Uuid>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
    Json(req): Json<AddItemRequest>,
) -> Result<(StatusCode, Json<PersonaItemView>), AppError> {
    require_owned_instance(&state, instance_id, jwt_user).await?;
    let name = item_name(&req.name, "name").map_err(AppError::Unprocessable)?;
    let row = PersonaRepo { pool: &state.pool }
        .insert_item(instance_id, req.kind.into(), &name)
        .await?;
    let view = PersonaItemView::from_row(row)
        .ok_or_else(|| AppError::Internal("stored item kind unreadable".into()))?;
    Ok((StatusCode::CREATED, Json(view)))
}

#[utoipa::path(
    delete,
    path = "/v2/comp/instance/{instance_id}/items/{item_id}",
    tag = "persona",
    params(
        ("instance_id" = Uuid, Path, description = "Persona instance id owned by the JWT user"),
        ("item_id" = Uuid, Path, description = "An item row of this instance")
    ),
    responses(
        (status = 204, description = "deleted"),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "instance is not owned by the JWT user"),
        (status = 404, description = "no such active instance, or no such item on it")
    ),
    security(("bearer" = []))
)]
async fn delete_item(
    State(state): State<AppState>,
    Path((instance_id, item_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(jwt_user)): Extension<AuthUser>,
) -> Result<StatusCode, AppError> {
    require_owned_instance(&state, instance_id, jwt_user).await?;
    let deleted = PersonaRepo { pool: &state.pool }
        .delete_item(instance_id, item_id)
        .await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("no such item".into()))
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_items, add_item))
        .routes(routes!(delete_item))
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

    async fn app(pool: sqlx::PgPool) -> Router {
        build_router(test_state(pool))
    }

    async fn seed_instance(pool: &sqlx::PgPool, owner: Uuid, art: serde_json::Value) -> Uuid {
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ($1, 'p', $2) RETURNING id",
        )
        .bind(format!("seed-{}", Uuid::new_v4()))
        .bind(art)
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

    async fn call(
        app: Router,
        method: &str,
        uri: String,
        user: Uuid,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri).header(
            header::AUTHORIZATION,
            format!("Bearer {}", mint_test_jwt(user)),
        );
        let body = match body {
            Some(b) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = app.oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 65536).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn presets_come_first_then_rows_newest_first(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id =
            seed_instance(&pool, owner, serde_json::json!({"wardrobe": ["白色开衫"]})).await;
        let uri = format!("/v2/comp/instance/{instance_id}/items");

        let (s, posted) = call(
            app(pool.clone()).await,
            "POST",
            uri.clone(),
            owner,
            Some(serde_json::json!({"kind": "item", "name": " 摩托车 "})),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
        assert_eq!(posted["name"], "摩托车");
        assert_eq!(posted["kind"], "item");
        assert_eq!(posted["source"], "caller");

        let (s, body) = call(app(pool).await, "GET", uri, owner, None).await;
        assert_eq!(s, StatusCode::OK);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["source"], "preset");
        assert_eq!(items[0]["name"], "白色开衫");
        assert!(items[0]["id"].is_null());
        assert_eq!(items[1]["id"], posted["id"]);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn post_rejects_a_bad_name(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_instance(&pool, owner, serde_json::json!({})).await;
        let (s, _) = call(
            app(pool).await,
            "POST",
            format!("/v2/comp/instance/{instance_id}/items"),
            owner,
            Some(serde_json::json!({"kind": "clothing", "name": "字".repeat(65)})),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn delete_removes_a_row_and_404s_a_stranger_id(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_instance(&pool, owner, serde_json::json!({})).await;
        let uri = format!("/v2/comp/instance/{instance_id}/items");
        let (_, posted) = call(
            app(pool.clone()).await,
            "POST",
            uri.clone(),
            owner,
            Some(serde_json::json!({"kind": "item", "name": "吉他"})),
        )
        .await;
        let id = posted["id"].as_str().unwrap().to_string();

        let (s, _) = call(
            app(pool.clone()).await,
            "DELETE",
            format!("{uri}/{id}"),
            owner,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _) = call(
            app(pool.clone()).await,
            "DELETE",
            format!("{uri}/{id}"),
            owner,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND, "already gone");
        let (_, body) = call(app(pool).await, "GET", uri, owner, None).await;
        assert_eq!(body["items"], serde_json::json!([]));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn another_users_instance_is_403_and_an_archived_one_404(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_instance(&pool, owner, serde_json::json!({})).await;
        let uri = format!("/v2/comp/instance/{instance_id}/items");
        let (s, _) = call(
            app(pool.clone()).await,
            "GET",
            uri.clone(),
            Uuid::new_v4(),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(
            app(pool.clone()).await,
            "POST",
            uri.clone(),
            Uuid::new_v4(),
            Some(serde_json::json!({"kind": "item", "name": "吉他"})),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
            .bind(instance_id)
            .execute(&pool)
            .await
            .unwrap();
        let (s, _) = call(app(pool).await, "GET", uri, owner, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }
}

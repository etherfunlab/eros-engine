// SPDX-License-Identifier: AGPL-3.0-only
//! Genome write routes (`/svc/v1/genomes`): create and update a persona genome
//! from an `eros-character` card (docs/eros-character-v1.2.md).
//!
//! Admitted by the shared secret header `x-genome-write-secret`, never by a
//! user token, and mounted only when the deployment sets
//! `GENOME_WRITE_SECRET` (see `routes::router`). No user is on the request and
//! no ownership is checked: the caller does its own.
//!
//! No error built here quotes card text.

use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::persona::{NewGenome, PersonaRepo};

use crate::byoc::{parse_card_v1, Card, MAX_PLAINTEXT_LEN};
use crate::error::AppError;
use crate::routes::svc::admit_with;
use crate::state::{AppState, Gate};

pub(crate) const GENOME_SECRET_HEADER: &str = "x-genome-write-secret";

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateGenomeRequest {
    /// The id to create the genome under. When a genome already has it, the
    /// request writes nothing and answers `200`, so a retried create leaves
    /// one row. Omitted: the engine mints one.
    pub id: Option<Uuid>,
    /// An `eros-character` card (`spec`, `spec_version` `"1"`, `"1.1"`,
    /// `"1.2"` or `"1.3"`, `data`), at most 64 KiB once serialised.
    #[schema(value_type = Object)]
    pub card: serde_json::Value,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateGenomeRequest {
    /// An `eros-character` card (`spec`, `spec_version` `"1"`, `"1.1"`,
    /// `"1.2"` or `"1.3"`, `data`), at most 64 KiB once serialised.
    #[schema(value_type = Object)]
    pub card: serde_json::Value,
}

#[derive(Serialize, ToSchema)]
pub struct GenomeIdResponse {
    pub id: Uuid,
}

fn admit_genomes(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    admit_with(
        headers,
        &state.config.keys(Gate::GenomeWrite),
        GENOME_SECRET_HEADER,
        "genome write",
    )
}

/// The body did not parse as the request type. serde's own text quotes the
/// input, so none of it is surfaced.
fn malformed_body() -> AppError {
    AppError::BadRequest("body must be a JSON object of the documented fields".into())
}

/// The path segment is not a uuid. The path extractor's own text quotes it,
/// so none of it is surfaced.
fn malformed_id() -> AppError {
    AppError::BadRequest("id must be a uuid".into())
}

/// Whether any string in `v`, an object key included, holds U+0000, which
/// Postgres refuses in both `text` and `jsonb`.
fn contains_nul(v: &serde_json::Value) -> bool {
    use serde_json::Value;
    match v {
        Value::String(s) => s.contains('\0'),
        Value::Array(items) => items.iter().any(contains_nul),
        Value::Object(map) => map.iter().any(|(k, v)| k.contains('\0') || contains_nul(v)),
        _ => false,
    }
}

/// A card that is over the size limit, that `parse_card_v1` refuses, or that
/// holds a NUL character the database would refuse; the message is one of
/// `parse_card_v1`'s fixed strings or one of this function's.
fn checked_card(card: &serde_json::Value) -> Result<Card, AppError> {
    let bytes = serde_json::to_vec(card)?;
    if bytes.len() > MAX_PLAINTEXT_LEN {
        return Err(AppError::BadRequest("card exceeds 64 KiB".into()));
    }
    let parsed = parse_card_v1(&bytes).map_err(AppError::BadRequest)?;
    if contains_nul(card) {
        return Err(AppError::BadRequest("card contains a NUL character".into()));
    }
    Ok(parsed)
}

fn new_genome(card: &Card) -> NewGenome<'_> {
    NewGenome {
        name: &card.name,
        system_prompt: &card.system_prompt,
        tip_personality: card.tip_personality.as_deref(),
        art_metadata: &card.art_metadata,
    }
}

/// Create a persona genome from an `eros-character` card.
///
/// Retry-safe: with a caller-supplied `id` that already exists, answers `200`
/// with the id and writes nothing, so the stored genome keeps its first
/// columns even when the retried card differs. Without `id` the engine mints
/// one. A rejected card is `400` with a fixed message; card text is never
/// echoed.
#[utoipa::path(
    post,
    path = "/svc/v1/genomes",
    tag = "svc",
    request_body = CreateGenomeRequest,
    responses(
        (status = 201, description = "A genome was created", body = GenomeIdResponse),
        (status = 200, description = "A genome with this id already exists; nothing was written", body = GenomeIdResponse),
        (status = 400, description = "Malformed body, or a card that is oversize or invalid"),
        (status = 403, description = "x-genome-write-secret missing or wrong")
    ),
    security(("genome_write_secret" = []))
)]
async fn create_genome(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateGenomeRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<GenomeIdResponse>), AppError> {
    // The body is already read by the extractor; its error is not reported
    // before admission, and nothing touches the DB before it.
    admit_genomes(&state, &headers)?;
    let Json(req) = body.map_err(|_| malformed_body())?;
    let card = checked_card(&req.card)?;

    let (id, created) = PersonaRepo { pool: &state.pool }
        .create_genome(req.id, &new_genome(&card))
        .await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(GenomeIdResponse { id })))
}

/// Overwrite a persona genome from an `eros-character` card.
///
/// Replaces `name`, `system_prompt`, `tip_personality` and `art_metadata` in
/// place; `created_at` is kept. A rejected card is `400` with a fixed message;
/// card text is never echoed.
#[utoipa::path(
    put,
    path = "/svc/v1/genomes/{id}",
    tag = "svc",
    params(("id" = Uuid, Path, description = "The genome to overwrite")),
    request_body = UpdateGenomeRequest,
    responses(
        (status = 204, description = "The genome was overwritten"),
        (status = 400, description = "Malformed body, or a card that is oversize or invalid"),
        (status = 403, description = "x-genome-write-secret missing or wrong"),
        (status = 404, description = "No genome has this id")
    ),
    security(("genome_write_secret" = []))
)]
async fn update_genome(
    State(state): State<AppState>,
    headers: HeaderMap,
    id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<UpdateGenomeRequest>, JsonRejection>,
) -> Result<StatusCode, AppError> {
    admit_genomes(&state, &headers)?;
    let Path(id) = id.map_err(|_| malformed_id())?;
    let Json(req) = body.map_err(|_| malformed_body())?;
    let card = checked_card(&req.card)?;

    let updated = PersonaRepo { pool: &state.pool }
        .update_genome(id, &new_genome(&card))
        .await?;
    if updated {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("genome".into()))
    }
}

/// Genome-write sub-tree: mounted under `GENOME_WRITE_SECRET`.
pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_genome))
        .routes(routes!(update_genome))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use chrono::{DateTime, Utc};
    use serde_json::{json, Value};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::GENOME_SECRET_HEADER;
    use crate::byoc::MAX_PLAINTEXT_LEN;
    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::build_router;

    const SECRET: &str = "9876543210fedcba9876543210fedcba";
    /// Placed in card text that must never come back in a response.
    const MARKER: &str = "zz-card-marker-4f1c";

    fn app(pool: sqlx::PgPool, secret: Option<&str>) -> Router {
        let mut state = test_state(pool);
        state.config.genome_write.secret = secret.map(String::from);
        build_router(state)
    }

    /// A response: status, and the body as JSON (`Null` when empty) next to
    /// its raw text.
    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        secret: Option<&str>,
        body: &str,
    ) -> (StatusCode, Value, String) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if let Some(s) = secret {
            req = req.header(GENOME_SECRET_HEADER, s);
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json, text)
    }

    fn card(name: &str, system_prompt: &str) -> Value {
        json!({
            "spec": "eros-character",
            "spec_version": "1.1",
            "data": {
                "name": name,
                "system_prompt": system_prompt,
                "tip_personality": "shy",
                "art_metadata": { "hair": "red" }
            }
        })
    }

    async fn create(app: &Router, id: Option<Uuid>, card: Value) -> (StatusCode, Value, String) {
        let mut body = json!({ "card": card });
        if let Some(id) = id {
            body["id"] = json!(id);
        }
        call(
            app,
            "POST",
            "/svc/v1/genomes",
            Some(SECRET),
            &body.to_string(),
        )
        .await
    }

    async fn update(app: &Router, id: Uuid, card: Value) -> (StatusCode, Value, String) {
        call(
            app,
            "PUT",
            &format!("/svc/v1/genomes/{id}"),
            Some(SECRET),
            &json!({ "card": card }).to_string(),
        )
        .await
    }

    type Columns = (String, String, Option<String>, Value, DateTime<Utc>);

    async fn columns(pool: &sqlx::PgPool, id: Uuid) -> Option<Columns> {
        sqlx::query_as(
            "SELECT name, system_prompt, tip_personality, art_metadata, created_at \
             FROM engine.persona_genomes WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    async fn genome_count(pool: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM engine.persona_genomes")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn routes_are_absent_without_the_secret(pool: sqlx::PgPool) {
        // Unmounted, the paths answer as any unknown path does, so they do not
        // confirm the feature exists.
        let app = app(pool.clone(), None);
        let body = json!({ "card": card("Nova", "sp") }).to_string();
        let (unknown, _, _) =
            call(&app, "POST", "/svc-does-not-exist/zzz", Some(SECRET), &body).await;
        let (status, _, _) = call(&app, "POST", "/svc/v1/genomes", Some(SECRET), &body).await;
        assert_eq!(status, unknown);
        let uri = format!("/svc/v1/genomes/{}", Uuid::new_v4());
        let (status, _, _) = call(&app, "PUT", &uri, Some(SECRET), &body).await;
        assert_eq!(status, unknown);
        assert_eq!(genome_count(&pool).await, 0);
    }

    /// A master key mounts the routes and is accepted in the feature's header.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_master_key_writes_a_genome(pool: sqlx::PgPool) {
        const ADMIN: &str = "admin-0123456789abcdef0123456789abcdef";
        let mut state = test_state(pool.clone());
        state.config.masters.admin_secret = Some(ADMIN.into());
        let app = build_router(state);
        let body = json!({ "card": card("Nova", "sp") }).to_string();
        let (status, _, _) = call(&app, "POST", "/svc/v1/genomes", Some(ADMIN), &body).await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _, _) = call(&app, "POST", "/svc/v1/genomes", Some("wrong"), &body).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(genome_count(&pool).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_wrong_or_missing_secret_is_403(pool: sqlx::PgPool) {
        let app = app(pool.clone(), Some(SECRET));
        let id = Uuid::new_v4();
        let ok_body = json!({ "card": card("Nova", "sp") }).to_string();
        // A bad body is not reported before admission.
        for body in [ok_body.as_str(), "not json at all"] {
            for secret in [None, Some("wrong-secret"), Some("")] {
                let (status, _, text) = call(&app, "POST", "/svc/v1/genomes", secret, body).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "POST {secret:?}");
                assert!(!text.contains(SECRET));
                let uri = format!("/svc/v1/genomes/{id}");
                let (status, _, _) = call(&app, "PUT", &uri, secret, body).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "PUT {secret:?}");
            }
        }
        // Another sub-tree's header does not open this one.
        let req = Request::post("/svc/v1/genomes")
            .header("content-type", "application/json")
            .header("x-user-erase-secret", SECRET)
            .body(Body::from(ok_body))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert_eq!(genome_count(&pool).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn create_returns_201_and_the_row(pool: sqlx::PgPool) {
        let app = app(pool.clone(), Some(SECRET));

        // No id: the engine mints one.
        let (status, body, _) = create(&app, None, card("Nova", "you are Nova")).await;
        assert_eq!(status, StatusCode::CREATED);
        let minted: Uuid = body["id"].as_str().unwrap().parse().unwrap();
        let (name, system_prompt, tip, art, _) = columns(&pool, minted).await.unwrap();
        assert_eq!(name, "Nova");
        assert_eq!(system_prompt, "you are Nova");
        assert_eq!(tip.as_deref(), Some("shy"));
        assert_eq!(art, json!({ "hair": "red" }));

        // A supplied id is the row's id; a card without the optional fields
        // stores NULL and `{}`.
        let wanted = Uuid::new_v4();
        let bare = json!({
            "spec": "eros-character",
            "spec_version": "1",
            "data": { "name": "Echo", "system_prompt": "sp" }
        });
        let (status, body, _) = create(&app, Some(wanted), bare).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body, json!({ "id": wanted }));
        let (name, _, tip, art, _) = columns(&pool, wanted).await.unwrap();
        assert_eq!(name, "Echo");
        assert_eq!(tip, None);
        assert_eq!(art, json!({}));
        assert_eq!(genome_count(&pool).await, 2);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn create_with_a_taken_id_is_200_and_writes_nothing(pool: sqlx::PgPool) {
        let app = app(pool.clone(), Some(SECRET));
        let id = Uuid::new_v4();
        let (status, _, _) = create(&app, Some(id), card("First", "first prompt")).await;
        assert_eq!(status, StatusCode::CREATED);
        let before = columns(&pool, id).await.unwrap();

        // The retry carries a different card; the row keeps the first one.
        let (status, body, _) = create(&app, Some(id), card("Second", "second prompt")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "id": id }));
        assert_eq!(columns(&pool, id).await.unwrap(), before);
        assert_eq!(genome_count(&pool).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn update_overwrites_in_place_and_is_204(pool: sqlx::PgPool) {
        let app = app(pool.clone(), Some(SECRET));
        let (_, body, _) = create(&app, None, card("Old", "old prompt")).await;
        let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
        let (_, _, _, _, created_at) = columns(&pool, id).await.unwrap();

        let next = json!({
            "spec": "eros-character",
            "spec_version": "1",
            "data": { "name": "New", "system_prompt": "new prompt" }
        });
        let (status, _, text) = update(&app, id, next).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(text.is_empty());
        assert_eq!(
            columns(&pool, id).await.unwrap(),
            (
                "New".to_string(),
                "new prompt".to_string(),
                None,
                json!({}),
                created_at
            )
        );
        assert_eq!(genome_count(&pool).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn update_of_a_missing_genome_is_404(pool: sqlx::PgPool) {
        let app = app(pool.clone(), Some(SECRET));
        let (status, _, _) = update(&app, Uuid::new_v4(), card("Nobody", "sp")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(genome_count(&pool).await, 0);
    }

    /// A card whose serialised form is exactly `len` bytes, the marker in its
    /// prompt.
    fn card_of_len(len: usize) -> Value {
        let mut c = card("Big", MARKER);
        let base = serde_json::to_vec(&c).unwrap().len();
        c["data"]["system_prompt"] = json!(format!("{MARKER}{}", "a".repeat(len - base)));
        assert_eq!(serde_json::to_vec(&c).unwrap().len(), len);
        c
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_bad_card_is_400_and_never_quoted(pool: sqlx::PgPool) {
        let app = app(pool.clone(), Some(SECRET));
        let (_, body, _) = create(&app, None, card("Kept", "kept prompt")).await;
        let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
        let kept = columns(&pool, id).await.unwrap();

        let mut unknown_field = card("Nova", MARKER);
        unknown_field["data"][MARKER] = json!(MARKER);
        let mut unknown_top = card("Nova", MARKER);
        unknown_top[MARKER] = json!(MARKER);
        let mut wrong_spec = card("Nova", MARKER);
        wrong_spec["spec"] = json!(MARKER);
        let mut wrong_type = card("Nova", MARKER);
        wrong_type["data"]["tip_personality"] = json!([MARKER]);
        let cards = [
            ("unknown field", unknown_field),
            ("unknown top-level field", unknown_top),
            ("wrong spec", wrong_spec),
            ("wrong type", wrong_type),
            ("blank name", card("   ", MARKER)),
            ("not an object", json!(MARKER)),
            ("oversize", card_of_len(MAX_PLAINTEXT_LEN + 1)),
        ];
        for (what, bad) in cards {
            let (status, _, text) = create(&app, None, bad.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "create: {what}");
            assert!(!text.contains(MARKER), "create: {what} quoted the card");
            let (status, _, text) = update(&app, id, bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "update: {what}");
            assert!(!text.contains(MARKER), "update: {what} quoted the card");
        }
        // Nothing was written by any of them.
        assert_eq!(columns(&pool, id).await.unwrap(), kept);
        assert_eq!(genome_count(&pool).await, 1);

        // Exactly the limit is accepted.
        let (status, _, _) = create(&app, None, card_of_len(MAX_PLAINTEXT_LEN)).await;
        assert_eq!(status, StatusCode::CREATED);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_malformed_body_is_400_and_never_quoted(pool: sqlx::PgPool) {
        let app = app(pool.clone(), Some(SECRET));
        let id = Uuid::new_v4();
        let uri = format!("/svc/v1/genomes/{id}");
        let good = card("Nova", "sp");
        let bodies = [
            json!({ "card": good, MARKER: 1 }).to_string(),
            json!({ "id": MARKER, "card": good }).to_string(),
            json!({ "id": id }).to_string(),
            json!([MARKER]).to_string(),
            format!("{{\"card\": {MARKER}"),
        ];
        for body in &bodies {
            let (status, _, text) = call(&app, "POST", "/svc/v1/genomes", Some(SECRET), body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "POST {body}");
            assert!(!text.contains(MARKER), "POST {body} quoted the body");
        }
        // The update body takes no `id`.
        let with_id = json!({ "id": id, "card": good }).to_string();
        let (status, _, _) = call(&app, "PUT", &uri, Some(SECRET), &with_id).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, text) = call(&app, "PUT", &uri, Some(SECRET), &bodies[4]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!text.contains(MARKER));
        assert_eq!(genome_count(&pool).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_card_with_a_nul_character_is_400_and_never_quoted(pool: sqlx::PgPool) {
        let app = app(pool.clone(), Some(SECRET));
        let (_, body, _) = create(&app, None, card("Kept", "kept prompt")).await;
        let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
        let kept = columns(&pool, id).await.unwrap();
        let nul = format!("{MARKER}\u{0}{MARKER}");

        let in_name = card(&nul, "sp");
        let in_prompt = card("Nova", &nul);
        let mut in_tip = card("Nova", "sp");
        in_tip["data"]["tip_personality"] = json!(nul);
        let mut in_art_value = card("Nova", "sp");
        in_art_value["data"]["art_metadata"] = json!({ "look": { "tags": ["ok", nul] } });
        let mut in_art_key = card("Nova", "sp");
        in_art_key["data"]["art_metadata"] = json!({ "look": { nul.clone(): "ok" } });
        let cards = [
            ("name", in_name),
            ("system_prompt", in_prompt),
            ("tip_personality", in_tip),
            ("art_metadata value", in_art_value),
            ("art_metadata key", in_art_key),
        ];
        for (what, bad) in cards {
            let (status, _, text) = create(&app, None, bad.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "create: NUL in {what}");
            assert!(!text.contains(MARKER), "create: {what} quoted the card");
            let (status, _, text) = update(&app, id, bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "update: NUL in {what}");
            assert!(!text.contains(MARKER), "update: {what} quoted the card");
        }
        assert_eq!(columns(&pool, id).await.unwrap(), kept);
        assert_eq!(genome_count(&pool).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_path_that_is_not_a_uuid_is_403_then_400(pool: sqlx::PgPool) {
        let app = app(pool.clone(), Some(SECRET));
        let uri = format!("/svc/v1/genomes/{MARKER}");
        let body = json!({ "card": card("Nova", "sp") }).to_string();
        let (status, _, text) = call(&app, "PUT", &uri, None, &body).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(!text.contains(MARKER));
        let (status, _, text) = call(&app, "PUT", &uri, Some(SECRET), &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!text.contains(MARKER), "the rejected segment was quoted");
        assert_eq!(genome_count(&pool).await, 0);
    }

    #[test]
    fn openapi_lists_the_genome_routes_and_scheme() {
        let api = crate::routes::openapi_for_tests();
        let create = &api["paths"]["/svc/v1/genomes"]["post"];
        let update = &api["paths"]["/svc/v1/genomes/{id}"]["put"];
        for op in [create, update] {
            assert!(op.is_object());
            assert!(op["security"][0]["genome_write_secret"].is_array());
        }
        let (_router, full) =
            utoipa_axum::router::OpenApiRouter::<crate::state::AppState>::with_openapi(
                <crate::openapi::ApiDoc as utoipa::OpenApi>::openapi(),
            )
            .merge(crate::routes::router_for_openapi())
            .split_for_parts();
        let full = serde_json::to_value(full).unwrap();
        let scheme = &full["components"]["securitySchemes"]["genome_write_secret"];
        assert_eq!(scheme["name"], "x-genome-write-secret");
        assert_eq!(scheme["in"], "header");
    }
}

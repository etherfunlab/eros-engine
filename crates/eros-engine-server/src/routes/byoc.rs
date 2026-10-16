// SPDX-License-Identifier: AGPL-3.0-only
//! BYOC key lifecycle (spec docs/superpowers/specs/2026-10-16-byoc-design.md
//! §5): `POST /v2/comp/byoc/init` draws a key and its ticket, and
//! `POST /v2/comp/byoc/create` turns a ticketed key into an instance.

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::persona::PersonaRepo;

use crate::auth::middleware::AuthUser;
use crate::byoc::{self, b64_decode, b64_encode};
use crate::byok::pre_error;
use crate::error::AppError;
use crate::routes::companion_stream::StreamPreErrorBody;
use crate::state::AppState;

#[derive(Serialize, utoipa::ToSchema)]
pub struct ByocInitResponse {
    /// 32 random bytes, base64url. Keep it: it opens every card sent for
    /// the instance, and the engine never shows it again.
    pub key: String,
    /// 32 bytes, base64url. Proves to `create` that the engine issued `key`
    /// to this user.
    pub ticket: String,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ByocCreateRequest {
    /// The `key` from init (base64url, padding optional).
    pub key: String,
    /// The `ticket` from init (base64url, padding optional).
    pub ticket: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ByocCreateResponse {
    pub instance_id: Uuid,
}

/// Admit the caller, and hand back the secret it proved it holds.
fn admit<'a>(state: &'a AppState, headers: &HeaderMap) -> Result<&'a str, AppError> {
    byoc::admit(&state.config.byoc, headers)?;
    Ok(state
        .config
        .byoc
        .caller_secret
        .as_deref()
        .expect("admit passes only when the secret is set"))
}

/// Draw a key for a new BYOC instance.
///
/// Writes nothing: an instance exists only once `create` is called with the
/// returned key and ticket, after the downstream has stored the key. Needs
/// the deployment's caller secret.
#[utoipa::path(
    post,
    path = "/v2/comp/byoc/init",
    tag = "byoc",
    params(
        ("X-Byoc-Caller-Secret" = Option<String>, Header,
            description = "The deployment's BYOC_CALLER_SECRET; required"),
    ),
    responses(
        (status = 200, body = ByocInitResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = StreamPreErrorBody),
    ),
    security(("bearer" = []))
)]
async fn byoc_init(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    headers: HeaderMap,
) -> Result<Json<ByocInitResponse>, AppError> {
    let secret = admit(&state, &headers)?;
    let key = byoc::mint_key();
    Ok(Json(ByocInitResponse {
        key: b64_encode(&key),
        ticket: b64_encode(&byoc::ticket(secret, user_id, &key)),
    }))
}

/// Create the BYOC instance for a key from init.
///
/// Idempotent per key: a repeat returns the same `instance_id`, and
/// re-creating an archived instance reactivates it. Needs the deployment's
/// caller secret.
#[utoipa::path(
    post,
    path = "/v2/comp/byoc/create",
    tag = "byoc",
    params(
        ("X-Byoc-Caller-Secret" = Option<String>, Header,
            description = "The deployment's BYOC_CALLER_SECRET; required"),
    ),
    request_body = ByocCreateRequest,
    responses(
        (status = 200, body = ByocCreateResponse),
        (status = 400, body = StreamPreErrorBody),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = StreamPreErrorBody),
        (status = 422, description = "malformed body"),
    ),
    security(("bearer" = []))
)]
async fn byoc_create(
    State(state): State<AppState>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    headers: HeaderMap,
    Json(req): Json<ByocCreateRequest>,
) -> Result<Json<ByocCreateResponse>, AppError> {
    let secret = admit(&state, &headers)?;
    let malformed = || {
        pre_error(
            StatusCode::BAD_REQUEST,
            "invalid_payload",
            "key and ticket must be base64url of 32 bytes each".into(),
        )
    };
    let key: [u8; 32] = b64_decode(&req.key)
        .and_then(|k| k.try_into().ok())
        .ok_or_else(malformed)?;
    let ticket: [u8; 32] = b64_decode(&req.ticket)
        .and_then(|t| t.try_into().ok())
        .ok_or_else(malformed)?;
    if !byoc::verify_ticket(secret, user_id, &key, &ticket) {
        return Err(pre_error(
            StatusCode::FORBIDDEN,
            "byoc_forbidden",
            "key was not issued to this user".into(),
        ));
    }
    let instance_id = PersonaRepo { pool: &state.pool }
        .create_byoc_instance(user_id, &key)
        .await?;
    Ok(Json(ByocCreateResponse { instance_id }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(byoc_init))
        .routes(routes!(byoc_create))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use serde_json::{json, Value};
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::byoc::{b64_decode, b64_encode};
    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        build_router, enable_byoc, mint_test_jwt, seed_byoc_instance_with_key, send_request,
        BYOC_SECRET,
    };
    use crate::state::AppState;

    fn byoc_state(pool: &PgPool) -> AppState {
        let mut state = test_state(pool.clone());
        enable_byoc(&mut state);
        state
    }

    /// A POST as `user`; `secret` is the `X-Byoc-Caller-Secret` value, if any.
    fn post(path: &str, user: Uuid, secret: Option<&str>, body: Option<Value>) -> Request<Body> {
        let mut b = Request::builder().method("POST").uri(path).header(
            header::AUTHORIZATION,
            format!("Bearer {}", mint_test_jwt(user)),
        );
        if let Some(s) = secret {
            b = b.header("x-byoc-caller-secret", s);
        }
        match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string())),
            None => b.body(Body::empty()),
        }
        .unwrap()
    }

    async fn init(state: &AppState, user: Uuid) -> (StatusCode, Value) {
        send_request(
            &mut build_router(state.clone()),
            post("/v2/comp/byoc/init", user, Some(BYOC_SECRET), None),
        )
        .await
    }

    async fn create(state: &AppState, user: Uuid, body: Value) -> (StatusCode, Value) {
        send_request(
            &mut build_router(state.clone()),
            post("/v2/comp/byoc/create", user, Some(BYOC_SECRET), Some(body)),
        )
        .await
    }

    /// A fresh `{key, ticket}` from init, which is also the create body.
    async fn issued(state: &AppState, user: Uuid) -> Value {
        let (status, body) = init(state, user).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn count_instances(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM engine.persona_instances")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn instance_id(body: &Value) -> Uuid {
        body["instance_id"].as_str().unwrap().parse().unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn init_then_create_makes_a_byoc_instance(pool: PgPool) {
        let state = byoc_state(&pool);
        let user = Uuid::new_v4();
        let issued = issued(&state, user).await;
        let key = b64_decode(issued["key"].as_str().unwrap()).unwrap();
        assert_eq!(key.len(), 32);

        let (status, body) = create(&state, user, issued).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (genome_id, byoc_key, owner_uid): (Option<Uuid>, Option<Vec<u8>>, Uuid) =
            sqlx::query_as(
                "SELECT genome_id, byoc_key, owner_uid FROM engine.persona_instances WHERE id = $1",
            )
            .bind(instance_id(&body))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(genome_id, None);
        assert_eq!(byoc_key, Some(key));
        assert_eq!(owner_uid, user);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn byoc_init_writes_nothing(pool: PgPool) {
        let state = byoc_state(&pool);
        let before = count_instances(&pool).await;
        issued(&state, Uuid::new_v4()).await;
        assert_eq!(count_instances(&pool).await, before);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn create_refuses_keys_the_engine_did_not_issue(pool: PgPool) {
        let state = byoc_state(&pool);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let (_, held_key) = seed_byoc_instance_with_key(&pool, a).await;
        let before = count_instances(&pool).await;
        let code =
            |r: (StatusCode, Value)| (r.0.as_u16(), r.1["code"].as_str().unwrap().to_owned());
        let forbidden = (403, "byoc_forbidden".to_owned());

        // A key and ticket the engine never issued.
        let random = json!({
            "key": b64_encode(&crate::byoc::mint_key()),
            "ticket": b64_encode(&crate::byoc::mint_key()),
        });
        assert_eq!(code(create(&state, a, random).await), forbidden);

        // A key an instance already holds, without its ticket.
        let held = json!({"key": b64_encode(&held_key), "ticket": b64_encode(&[0u8; 32])});
        assert_eq!(code(create(&state, a, held).await), forbidden);

        // Another user's key and ticket.
        let issued_to_a = issued(&state, a).await;
        assert_eq!(
            code(create(&state, b, issued_to_a.clone()).await),
            forbidden
        );

        // A ticket with one byte flipped.
        let mut ticket = b64_decode(issued_to_a["ticket"].as_str().unwrap()).unwrap();
        ticket[0] ^= 1;
        let flipped = json!({"key": issued_to_a["key"], "ticket": b64_encode(&ticket)});
        assert_eq!(code(create(&state, a, flipped).await), forbidden);

        assert_eq!(count_instances(&pool).await, before);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn byoc_create_is_idempotent(pool: PgPool) {
        let state = byoc_state(&pool);
        let user = Uuid::new_v4();
        let issued_once = issued(&state, user).await;
        let (s1, first) = create(&state, user, issued_once.clone()).await;
        let (s2, second) = create(&state, user, issued_once).await;
        assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
        assert_eq!(first["instance_id"], second["instance_id"]);

        // Two creates racing on a fresh key land on one row.
        let raced = issued(&state, user).await;
        let (mut r1, mut r2) = (build_router(state.clone()), build_router(state.clone()));
        let ((s1, one), (s2, two)) = tokio::join!(
            send_request(
                &mut r1,
                post(
                    "/v2/comp/byoc/create",
                    user,
                    Some(BYOC_SECRET),
                    Some(raced.clone())
                )
            ),
            send_request(
                &mut r2,
                post(
                    "/v2/comp/byoc/create",
                    user,
                    Some(BYOC_SECRET),
                    Some(raced.clone())
                )
            ),
        );
        assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));
        assert_eq!(one["instance_id"], two["instance_id"]);
        let key = b64_decode(raced["key"].as_str().unwrap()).unwrap();
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.persona_instances WHERE byoc_key = $1")
                .bind(key)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn byoc_recreate_after_archive_reactivates(pool: PgPool) {
        // Review Focus 3
        let state = byoc_state(&pool);
        let user = Uuid::new_v4();
        let issued = issued(&state, user).await;
        let (_, first) = create(&state, user, issued.clone()).await;
        let id = instance_id(&first);
        sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let (status, again) = create(&state, user, issued).await;
        assert_eq!(status, StatusCode::OK, "{again}");
        assert_eq!(instance_id(&again), id);
        let state_after: String =
            sqlx::query_scalar("SELECT status FROM engine.persona_instances WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state_after, "active");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn byoc_routes_need_the_caller_secret(pool: PgPool) {
        let user = Uuid::new_v4();
        let body = json!({"key": b64_encode(&[1u8; 32]), "ticket": b64_encode(&[2u8; 32])});
        let off = test_state(pool.clone());
        let on = byoc_state(&pool);
        for (state, secret) in [
            (&off, Some(BYOC_SECRET)),
            (&on, None),
            (&on, Some("not-the-deployments-secret-0123456789")),
        ] {
            for (path, req_body) in [
                ("/v2/comp/byoc/init", None),
                ("/v2/comp/byoc/create", Some(body.clone())),
            ] {
                let (status, resp) = send_request(
                    &mut build_router(state.clone()),
                    post(path, user, secret, req_body),
                )
                .await;
                assert_eq!(
                    (status.as_u16(), resp["code"].as_str()),
                    (403, Some("byoc_forbidden")),
                    "{path} with secret {secret:?}"
                );
            }
        }
        assert_eq!(count_instances(&pool).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn create_validates_key_and_ticket_shape(pool: PgPool) {
        let state = byoc_state(&pool);
        let user = Uuid::new_v4();
        let issued = issued(&state, user).await;
        let (key, ticket) = (
            issued["key"].as_str().unwrap().to_owned(),
            issued["ticket"].as_str().unwrap().to_owned(),
        );
        let invalid =
            |r: (StatusCode, Value)| (r.0.as_u16(), r.1["code"].as_str().map(str::to_owned));
        let expect = (400, Some("invalid_payload".to_owned()));

        assert_eq!(
            invalid(create(&state, user, json!({"key": "!!", "ticket": ticket})).await),
            expect
        );
        assert_eq!(
            invalid(create(&state, user, json!({"key": key, "ticket": "!!"})).await),
            expect
        );
        let short_key = b64_encode(&b64_decode(&key).unwrap()[..31]);
        assert_eq!(
            invalid(create(&state, user, json!({"key": short_key, "ticket": ticket})).await),
            expect
        );
        let short_ticket = b64_encode(&b64_decode(&ticket).unwrap()[..31]);
        assert_eq!(
            invalid(create(&state, user, json!({"key": key, "ticket": short_ticket})).await),
            expect
        );
        assert_eq!(count_instances(&pool).await, 0);

        // Review Focus 2: a padding encoder's output is the same key and ticket.
        let pad = |s: &str| {
            let mut s = s.to_owned();
            while !s.len().is_multiple_of(4) {
                s.push('=');
            }
            s
        };
        let (padded_key, padded_ticket) = (pad(&key), pad(&ticket));
        assert_ne!(padded_key, key);
        let (status, body) = create(
            &state,
            user,
            json!({"key": padded_key, "ticket": padded_ticket}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! POST /persona/{instance_id}/image/compose — persona-scoped standalone
//! image-prompt composition. Not a chat turn: nothing is persisted, no
//! affinity runs, no memory is written. Doubles as the composer test surface
//! (raw output, model, generation_id are all exposed).
//!
//! Spec: docs/superpowers/specs/2026-08-03-image-force-and-compose-endpoint-design.md

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_llm::model_config::StyleKey;
use eros_engine_store::persona::PersonaRepo;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::pipeline::handlers::compose_image_prompt;
use crate::pipeline::stream::run_image_prompt_compose;
use crate::routes::companion_stream::aspect_ratio_supported;
use crate::state::AppState;

/// Same cap as the chat/voice `content`.
const MAX_CONTENT_CHARS: usize = 4096;
/// Roomy enough to paste a real transcript slice (the chat path feeds the
/// composer 8 rows) without becoming an unbounded prompt-injection surface.
const MAX_SCENE_CHARS: usize = 8192;
const CONCURRENT_STREAMS_PER_USER: u32 = 3;

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ComposeRequest {
    /// Lands in the `[对方最新消息]` slot. Required, non-empty after trim.
    pub content: String,
    /// Lands in the `[最近场景]` slot. Omitted or blank ⇒ `（无）`. A composer
    /// *input*, not the prompt: the composer reads it and writes its own
    /// subject — there is no verbatim injection channel.
    #[serde(default)]
    pub scene: Option<String>,
    /// Same three presets as the chat path; default `realistic`.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub style: Option<StyleKey>,
    /// Same allow-list as the chat path.
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    /// Same `filter_prompt` variant selection as the chat path, including the
    /// unknown-key-falls-back-to-built-in rule.
    #[serde(default)]
    pub prompt_variant: Option<String>,
    /// Default `true` (spec 2026-08-03 §1).
    #[serde(default = "default_true")]
    pub stream: bool,
}

/// The five fields both modes return (`stream: true` carries them on the
/// terminal `done` frame, byte-identical minus the `type` discriminator).
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ComposeResponse {
    /// Style preset + persona appearance + subject — the string to hand an
    /// image vendor.
    pub composed_prompt: String,
    /// The composer's own prompt field, before assembly. A successful-but-
    /// non-JSON composer reply becomes this whole field (spec §3.5).
    pub subject: String,
    /// The composer's short caption; `null` when it produced none.
    pub caption: Option<String>,
    /// The model that actually answered.
    pub model: String,
    /// For reconciling against provider logs.
    pub generation_id: Option<String>,
}

fn pre(status: StatusCode, code: &'static str, message: &str, user_message: &str) -> AppError {
    AppError::StreamPre(StreamPreError {
        status,
        code,
        message: message.into(),
        user_message: user_message.into(),
        original_user_message_id: None,
    })
}

fn validate(req: &ComposeRequest) -> Result<(), AppError> {
    if req.content.trim().is_empty() {
        return Err(pre(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            "content must not be blank",
            "请输入内容",
        ));
    }
    if req.content.chars().count() > MAX_CONTENT_CHARS {
        return Err(pre(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unprocessable",
            "content exceeds 4096 chars",
            "内容过长，请缩短后重试",
        ));
    }
    if let Some(scene) = req.scene.as_deref() {
        if scene.chars().count() > MAX_SCENE_CHARS {
            return Err(pre(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unprocessable",
                "scene exceeds 8192 chars",
                "场景过长，请缩短后重试",
            ));
        }
    }
    if let Some(ar) = req.aspect_ratio.as_deref() {
        if !aspect_ratio_supported(ar) {
            return Err(pre(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unprocessable",
                "unsupported aspect_ratio",
                "不支持的画幅比例",
            ));
        }
    }
    Ok(())
}

#[utoipa::path(
    post,
    path = "/persona/{instance_id}/image/compose",
    tag = "persona",
    params(("instance_id" = Uuid, Path, description = "Persona instance id (must belong to the JWT user)")),
    request_body = ComposeRequest,
    responses(
        (status = 200, description = "With `stream: false`, one JSON body (ComposeResponse). \
            With `stream: true` (the default), `text/event-stream`: `delta` frames carrying the \
            composer's raw output verbatim, then one terminal `done` frame whose payload minus \
            the `type` discriminator is byte-identical to the `stream: false` body — or a single \
            in-band `error` frame after streaming has begun.", body = ComposeResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 404, description = "instance not found"),
        (status = 422, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 429, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 501, body = crate::routes::companion_stream::StreamPreErrorBody),
        (status = 502, description = "composer chain exhausted before any output — \
            `{\"error\": \"upstream\", \"message\": …}`. No portrait fallback: this endpoint \
            has no chat turn to protect."),
    ),
    security(("bearer" = []))
)]
pub async fn compose_image(
    State(state): State<AppState>,
    Path(instance_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<ComposeRequest>,
) -> Result<axum::response::Response, AppError> {
    validate(&req)?;

    let persona_repo = PersonaRepo { pool: &state.pool };
    let persona = persona_repo
        .load_companion(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("instance not found".into()))?;
    if persona.instance.owner_uid != user_id {
        return Err(pre(
            StatusCode::FORBIDDEN,
            "instance_forbidden",
            "instance not owned by JWT user",
            "无权访问该角色",
        ));
    }

    // Opt-in: no [tasks.chat_image_prompt_compose] ⇒ 501 (mirrors the voice
    // endpoint's absent [tasks.chat_voice]). `has_task`, NOT the resolver —
    // resolving advances the round-robin model cursor as a side effect, and a
    // refused request must not skew it.
    if !state.model_config.has_task("chat_image_prompt_compose") {
        return Err(pre(
            StatusCode::NOT_IMPLEMENTED,
            "compose_disabled",
            "[tasks.chat_image_prompt_compose] is not configured on this deployment",
            "该服务未启用图片提示词合成",
        ));
    }

    // Shared with chat/voice: the composer is an LLM entry point any
    // authenticated user can trigger, bounded by the same per-user pool.
    let _guard = state
        .stream_slots
        .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
        .ok_or_else(|| {
            pre(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "per-user stream cap reached",
                "请求过多，请稍后再试",
            )
        })?;

    let resolved = state
        .model_config
        .resolve_image_prompt_compose(req.prompt_variant.as_deref())
        .expect("has_task checked above");
    let content = req.content.trim().to_string();
    // Blank ⇒ "" here; run_image_prompt_compose renders the empty slot as （无）.
    let scene = req
        .scene
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let style_key = req.style.unwrap_or_default();
    // Same serde-derived slot string as build_delegated_image_prompt.
    let style_str = serde_json::to_value(style_key)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "realistic".to_string());

    if req.stream {
        return Err(AppError::BadRequest(
            "stream mode lands in the next commit".into(),
        ));
    }

    // Walks [model] + fallback; usage is logged inside (§3.7). `None` after
    // the whole chain ⇒ 502 — no portrait fallback on this endpoint (§3.6).
    let outcome = run_image_prompt_compose(
        &state,
        &resolved,
        &persona,
        &scene,
        &content,
        req.aspect_ratio.as_deref(),
        &style_str,
    )
    .await
    .ok_or_else(|| AppError::Upstream("image composer chain exhausted".into()))?;

    let composed_prompt = compose_image_prompt(style_key, &persona, &outcome.prompt);
    Ok(Json(ComposeResponse {
        composed_prompt,
        subject: outcome.prompt,
        caption: outcome.caption,
        model: outcome.model,
        generation_id: outcome.generation_id,
    })
    .into_response())
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(compose_image))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request};
    use axum::Router;
    use eros_engine_llm::model_config::ModelConfig;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;
    use sqlx::PgPool;
    use std::sync::Arc;
    use tower::Service;
    use wiremock::matchers::path as wm_path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mint_jwt(uid: Uuid) -> String {
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp();
        encode(
            &Header::default(),
            &json!({ "sub": uid.to_string(), "exp": exp }),
            &EncodingKey::from_secret(crate::routes::companion::TEST_SECRET.as_ref()),
        )
        .unwrap()
    }

    fn build_router(state: AppState) -> Router {
        let (axum, _api) = crate::routes::router(state.clone()).split_for_parts();
        axum.with_state(state)
    }

    /// Seed a genome (with an appearance, so `composed_prompt` assembly is
    /// observable) + an instance owned by `owner`.
    async fn seed_instance(pool: &PgPool, owner: Uuid) -> Uuid {
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('P', 'p', '{\"appearance\": \"银发红瞳\"}'::jsonb) RETURNING id",
        )
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

    fn with_composer(mut state: AppState, mock_uri: &str) -> AppState {
        state.model_config = Arc::new(
            ModelConfig::from_toml_str("[tasks.chat_image_prompt_compose]\nmodel = \"composer\"\n")
                .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{mock_uri}/api/v1/chat/completions"),
            ),
        );
        state
    }

    async fn post_compose(
        app: &mut Router,
        instance_id: Uuid,
        jwt: &str,
        body: serde_json::Value,
    ) -> axum::http::Response<Body> {
        let req = Request::builder()
            .method("POST")
            .uri(format!("/persona/{instance_id}/image/compose"))
            .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.call(req).await.unwrap()
    }

    async fn body_json(resp: axum::http::Response<Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// A successful composer mock returning a well-formed JSON reply.
    async fn mount_json_composer(mock: &MockServer) {
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-standalone",
                "model": "served/composer-model",
                "choices": [{"message": {"content":
                    r#"{"prompt":"STANDALONE SUBJECT","caption":"一张图"}"#}}],
            })))
            .mount(mock)
            .await;
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_501_when_task_absent(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        // Default test_state has no chat_image_prompt_compose task.
        let mut app = build_router(crate::routes::companion::test_state(pool));
        let jwt = mint_jwt(user_id);
        let resp = post_compose(
            &mut app,
            instance_id,
            &jwt,
            json!({"content": "在海边", "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_404_when_instance_missing(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let mock = MockServer::start().await;
        let mut app = build_router(with_composer(
            crate::routes::companion::test_state(pool),
            &mock.uri(),
        ));
        let jwt = mint_jwt(user_id);
        let resp = post_compose(
            &mut app,
            Uuid::new_v4(),
            &jwt,
            json!({"content": "在海边", "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_403_when_not_owner(pool: PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_instance(&pool, owner).await;
        let mock = MockServer::start().await;
        let mut app = build_router(with_composer(
            crate::routes::companion::test_state(pool),
            &mock.uri(),
        ));
        let other = mint_jwt(Uuid::new_v4());
        let resp = post_compose(
            &mut app,
            instance_id,
            &other,
            json!({"content": "在海边", "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_422_when_content_blank(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        let mock = MockServer::start().await;
        let mut app = build_router(with_composer(
            crate::routes::companion::test_state(pool),
            &mock.uri(),
        ));
        let jwt = mint_jwt(user_id);
        let resp = post_compose(
            &mut app,
            instance_id,
            &jwt,
            json!({"content": "  ", "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_422_when_scene_over_cap(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        let mock = MockServer::start().await;
        let mut app = build_router(with_composer(
            crate::routes::companion::test_state(pool),
            &mock.uri(),
        ));
        let jwt = mint_jwt(user_id);
        let resp = post_compose(
            &mut app,
            instance_id,
            &jwt,
            json!({"content": "在海边", "scene": "x".repeat(8193), "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_422_when_aspect_unsupported(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        let mock = MockServer::start().await;
        let mut app = build_router(with_composer(
            crate::routes::companion::test_state(pool),
            &mock.uri(),
        ));
        let jwt = mint_jwt(user_id);
        let resp = post_compose(
            &mut app,
            instance_id,
            &jwt,
            json!({"content": "在海边", "aspect_ratio": "2:5", "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_429_over_cap(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        let mock = MockServer::start().await;
        let state = with_composer(crate::routes::companion::test_state(pool), &mock.uri());
        // Fill the shared per-user pool before the request arrives.
        let _g1 = state.stream_slots.try_acquire(user_id, 3).unwrap();
        let _g2 = state.stream_slots.try_acquire(user_id, 3).unwrap();
        let _g3 = state.stream_slots.try_acquire(user_id, 3).unwrap();
        let mut app = build_router(state);
        let jwt = mint_jwt(user_id);
        let resp = post_compose(
            &mut app,
            instance_id,
            &jwt,
            json!({"content": "在海边", "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_returns_five_fields_and_assembled_prompt(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_json_composer(&mock).await;
        let mut app = build_router(with_composer(
            crate::routes::companion::test_state(pool),
            &mock.uri(),
        ));
        let jwt = mint_jwt(user_id);
        let resp = post_compose(
            &mut app,
            instance_id,
            &jwt,
            json!({"content": "在海边，黄昏", "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["subject"], "STANDALONE SUBJECT");
        assert_eq!(v["caption"], "一张图");
        assert_eq!(v["model"], "served/composer-model");
        assert_eq!(v["generation_id"], "gen-standalone");
        let composed = v["composed_prompt"].as_str().unwrap();
        assert!(
            composed.contains("STANDALONE SUBJECT"),
            "composed_prompt carries the subject: {composed}"
        );
        assert!(
            composed.contains("银发红瞳"),
            "composed_prompt carries the persona appearance: {composed}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_non_json_reply_becomes_subject_with_null_caption(pool: PgPool) {
        // spec §3.5: same behaviour as the chat path so the two cannot disagree.
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "PLAIN TEXT PROMPT, no json"}}],
            })))
            .mount(&mock)
            .await;
        let mut app = build_router(with_composer(
            crate::routes::companion::test_state(pool),
            &mock.uri(),
        ));
        let jwt = mint_jwt(user_id);
        let resp = post_compose(
            &mut app,
            instance_id,
            &jwt,
            json!({"content": "在海边", "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["subject"], "PLAIN TEXT PROMPT, no json");
        assert_eq!(v["caption"], serde_json::Value::Null);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_scene_slot_renders_none_marker_when_omitted(pool: PgPool) {
        // The mirror of the chat path's forced_image_without_pde_still_feeds_the_scene:
        // scene omitted ⇒ （无） in the payload; scene supplied ⇒ verbatim slot.
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        let mock = MockServer::start().await;
        mount_json_composer(&mock).await;
        let mut app = build_router(with_composer(
            crate::routes::companion::test_state(pool),
            &mock.uri(),
        ));
        let jwt = mint_jwt(user_id);

        let resp = post_compose(
            &mut app,
            instance_id,
            &jwt,
            json!({"content": "在海边，黄昏", "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = post_compose(
            &mut app,
            instance_id,
            &jwt,
            json!({"content": "在海边，黄昏", "scene": "两人在天台看日落", "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let reqs = mock.received_requests().await.expect("recorded requests");
        assert_eq!(reqs.len(), 2);
        let payload_of = |i: usize| -> String {
            let body: serde_json::Value = serde_json::from_slice(&reqs[i].body).unwrap();
            body["messages"][1]["content"].as_str().unwrap().to_string()
        };
        let first = payload_of(0);
        assert!(
            first.contains("[最近场景]\n（无）"),
            "omitted scene renders （无）: {first}"
        );
        assert!(
            first.contains("[对方最新消息]\n在海边，黄昏"),
            "content lands in its slot: {first}"
        );
        let second = payload_of(1);
        assert!(
            second.contains("[最近场景]\n两人在天台看日落"),
            "supplied scene lands in its slot: {second}"
        );
        assert!(
            !second.contains("（无）"),
            "no empty-slot marker when scene is supplied: {second}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_502_when_chain_exhausted(pool: PgPool) {
        // spec §3.6: no portrait fallback here — the fallback exists to keep a
        // chat turn moving, and this endpoint has no turn to protect.
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;
        let mut app = build_router(with_composer(
            crate::routes::companion::test_state(pool),
            &mock.uri(),
        ));
        let jwt = mint_jwt(user_id);
        let resp = post_compose(
            &mut app,
            instance_id,
            &jwt,
            json!({"content": "在海边", "stream": false}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let v = body_json(resp).await;
        assert_eq!(v["error"], "upstream");
    }
}

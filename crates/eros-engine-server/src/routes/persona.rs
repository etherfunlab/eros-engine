// SPDX-License-Identifier: AGPL-3.0-only
//! POST /persona/{instance_id}/image/compose — persona-scoped standalone
//! image-prompt composition. Not a chat turn: nothing is persisted, no
//! affinity runs, no memory is written. Doubles as the composer test surface
//! (raw output, model, generation_id are all exposed).
//!
//! Spec: docs/superpowers/specs/2026-08-03-image-force-and-compose-endpoint-design.md

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_llm::model_config::StyleKey;
use eros_engine_store::persona::PersonaRepo;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::pipeline::handlers::compose_image_prompt;
use crate::pipeline::stream::{
    compose_user_payload, parse_compose_reply, run_image_prompt_compose, StreamErrorCode,
    FILTER_TIMEOUT,
};
use crate::routes::companion_stream::aspect_ratio_supported;
use crate::state::{AppState, StreamSlotGuard};

/// Same cap as the chat/voice `content`.
const MAX_CONTENT_CHARS: usize = 4096;
/// Roomy enough to paste a real transcript slice (the chat path feeds the
/// composer 8 rows) without becoming an unbounded prompt-injection surface.
const MAX_SCENE_CHARS: usize = 8192;
const CONCURRENT_STREAMS_PER_USER: u32 = 3;
const SSE_KEEPALIVE_SECS: u64 = 15;

fn default_true() -> bool {
    true
}

/// Wire frames for the compose SSE mode. Names deliberately reuse the chat
/// stream's `delta` / `done` / `error` vocabulary (spec 2026-08-03 §3.4);
/// there is no `meta` frame — `model` rides the terminal frame. `Done` minus
/// the `type` discriminator is byte-identical to the `stream: false` body.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ComposeFrame {
    /// The composer's raw output as it arrives, verbatim and unparsed — this
    /// is what makes the endpoint usable for diagnosing a `filter_prompt`
    /// whose model emits malformed JSON.
    Delta { content: String },
    Done {
        composed_prompt: String,
        subject: String,
        caption: Option<String>,
        model: String,
        generation_id: Option<String>,
    },
    Error {
        code: StreamErrorCode,
        retryable: bool,
        message: String,
        user_message: String,
    },
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ComposeRequest {
    /// Lands in the `[对方最新消息]` slot. Required, non-empty after trim.
    pub content: String,
    /// Lands in the `[最近场景]` slot. Omitted or blank ⇒ `（无）`. A composer
    /// *input*, not the prompt: the engine never copies it into
    /// `composed_prompt` — only the composer's own output is assembled. That
    /// is a routing property, not sanitization: the composer is a language
    /// model reading caller-supplied text, so it can be steered by this slot
    /// and can echo it back through the deltas and `subject`. Treat the output
    /// as model-generated, not as trusted.
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
        return compose_stream(
            state,
            persona,
            resolved,
            style_key,
            &style_str,
            &scene,
            &content,
            req.aspect_ratio.as_deref(),
            _guard,
        )
        .await;
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

/// A composer candidate that has proven itself by emitting its first content
/// chunk. Carries that chunk (already consumed from `stream`, so it must be
/// re-emitted) plus whatever audit fields the leading chunks latched.
struct Opened {
    /// The model id this attempt was sent as — the `done` frame's fallback
    /// when the provider never echoes a served model.
    attempted_model: String,
    first: String,
    served_model: Option<String>,
    generation_id: Option<String>,
    usage: Option<eros_engine_llm::openrouter::UsageBlock>,
    stream: eros_engine_llm::openrouter::DeltaStream,
    /// This candidate's remaining budget, shared by the open, the peek, and
    /// the rest of the consumption.
    deadline: tokio::time::Instant,
}

/// Stream mode. The composer chain (`[model] + fallback`) is walked BEFORE the
/// SSE response is constructed, and a candidate only counts as opened once it
/// has produced its first content chunk — so a fully-failed chain is a real
/// HTTP 502 and only a death after that first chunk becomes the in-band
/// `error` frame (spec §3.6). Mirrors `run_image_prompt_compose`'s request
/// shape so both modes hit the composer identically.
#[allow(clippy::too_many_arguments)]
async fn compose_stream(
    state: AppState,
    persona: eros_engine_core::persona::CompanionPersona,
    resolved: eros_engine_llm::model_config::ResolvedImagePromptCompose,
    style_key: StyleKey,
    style_str: &str,
    scene: &str,
    content: &str,
    aspect_ratio: Option<&str>,
    guard: StreamSlotGuard,
) -> Result<axum::response::Response, AppError> {
    use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
    let appearance = crate::prompt::meta_str(&persona, "appearance")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("（无）");
    let scene_slot = if scene.is_empty() { "（无）" } else { scene };
    let ar = aspect_ratio.unwrap_or("（未指定）");
    let user_payload = compose_user_payload(appearance, scene_slot, content, style_str, ar);
    // The model on the wire comes from the per-candidate `execute_stream_as`
    // argument, not this field.
    let req = ChatRequest {
        model: resolved.model.clone(),
        fallback_model: vec![],
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: resolved.compose_prompt.clone(),
            },
            ChatMessage {
                role: "user".into(),
                content: user_payload,
            },
        ],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        sampling: resolved.sampling,
        reasoning: resolved.reasoning.clone(),
        task: Some("chat_image_prompt_compose".into()),
        ..Default::default()
    };

    let chain: Vec<String> = std::iter::once(resolved.model.clone())
        .chain(resolved.fallback_model.iter().cloned())
        .collect();
    // A candidate counts as opened only once it has produced its FIRST content
    // chunk. Merely getting a 200 back is not enough: a provider that accepts
    // the call and then errors or EOFs without a token has produced nothing
    // usable, and the non-stream mode would advance the chain on exactly that
    // (its "empty reply; next" arm). Peeking here keeps the two modes'
    // fallback behaviour identical and keeps a fully-failed chain a real
    // pre-stream 502 rather than a 200 carrying an error frame.
    let mut opened: Option<Opened> = None;
    for model_id in chain {
        // One budget per candidate, covering both the open and the whole
        // consumption — the composer writes a short JSON reply, so the
        // non-stream mode's per-call FILTER_TIMEOUT is the right total here.
        let deadline = tokio::time::Instant::now() + FILTER_TIMEOUT;
        let mut stream = match tokio::time::timeout_at(
            deadline,
            state.openrouter.execute_stream_as(&req, &model_id),
        )
        .await
        {
            Ok(Ok(ds)) => ds,
            Ok(Err(e)) => {
                tracing::warn!(model = %model_id, error = %e, "compose endpoint: stream open failed; next");
                continue;
            }
            Err(_) => {
                tracing::warn!(model = %model_id, "compose endpoint: stream open timeout; next");
                continue;
            }
        };
        let mut first: Option<String> = None;
        let mut served_model: Option<String> = None;
        let mut generation_id: Option<String> = None;
        let mut usage: Option<eros_engine_llm::openrouter::UsageBlock> = None;
        while first.is_none() {
            match tokio::time::timeout_at(deadline, futures_util::StreamExt::next(&mut stream))
                .await
            {
                Ok(Some(Ok(chunk))) => {
                    if let Some(m) = chunk.model {
                        served_model = Some(m);
                    }
                    if let Some(g) = chunk.generation_id {
                        generation_id = Some(g);
                    }
                    if let Some(u) = chunk.usage {
                        usage = Some(u);
                    }
                    if let Some(c) = chunk.content {
                        if !c.is_empty() {
                            first = Some(c);
                        }
                    }
                }
                Ok(Some(Err(e))) => {
                    tracing::warn!(model = %model_id, error = %e, "compose endpoint: stream died before first token; next");
                    break;
                }
                Ok(None) => {
                    tracing::warn!(model = %model_id, "compose endpoint: stream ended with no content; next");
                    break;
                }
                Err(_) => {
                    tracing::warn!(model = %model_id, "compose endpoint: timeout before first token; next");
                    break;
                }
            }
        }
        if let Some(first) = first {
            opened = Some(Opened {
                attempted_model: model_id,
                first,
                served_model,
                generation_id,
                usage,
                stream,
                deadline,
            });
            break;
        }
    }
    let Opened {
        attempted_model,
        first,
        mut served_model,
        mut generation_id,
        mut usage,
        stream: mut delta_stream,
        deadline,
    } = opened.ok_or_else(|| AppError::Upstream("image composer chain exhausted".into()))?;

    let frames = async_stream::stream! {
        let _guard = guard;
        let mut acc = first.clone();
        // The peeked chunk is real output — emit it before resuming the stream.
        yield ComposeFrame::Delta { content: first };
        // `failure` is Some once this attempt can no longer produce a `done`.
        // There is no chain left to walk mid-stream, so it becomes an in-band
        // error frame instead of a status code.
        let mut failure: Option<String> = None;
        loop {
            match tokio::time::timeout_at(deadline, futures_util::StreamExt::next(&mut delta_stream)).await {
                Ok(Some(Ok(chunk))) => {
                    if let Some(m) = chunk.model {
                        served_model = Some(m);
                    }
                    if let Some(g) = chunk.generation_id {
                        generation_id = Some(g);
                    }
                    if let Some(u) = chunk.usage {
                        usage = Some(u);
                    }
                    if let Some(c) = chunk.content {
                        if !c.is_empty() {
                            acc.push_str(&c);
                            yield ComposeFrame::Delta { content: c };
                        }
                    }
                }
                Ok(Some(Err(e))) => {
                    tracing::warn!(error = %e, "compose endpoint: stream died mid-flight");
                    failure = Some(format!("composer stream failed: {e}"));
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    // Bounds a provider that opens the response and then goes
                    // quiet: without this the request — and the per-user slot
                    // guard held by this stream — would live until the client
                    // gave up. Mirrors the chat path's STREAM_TOTAL_TIMEOUT.
                    tracing::warn!("compose endpoint: total timeout while streaming");
                    failure = Some("composer stream timed out".into());
                    break;
                }
            }
        }
        // Usage is logged on EVERY terminal path, failures included: a stream
        // that died mid-flight may still have been billed, and reconciling
        // that is this endpoint's job. Same synthesis as the chat path's
        // streaming arms.
        crate::pipeline::log_openrouter_usage(
            "chat_image_prompt_compose",
            None,
            &eros_engine_llm::openrouter::ChatResponse {
                reply: String::new(), // usage log only — never echo content
                generation_id: generation_id.clone(),
                model: served_model.clone(),
                usage: usage.as_ref().and_then(|u| serde_json::to_value(u).ok()),
                finish_reason: None,
            },
        );
        let (subject, caption) = parse_compose_reply(acc.trim());
        match failure {
            Some(message) => yield ComposeFrame::Error {
                code: StreamErrorCode::UpstreamUnavailable,
                retryable: true,
                message,
                user_message: "服务出现问题，请稍后再试".into(),
            },
            // A parse that yields no subject cannot be served as a `done`.
            None if subject.is_empty() => yield ComposeFrame::Error {
                code: StreamErrorCode::UpstreamUnavailable,
                retryable: true,
                message: "composer returned no usable prompt".into(),
                user_message: "服务出现问题，请稍后再试".into(),
            },
            None => {
                let composed_prompt = compose_image_prompt(style_key, &persona, &subject);
                yield ComposeFrame::Done {
                    composed_prompt,
                    subject,
                    caption,
                    model: served_model.unwrap_or(attempted_model),
                    generation_id,
                };
            }
        }
    };
    let sse = futures_util::StreamExt::map(frames, |f: ComposeFrame| {
        let json = serde_json::to_string(&f).expect("ComposeFrame serialization is infallible");
        Ok::<_, std::convert::Infallible>(Event::default().data(json))
    });
    Ok(Sse::new(sse)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(SSE_KEEPALIVE_SECS))
                .text("ping"),
        )
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
    use wiremock::matchers::{body_string_contains, path as wm_path};
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
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
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

    /// Collect the `data:` frames out of an SSE body (keep-alive comment lines
    /// are skipped by the `data: ` prefix filter).
    async fn sse_frames(resp: axum::http::Response<Body>) -> Vec<serde_json::Value> {
        let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        text.lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .map(|d| serde_json::from_str(d).unwrap())
            .collect()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_stream_deltas_then_done_matches_nonstream_body(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;

        // The composer's raw reply, split across two SSE chunks (ASCII cut).
        let reply = r#"{"prompt":"STREAMED SUBJECT","caption":"一张流式图"}"#;
        let (a, b) = reply.split_at(14);
        let chunk1 = json!({"choices": [{"delta": {"content": a}}]});
        let chunk2 = json!({
            "choices": [{"delta": {"content": b}}],
            "usage": {"prompt_tokens": 2, "completion_tokens": 9, "total_tokens": 11},
            "id": "gen-stream",
            "model": "served/composer-model",
        });
        let sse_body = format!("data: {chunk1}\n\ndata: {chunk2}\n\ndata: [DONE]\n\n");
        let stream_mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&stream_mock)
            .await;

        let mut app = build_router(with_composer(
            crate::routes::companion::test_state(pool.clone()),
            &stream_mock.uri(),
        ));
        let jwt = mint_jwt(user_id);
        // `stream` omitted — pins the default-true decision (spec §1).
        let resp = post_compose(
            &mut app,
            instance_id,
            &jwt,
            json!({"content": "在海边，黄昏"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/event-stream"),
            "default mode is SSE"
        );
        let frames = sse_frames(resp).await;
        let deltas: String = frames
            .iter()
            .filter(|f| f["type"] == "delta")
            .map(|f| f["content"].as_str().unwrap())
            .collect();
        assert_eq!(deltas, reply, "raw composer output passes through verbatim");
        let done: Vec<_> = frames.iter().filter(|f| f["type"] == "done").collect();
        assert_eq!(done.len(), 1, "exactly one terminal done: {frames:?}");
        assert_eq!(
            frames.last().unwrap()["type"],
            "done",
            "done is terminal: {frames:?}"
        );
        let mut done_obj = done[0].clone();
        done_obj.as_object_mut().unwrap().remove("type");

        // Non-stream twin against a JSON mock carrying the same reply/model/id:
        // the done frame minus the discriminator must equal the stream:false body.
        let json_mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-stream",
                "model": "served/composer-model",
                "choices": [{"message": {"content": reply}}],
            })))
            .mount(&json_mock)
            .await;
        let mut app2 = build_router(with_composer(
            crate::routes::companion::test_state(pool),
            &json_mock.uri(),
        ));
        let resp2 = post_compose(
            &mut app2,
            instance_id,
            &jwt,
            json!({"content": "在海边，黄昏", "stream": false}),
        )
        .await;
        assert_eq!(resp2.status(), StatusCode::OK);
        let v2 = body_json(resp2).await;
        assert_eq!(
            done_obj, v2,
            "done minus the type discriminator equals the stream:false body"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_stream_non_json_reply_passes_through_raw(pool: PgPool) {
        // spec §3.5's stream twin: a successful-but-non-JSON reply streams out
        // verbatim and becomes the whole subject with a null caption.
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        let chunk = json!({
            "choices": [{"delta": {"content": "PLAIN STREAM PROMPT"}}],
            "id": "gen-raw",
            "model": "served/composer-model",
        });
        let sse_body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;
        let mut app = build_router(with_composer(
            crate::routes::companion::test_state(pool),
            &mock.uri(),
        ));
        let jwt = mint_jwt(user_id);
        let resp = post_compose(&mut app, instance_id, &jwt, json!({"content": "在海边"})).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let frames = sse_frames(resp).await;
        let done = frames
            .iter()
            .find(|f| f["type"] == "done")
            .expect("done frame");
        assert_eq!(done["subject"], "PLAIN STREAM PROMPT");
        assert_eq!(done["caption"], serde_json::Value::Null);
        let deltas: String = frames
            .iter()
            .filter(|f| f["type"] == "delta")
            .map(|f| f["content"].as_str().unwrap())
            .collect();
        assert_eq!(deltas, "PLAIN STREAM PROMPT");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_stream_advances_chain_when_candidate_yields_no_content(pool: PgPool) {
        // Codex review (PR #220, P1): a 200 that carries no token is not a
        // usable candidate. The non-stream mode advances its chain on exactly
        // that ("empty reply; next"), so the stream mode must too — otherwise
        // a configured fallback is silently skipped and the caller gets a 200
        // error frame instead of the fallback's real output.
        let user_id = Uuid::new_v4();
        let instance_id = seed_instance(&pool, user_id).await;
        let mock = MockServer::start().await;
        // Primary: 200, well-formed SSE, but zero content chunks.
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("\"model\":\"composer\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw("data: [DONE]\n\n", "text/event-stream"),
            )
            .mount(&mock)
            .await;
        // Fallback: the real answer.
        let chunk = json!({
            "choices": [{"delta": {"content": r#"{"prompt":"FALLBACK SUBJECT"}"#}}],
            "id": "gen-fallback",
            "model": "served/fallback-model",
        });
        Mock::given(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("\"model\":\"backup\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
                        "text/event-stream",
                    ),
            )
            .mount(&mock)
            .await;

        let mut state = crate::routes::companion::test_state(pool);
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_image_prompt_compose]\nmodel = \"composer\"\nfallback = [\"backup\"]\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        let mut app = build_router(state);
        let jwt = mint_jwt(user_id);
        let resp = post_compose(&mut app, instance_id, &jwt, json!({"content": "在海边"})).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let frames = sse_frames(resp).await;
        let done = frames
            .iter()
            .find(|f| f["type"] == "done")
            .expect("fallback served a done frame");
        assert_eq!(done["subject"], "FALLBACK SUBJECT");
        assert_eq!(done["model"], "served/fallback-model");
        assert!(
            !frames.iter().any(|f| f["type"] == "error"),
            "no error frame once the fallback served: {frames:?}"
        );
        let reqs = mock.received_requests().await.expect("recorded requests");
        assert_eq!(reqs.len(), 2, "both chain candidates were tried");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn compose_stream_502_when_chain_exhausted(pool: PgPool) {
        // The chain is walked while OPENING the stream, before the SSE response
        // exists — total failure must be a real HTTP 502 even in stream mode.
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
        let resp = post_compose(&mut app, instance_id, &jwt, json!({"content": "在海边"})).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let v = body_json(resp).await;
        assert_eq!(v["error"], "upstream");
    }
}

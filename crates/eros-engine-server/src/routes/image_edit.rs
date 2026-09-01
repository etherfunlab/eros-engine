// SPDX-License-Identifier: AGPL-3.0-only
//! `POST /v2/comp/session/{session_id}/message/{message_id}/image/edit`
//!
//! Revise a picture the character already sent: the consumer names the image
//! turn and says what to change ("换套衣服"), and the engine composes a new
//! image prompt from that picture's subject and force-emits an image turn for
//! it. The engine composes; the consumer draws — the delegation contract from
//! v0.7.1 is unchanged, and the persisted row is an ordinary image turn, so
//! history discovery and the v1 recovery endpoint work on it unmodified.
//!
//! With `persist_instruction` the instruction additionally lands as an
//! ordinary `role='user'` row quoting the source turn, and the new image row
//! hangs off it — after which the whole turn is shape-identical to a chat-path
//! image turn.
//!
//! Nothing else about a turn runs here: no PDE verdict, no affinity, no
//! insight or memory extraction, no queue. A persisted instruction row is
//! visible to later turns' pipelines like any user message; this call
//! triggers none of them.

use axum::extract::{Extension, Path, State};
use axum::Json;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use ulid::Ulid;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_llm::model_config::StyleKey;
use eros_engine_store::chat::{AssistantInsert, ChatRepo};
use eros_engine_store::image_events::ImageComposeEventInsert;
use eros_engine_store::persona::PersonaRepo;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::pipeline::handlers::compose_image_prompt;
use crate::pipeline::stream::{
    build_delegated_image_marker, record_compose_event, run_image_prompt_compose, split_failures,
};
use crate::routes::companion::require_session_for_user;
use crate::routes::companion_stream::aspect_ratio_supported;
use crate::routes::persona::compose_chain_exhausted;
use crate::state::AppState;

/// Wire task name; also the config block name.
const EDIT_TASK: &str = "chat_image_edit_compose";
/// Same ceiling as the standalone composer's `content`.
const MAX_INSTRUCTION_CHARS: usize = 4096;
/// Same per-user in-flight cap as every other LLM entry point.
const CONCURRENT_STREAMS_PER_USER: u32 = 3;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ImageEditRequest {
    /// What to change, in the user's words ("换套衣服"). Required, non-blank.
    pub instruction: String,
    /// Style preset for the new picture. Default `realistic`; pass the style
    /// the source was drawn with — the engine does not record it.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub style: Option<StyleKey>,
    /// Same allow-list as the chat path. Defaults to the source turn's
    /// `aspect_ratio`.
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    /// `[tasks.chat_image_edit_compose].filter_prompt` variant, with the same
    /// selection rules as the chat path's `image.prompt_variant`.
    #[serde(default)]
    pub prompt_variant: Option<String>,
    /// Persist the instruction as a `role='user'` chat message quoting the
    /// source image turn (`metadata.reply_to_message_id`), and hang the new
    /// image row off it. The persisted row is an ordinary user message —
    /// visible to companion context, memory extraction and later affinity
    /// evaluation like anything else the user said. Default false: the
    /// audit-only contract, byte-for-byte.
    #[serde(default)]
    pub persist_instruction: bool,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ImageEditResponse {
    /// The new assistant message.
    #[schema(value_type = String)]
    pub message_id: Uuid,
    /// The image turn this revises.
    #[schema(value_type = String)]
    pub edit_of: Uuid,
    /// Base64 (STANDARD) of the UTF-8 composed prompt — the same encoding as
    /// the SSE `image_request` frame, so an existing draw path consumes it
    /// unchanged.
    pub composed_prompt: String,
    /// Always `"previous"`: on an edit turn that means the `edit_of` picture.
    pub image_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    /// The composer's caption for the new picture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    /// The persisted instruction message, when `persist_instruction` was set.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub instruction_message_id: Option<Uuid>,
}

/// The edit composer's payload. Mirrors `compose_user_payload`'s five slots,
/// with `[原图]` replacing `[最近场景]`/`[对方最新消息]`. Pure.
pub(crate) fn compose_edit_payload(
    appearance: &str,
    source_subject: &str,
    source_caption: Option<&str>,
    instruction: &str,
    style: &str,
    aspect_ratio: &str,
) -> String {
    let subject = source_subject.trim();
    let mut original = if subject.is_empty() {
        "（无）".to_string()
    } else {
        subject.to_string()
    };
    if let Some(c) = source_caption.map(str::trim).filter(|s| !s.is_empty()) {
        original.push('\n');
        original.push_str(c);
    }
    format!(
        "[人物外观]\n{appearance}\n\n[原图]\n{original}\n\n[修改要求]\n{instruction}\n\n[风格]\n{style}\n\n[画幅]\n{aspect_ratio}"
    )
}

/// `compose_edit_payload` with the persona's appearance and the placeholders
/// the prompt expects for empty slots — the edit-side twin of
/// `render_compose_payload`.
fn render_edit_payload(
    persona: &eros_engine_core::persona::CompanionPersona,
    source_subject: &str,
    source_caption: Option<&str>,
    instruction: &str,
    aspect_ratio: Option<&str>,
    style: &str,
) -> String {
    let appearance = crate::prompt::meta_str(persona, "appearance")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("（无）");
    compose_edit_payload(
        appearance,
        source_subject,
        source_caption,
        instruction,
        style,
        aspect_ratio.unwrap_or("（未指定）"),
    )
}

/// The audit row's `inputs`: seven keys, engine-supplied, never concatenated.
/// The `source` column says which shape to expect (the chat composer writes
/// five).
fn compose_edit_inputs_json(
    persona: &eros_engine_core::persona::CompanionPersona,
    source_message_id: Uuid,
    source_subject: &str,
    source_caption: Option<&str>,
    instruction: &str,
    style: &str,
    aspect_ratio: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "appearance": crate::prompt::meta_str(persona, "appearance")
            .map(str::trim)
            .unwrap_or(""),
        "source_message_id": source_message_id.to_string(),
        "source_subject": source_subject.trim(),
        "source_caption": source_caption.map(str::trim).unwrap_or(""),
        "instruction": instruction.trim(),
        "style": style,
        "aspect_ratio": aspect_ratio.unwrap_or(""),
    })
}

fn validate(req: &ImageEditRequest) -> Result<(), AppError> {
    if req.instruction.trim().is_empty() {
        return Err(AppError::Unprocessable(
            "instruction must not be blank".into(),
        ));
    }
    if req.instruction.chars().count() > MAX_INSTRUCTION_CHARS {
        return Err(AppError::Unprocessable(
            "instruction exceeds 4096 chars".into(),
        ));
    }
    if let Some(ar) = req.aspect_ratio.as_deref() {
        if !aspect_ratio_supported(ar) {
            return Err(AppError::Unprocessable("unsupported aspect_ratio".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod payload_tests {
    use super::compose_edit_payload;

    #[test]
    fn edit_payload_renders_every_slot() {
        let out = compose_edit_payload(
            "银发红瞳",
            "在天台看夕阳的少女",
            Some("在天台看夕阳"),
            "换套衣服",
            "anime",
            "3:4",
        );
        assert!(out.contains("[人物外观]\n银发红瞳"), "got {out}");
        assert!(
            out.contains("[原图]\n在天台看夕阳的少女\n在天台看夕阳"),
            "got {out}"
        );
        assert!(out.contains("[修改要求]\n换套衣服"), "got {out}");
        assert!(out.contains("[风格]\nanime"), "got {out}");
        assert!(out.contains("[画幅]\n3:4"), "got {out}");
    }

    #[test]
    fn edit_payload_omits_the_caption_line_when_absent() {
        let out = compose_edit_payload("银发红瞳", "少女", None, "换套衣服", "anime", "3:4");
        assert!(out.contains("[原图]\n少女\n\n[修改要求]"), "got {out}");

        let blank =
            compose_edit_payload("银发红瞳", "少女", Some("  "), "换套衣服", "anime", "3:4");
        assert!(blank.contains("[原图]\n少女\n\n[修改要求]"), "got {blank}");
    }

    #[test]
    fn edit_payload_placeholders_an_empty_source_subject() {
        // A source drawn through the portrait fallback has no subject; the
        // composer still works from appearance plus instruction.
        let out = compose_edit_payload("银发红瞳", "   ", None, "换套衣服", "anime", "3:4");
        assert!(out.contains("[原图]\n（无）"), "got {out}");
    }

    #[test]
    fn edit_inputs_json_has_exactly_the_seven_documented_keys() {
        // A renamed or dropped key on this audit row is a silent contract
        // break for anyone reading `engine.chat_images_events`; lock the key
        // SET so it fails here instead of shipping.
        use eros_engine_core::persona::{CompanionPersona, PersonaGenome, PersonaInstance};
        let iid = uuid::Uuid::new_v4();
        let gid = uuid::Uuid::new_v4();
        let persona = CompanionPersona {
            instance_id: iid,
            genome: PersonaGenome {
                id: gid,
                name: "Mia".into(),
                system_prompt: "You are Mia.".into(),
                tip_personality: Some("normal".into()),
                art_metadata: serde_json::json!({ "appearance": "银发红瞳" }),
            },
            instance: PersonaInstance {
                id: iid,
                genome_id: gid,
                owner_uid: uuid::Uuid::new_v4(),
                status: "active".into(),
            },
        };
        let value = super::compose_edit_inputs_json(
            &persona,
            uuid::Uuid::new_v4(),
            "在天台看夕阳的少女",
            Some("在天台看夕阳"),
            "换套衣服",
            "anime",
            Some("3:4"),
        );
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "appearance",
                "aspect_ratio",
                "instruction",
                "source_caption",
                "source_message_id",
                "source_subject",
                "style",
            ],
            "got {value}"
        );
    }
}

/// Revise an existing image turn.
///
/// By default the instruction is an input to a picture: it is recorded on the
/// audit row, never as a chat message, and the new assistant row inherits the
/// source's `user_message_id` — the edit belongs to the turn the original
/// picture answered. With `persist_instruction` the instruction is persisted
/// as a `role='user'` message quoting the source turn, and the new row hangs
/// off it — the user asked the character for a revision, and history replays
/// the exchange.
#[utoipa::path(
    post,
    path = "/v2/comp/session/{session_id}/message/{message_id}/image/edit",
    tag = "companion",
    params(
        ("session_id" = Uuid, Path, description = "Chat session id"),
        ("message_id" = Uuid, Path, description = "The image turn to revise")
    ),
    request_body = ImageEditRequest,
    responses(
        (status = 200, body = ImageEditResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, description = "not your session"),
        (status = 404, description = "session or message not found"),
        (status = 409, description = "the message is not an image turn"),
        (status = 422, description = "blank instruction or unsupported aspect_ratio"),
        (status = 429, description = "per-user in-flight cap reached"),
        (status = 501, description = "no image composer configured on this deployment"),
        (status = "5XX", description = "composer chain exhausted; the provider's own status \
            passes through, same body as the compose endpoint")
    ),
    security(("bearer" = []))
)]
async fn edit_image(
    State(state): State<AppState>,
    Path((session_id, message_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
    Json(req): Json<ImageEditRequest>,
) -> Result<Json<ImageEditResponse>, AppError> {
    // Ownership and state first, body second: a caller must not learn that
    // their aspect_ratio was invalid on a session they do not own.
    let session = require_session_for_user(&state, session_id, user_id).await?;
    let chat_repo = ChatRepo { pool: &state.pool };
    let source = chat_repo
        .message_by_id_in_session(session_id, message_id)
        .await?
        .ok_or_else(|| AppError::NotFound("no such message".into()))?;
    let marker = source
        .metadata
        .as_ref()
        .and_then(|m| m.get("image"))
        .cloned()
        .ok_or_else(|| AppError::Conflict("not an image turn".into()))?;
    // Without `persist_instruction` the edit belongs to the turn the source
    // answered, and a source with no turn has nothing to attach to (no engine
    // path produces one). With it, the new user row is the anchor and the
    // source's is never read — `None` here means exactly that branch.
    let source_user_message_id = if req.persist_instruction {
        None
    } else {
        Some(source.user_message_id.ok_or_else(|| {
            AppError::Conflict("source image turn has no originating user message".into())
        })?)
    };
    let instance_id = session
        .instance_id
        .ok_or_else(|| AppError::Conflict("session has no persona instance".into()))?;

    validate(&req)?;

    if !state.model_config.has_task(EDIT_TASK)
        && !state.model_config.has_task("chat_image_prompt_compose")
    {
        return Err(AppError::NotImplemented(
            "no image composer is configured on this deployment".into(),
        ));
    }

    // Same per-user in-flight pool as chat, voice and the compose endpoint:
    // this is one more user-triggered LLM call.
    let _guard = state
        .stream_slots
        .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
        .ok_or_else(|| AppError::TooManyRequests("per-user in-flight cap reached".into()))?;

    let persona = PersonaRepo { pool: &state.pool }
        .load_companion(instance_id)
        .await?
        .ok_or_else(|| AppError::NotFound("no such instance".into()))?;

    let source_subject = marker
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let source_caption = marker.get("caption").and_then(|v| v.as_str());
    let aspect_ratio = req.aspect_ratio.clone().or_else(|| {
        marker
            .get("aspect_ratio")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    });
    let style_key = req.style.unwrap_or_default();
    let style_str = serde_json::to_value(style_key)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "realistic".to_string());
    let instruction = req.instruction.trim().to_string();

    let resolved = state
        .model_config
        .resolve_image_edit_compose(req.prompt_variant.as_deref())
        .expect("has_task checked above");
    let run = run_image_prompt_compose(
        &state,
        Some(session_id),
        &resolved,
        &render_edit_payload(
            &persona,
            source_subject,
            source_caption,
            &instruction,
            aspect_ratio.as_deref(),
            &style_str,
        ),
        EDIT_TASK,
    )
    .await;

    let inputs = compose_edit_inputs_json(
        &persona,
        message_id,
        source_subject,
        source_caption,
        &instruction,
        &style_str,
        aspect_ratio.as_deref(),
    );
    let (llm_attempts, gateway_errors) = split_failures(&run.failures);

    let Some(outcome) = run.outcome else {
        // No portrait fallback here: an edit that ignored its instruction is
        // worse than an error, and the caller can retry.
        record_compose_event(
            &state.pool,
            ImageComposeEventInsert {
                llm_attempts,
                gateway_errors,
                source: "image_edit",
                user_id,
                instance_id: Some(instance_id),
                session_id: Some(session_id),
                status: "exhausted",
                inputs,
                subject: None,
                caption: None,
                composed_prompt: None,
                variant: resolved.variant_key.as_deref(),
                generation_id: run.last_generation_id.as_deref(),
                attempts: run.attempts,
                last_failure: run.last_failure,
            },
        )
        .await;
        return Err(compose_chain_exhausted(
            &run.failures,
            run.last_failure,
            EDIT_TASK,
        ));
    };

    let composed_prompt = compose_image_prompt(style_key, &persona, &outcome.prompt);
    let compose_event_id = record_compose_event(
        &state.pool,
        ImageComposeEventInsert {
            llm_attempts,
            gateway_errors,
            source: "image_edit",
            user_id,
            instance_id: Some(instance_id),
            session_id: Some(session_id),
            status: "ok",
            inputs,
            subject: Some(outcome.prompt.as_str()),
            caption: outcome.caption.as_deref(),
            composed_prompt: Some(composed_prompt.as_str()),
            variant: outcome.variant.as_deref(),
            generation_id: outcome.generation_id.as_deref(),
            attempts: run.attempts,
            last_failure: None,
        },
    )
    .await;

    // ULID-shaped id, like every other assistant row.
    let new_id: Uuid = Ulid::new().into();
    let image_marker = build_delegated_image_marker(
        &outcome.prompt,
        outcome.caption.as_deref(),
        aspect_ratio.as_deref(),
        outcome.variant.as_deref(),
        Some(outcome.model.as_str()),
        outcome.generation_id.as_deref(),
        compose_event_id,
        eros_engine_core::types::ImageRef::Previous,
        Some(message_id),
    );
    let insert = AssistantInsert {
        id: new_id,
        content: String::new(),
        assistant_action_type: "reply".into(),
        continues_from_message_id: None,
        truncated: false,
        generation_id: None,
        filter_audit: None,
        metadata: Some(serde_json::json!({ "image": image_marker })),
        llm_attempts: None,
        gateway_errors: None,
    };
    let instruction_message_id = match source_user_message_id {
        Some(umid) => {
            chat_repo
                .insert_assistant_batch(session_id, umid, &[insert])
                .await?;
            None
        }
        None => Some(
            chat_repo
                .insert_instruction_turn(session_id, &instruction, message_id, &insert)
                .await?,
        ),
    };

    Ok(Json(ImageEditResponse {
        message_id: new_id,
        edit_of: message_id,
        composed_prompt: base64::engine::general_purpose::STANDARD
            .encode(composed_prompt.as_bytes()),
        image_ref: "previous".into(),
        aspect_ratio,
        caption: outcome.caption,
        instruction_message_id,
    }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(edit_image))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use serde_json::json;
    use sqlx::PgPool;
    use std::sync::Arc;
    use uuid::Uuid;
    use wiremock::matchers::path as wm_path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        build_router, mint_test_jwt, seed_genome, seed_instance, seed_session, send_request,
    };

    /// State with an edit-capable composer pointed at the mock.
    fn with_composer(mut state: AppState, mock_uri: &str, toml: &str) -> AppState {
        state.model_config =
            Arc::new(eros_engine_llm::model_config::ModelConfig::from_toml_str(toml).unwrap());
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{mock_uri}/api/v1/chat/completions"),
            ),
        );
        state
    }

    const EDIT_TASK_TOML: &str = "[tasks.chat_image_edit_compose]\nmodel = \"editor\"\n";
    /// No `[tasks.chat_image_edit_compose]` — the config every existing
    /// deployment is already in. Exercises the fallback branch of
    /// `resolve_image_edit_compose` and the handler's
    /// `.expect("has_task checked above")`.
    ///
    /// The compose block carries its own `filter_prompt` so the fallback test
    /// below has something to prove was NOT used — without it, a handler that
    /// wrongly reused the chat composer's prompt would still pass.
    const COMPOSE_ONLY_TOML: &str = "[tasks.chat_image_prompt_compose]\nmodel = \"composer\"\nfilter_prompt = \"CHAT COMPOSER PROMPT MUST NOT BE USED FOR EDITS\"\n";

    /// A user row plus an assistant image turn pointing back at it. Returns
    /// (user_message_id, image_message_id).
    async fn seed_image_turn(pool: &PgPool, session_id: Uuid) -> (Uuid, Uuid) {
        let umid: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', '拍张照') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let mid: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages \
               (session_id, role, content, assistant_action_type, user_message_id, metadata) \
             VALUES ($1, 'assistant', '', 'reply', $2, $3) RETURNING id",
        )
        .bind(session_id)
        .bind(umid)
        .bind(json!({ "image": {
            "prompt": "在天台看夕阳的少女",
            "caption": "在天台看夕阳",
            "image_ref": "face",
            "aspect_ratio": "3:4"
        }}))
        .fetch_one(pool)
        .await
        .unwrap();
        (umid, mid)
    }

    async fn seed_text_turn(pool: &PgPool, session_id: Uuid) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, assistant_action_type) \
             VALUES ($1, 'assistant', '在的', 'reply') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn mount_editor(mock: &MockServer) {
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-edit",
                "model": "served/editor",
                "choices": [{"message": {"content":
                    r#"{"prompt":"EDITED SUBJECT","caption":"换了条裙子"}"#}}],
            })))
            .mount(mock)
            .await;
    }

    fn edit_req(
        session_id: Uuid,
        message_id: Uuid,
        jwt: &str,
        body: serde_json::Value,
    ) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(format!(
                "/v2/comp/session/{session_id}/message/{message_id}/image/edit"
            ))
            .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_404_when_session_unknown(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, _) = send_request(
            &mut app,
            edit_req(
                Uuid::new_v4(),
                Uuid::new_v4(),
                &token,
                json!({"instruction": "换套衣服"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_403_when_session_not_owned(pool: PgPool) {
        let owner = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        let (_, mid) = seed_image_turn(&pool, session_id).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(Uuid::new_v4()); // someone else
        let (status, _) = send_request(
            &mut app,
            edit_req(session_id, mid, &token, json!({"instruction": "换套衣服"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_404_when_message_not_in_session(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, _) = send_request(
            &mut app,
            edit_req(
                session_id,
                Uuid::new_v4(),
                &token,
                json!({"instruction": "换套衣服"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_409_when_not_an_image_turn(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let mid = seed_text_turn(&pool, session_id).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, body) = send_request(
            &mut app,
            edit_req(session_id, mid, &token, json!({"instruction": "换套衣服"})),
        )
        .await;
        // 409, NOT 404: the message was found; its state forbids the action.
        assert_eq!(status, StatusCode::CONFLICT, "body={body}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_422_on_blank_instruction_and_bad_aspect_ratio(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let (_, mid) = seed_image_turn(&pool, session_id).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, _) = send_request(
            &mut app,
            edit_req(session_id, mid, &token, json!({"instruction": "   "})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let (status, _) = send_request(
            &mut app,
            edit_req(
                session_id,
                mid,
                &token,
                json!({"instruction": "换套衣服", "aspect_ratio": "7:3"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_422_when_instruction_exceeds_max_chars(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let (_, mid) = seed_image_turn(&pool, session_id).await;

        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let too_long = "换".repeat(MAX_INSTRUCTION_CHARS + 1);
        let (status, _) = send_request(
            &mut app,
            edit_req(session_id, mid, &token, json!({"instruction": too_long})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_501_when_no_composer_configured(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let (_, mid) = seed_image_turn(&pool, session_id).await;

        // test_state's default model config has neither composer task.
        let state = test_state(pool);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, _) = send_request(
            &mut app,
            edit_req(session_id, mid, &token, json!({"instruction": "换套衣服"})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_502_writes_an_audit_row_and_no_message(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let (_, mid) = seed_image_turn(&pool, session_id).await;

        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let state = with_composer(test_state(pool.clone()), &mock.uri(), EDIT_TASK_TOML);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        // `persist_instruction` on the failing call: an exhausted composer
        // must persist neither the instruction row nor a message.
        let (status, _) = send_request(
            &mut app,
            edit_req(
                session_id,
                mid,
                &token,
                json!({"instruction": "换套衣服", "persist_instruction": true}),
            ),
        )
        .await;
        assert!(status.is_server_error(), "got {status}");

        let (n_events, exhausted): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE status = 'exhausted') \
             FROM engine.chat_images_events WHERE source = 'image_edit' AND session_id = $1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((n_events, exhausted), (1, 1));

        // No portrait fallback: nothing was persisted as a message.
        let n_msgs: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.chat_messages WHERE session_id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n_msgs, 2, "only the seeded user + image rows may exist");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_success_persists_the_turn_and_returns_the_payload(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let (umid, mid) = seed_image_turn(&pool, session_id).await;

        let mock = MockServer::start().await;
        mount_editor(&mock).await;
        let state = with_composer(test_state(pool.clone()), &mock.uri(), EDIT_TASK_TOML);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, body) = send_request(
            &mut app,
            edit_req(
                session_id,
                mid,
                &token,
                json!({"instruction": "换套衣服", "persist_instruction": false}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={body}");
        assert_eq!(body["edit_of"], json!(mid.to_string()));
        assert_eq!(body["image_ref"], json!("previous"));
        assert_eq!(body["caption"], json!("换了条裙子"));
        // Inherited from the source marker when the request omits it.
        assert_eq!(body["aspect_ratio"], json!("3:4"));
        // An explicit false is the v1 contract: no instruction row, no key.
        assert!(body.get("instruction_message_id").is_none(), "got {body}");

        use base64::Engine as _;
        let decoded = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(body["composed_prompt"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(decoded.contains("EDITED SUBJECT"), "got {decoded}");

        let new_id = Uuid::parse_str(body["message_id"].as_str().unwrap()).unwrap();
        let (content, action, row_umid, metadata): (
            String,
            Option<String>,
            Option<Uuid>,
            serde_json::Value,
        ) = sqlx::query_as(
            "SELECT content, assistant_action_type, user_message_id, metadata \
                 FROM engine.chat_messages WHERE id = $1",
        )
        .bind(new_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "");
        assert_eq!(action.as_deref(), Some("reply"));
        assert_eq!(
            row_umid,
            Some(umid),
            "the edit belongs to the source's turn"
        );
        assert_eq!(metadata["image"]["edit_of"], json!(mid.to_string()));
        assert_eq!(metadata["image"]["image_ref"], json!("previous"));
        assert_eq!(metadata["image"]["prompt"], json!("EDITED SUBJECT"));

        // The audit row is linked and records the instruction.
        let compose_event_id =
            Uuid::parse_str(metadata["image"]["compose_event_id"].as_str().unwrap()).unwrap();
        let (source, status_col, instruction): (String, String, String) = sqlx::query_as(
            "SELECT source, status, inputs->>'instruction' FROM engine.chat_images_events WHERE id = $1",
        )
        .bind(compose_event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(source, "image_edit");
        assert_eq!(status_col, "ok");
        assert_eq!(instruction, "换套衣服");

        // `task` is config routing only and never reaches the wire, so prove
        // the edit task drove this call by what DID: its model chain and its
        // built-in edit prompt.
        let reqs = mock.received_requests().await.unwrap();
        let sent = String::from_utf8_lossy(&reqs[0].body);
        assert!(
            sent.contains("editor"),
            "the edit block's model must be used: {sent}"
        );
        assert!(
            sent.contains("You revise a picture"),
            "the built-in EDIT prompt must be the system message: {sent}"
        );
        assert!(
            sent.contains("[修改要求]"),
            "the edit payload must be sent: {sent}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_falls_back_to_the_compose_chain_and_the_built_in_edit_prompt(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let (_, mid) = seed_image_turn(&pool, session_id).await;

        let mock = MockServer::start().await;
        mount_editor(&mock).await;
        // Only [tasks.chat_image_prompt_compose] is configured — no
        // [tasks.chat_image_edit_compose] block at all.
        let state = with_composer(test_state(pool.clone()), &mock.uri(), COMPOSE_ONLY_TOML);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, body) = send_request(
            &mut app,
            edit_req(session_id, mid, &token, json!({"instruction": "换套衣服"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={body}");

        // The chat composer's chain drove the call (its model is "composer"),
        // but the built-in EDIT prompt was used — never the chat composer's
        // own filter_prompt, which is written to compose from conversation
        // context, not to revise a picture.
        let reqs = mock.received_requests().await.unwrap();
        let sent = String::from_utf8_lossy(&reqs[0].body);
        assert!(
            sent.contains("composer"),
            "the compose block's model must drive the fallback chain: {sent}"
        );
        assert!(
            sent.contains("You revise a picture"),
            "the built-in EDIT prompt must be used, not the chat composer's own prompt: {sent}"
        );
        assert!(
            !sent.contains("CHAT COMPOSER PROMPT MUST NOT BE USED FOR EDITS"),
            "the chat composer's filter_prompt must never reach the edit call: {sent}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_row_is_discoverable_and_recoverable(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let (_, mid) = seed_image_turn(&pool, session_id).await;

        let mock = MockServer::start().await;
        mount_editor(&mock).await;
        let state = with_composer(test_state(pool.clone()), &mock.uri(), EDIT_TASK_TOML);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (_, body) = send_request(
            &mut app,
            edit_req(session_id, mid, &token, json!({"instruction": "换套衣服"})),
        )
        .await;
        let new_id = body["message_id"].as_str().unwrap().to_string();
        let composed = body["composed_prompt"].as_str().unwrap().to_string();

        // History flags it like any other image turn.
        let (status, hist) = send_request(
            &mut app,
            Request::builder()
                .method("GET")
                .uri(format!("/comp/chat/{session_id}/history"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let entry = hist
            .as_array()
            .or_else(|| hist["messages"].as_array())
            .expect("history array")
            .iter()
            .find(|e| e["id"] == json!(new_id))
            .expect("the edit row must appear in history");
        assert_eq!(entry["image"], json!(true));

        // And the v1 recovery endpoint returns the same prompt for it.
        let (status, rec) = send_request(
            &mut app,
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/comp/chat/{session_id}/messages/{new_id}/image-request"
                ))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={rec}");
        assert_eq!(rec["composed_prompt"], json!(composed));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn persist_instruction_lands_a_user_row_and_anchors_the_image_to_it(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let (_, mid) = seed_image_turn(&pool, session_id).await;

        let mock = MockServer::start().await;
        mount_editor(&mock).await;
        let state = with_composer(test_state(pool.clone()), &mock.uri(), EDIT_TASK_TOML);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (status, body) = send_request(
            &mut app,
            edit_req(
                session_id,
                mid,
                &token,
                json!({"instruction": "  换套衣服  ", "persist_instruction": true}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={body}");

        let instruction_id =
            Uuid::parse_str(body["instruction_message_id"].as_str().unwrap()).unwrap();
        let new_id = Uuid::parse_str(body["message_id"].as_str().unwrap()).unwrap();

        // The instruction row is an ordinary user message quoting the source
        // turn — the same metadata key the chat stream path writes.
        let (role, content, quote): (String, String, Option<String>) = sqlx::query_as(
            "SELECT role, content, metadata->>'reply_to_message_id' \
             FROM engine.chat_messages WHERE id = $1",
        )
        .bind(instruction_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(role, "user");
        assert_eq!(content, "换套衣服", "stored trimmed");
        assert_eq!(quote, Some(mid.to_string()));

        // The image row hangs off the instruction, not the source's turn.
        let row_umid: Option<Uuid> =
            sqlx::query_scalar("SELECT user_message_id FROM engine.chat_messages WHERE id = $1")
                .bind(new_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row_umid, Some(instruction_id));

        // History replays the whole exchange: instruction (quote handed back)
        // directly followed by the new picture.
        let (status, hist) = send_request(
            &mut app,
            Request::builder()
                .method("GET")
                .uri(format!("/comp/chat/{session_id}/history"))
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let entries = hist
            .as_array()
            .or_else(|| hist["messages"].as_array())
            .expect("history array");
        let i_user = entries
            .iter()
            .position(|e| e["id"] == json!(instruction_id.to_string()))
            .expect("instruction row in history");
        let i_image = entries
            .iter()
            .position(|e| e["id"] == json!(new_id.to_string()))
            .expect("image row in history");
        assert_eq!(
            i_image,
            i_user + 1,
            "instruction directly precedes the picture"
        );
        assert_eq!(
            entries[i_user]["reply_to_message_id"],
            json!(mid.to_string())
        );
        assert_eq!(entries[i_image]["image"], json!(true));
        assert_eq!(
            entries[i_image]["user_message_id"],
            json!(instruction_id.to_string())
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_edit_can_itself_be_edited(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let (_, mid) = seed_image_turn(&pool, session_id).await;

        let mock = MockServer::start().await;
        mount_editor(&mock).await;
        let state = with_composer(test_state(pool.clone()), &mock.uri(), EDIT_TASK_TOML);
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);

        let (_, first) = send_request(
            &mut app,
            edit_req(session_id, mid, &token, json!({"instruction": "换套衣服"})),
        )
        .await;
        let first_id = Uuid::parse_str(first["message_id"].as_str().unwrap()).unwrap();

        let (status, second) = send_request(
            &mut app,
            edit_req(
                session_id,
                first_id,
                &token,
                json!({"instruction": "换个角度"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={second}");
        assert_eq!(second["edit_of"], json!(first_id.to_string()));

        // With the switch on, an edit of an edit anchors to its own new
        // instruction row — not to the chain it descends from.
        let (status, third) = send_request(
            &mut app,
            edit_req(
                session_id,
                first_id,
                &token,
                json!({"instruction": "背景换成海边", "persist_instruction": true}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "body={third}");
        let instruction_id =
            Uuid::parse_str(third["instruction_message_id"].as_str().unwrap()).unwrap();
        let third_id = Uuid::parse_str(third["message_id"].as_str().unwrap()).unwrap();
        let row_umid: Option<Uuid> =
            sqlx::query_scalar("SELECT user_message_id FROM engine.chat_messages WHERE id = $1")
                .bind(third_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row_umid, Some(instruction_id));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn image_edit_429_when_the_per_user_cap_is_reached(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        let (_, mid) = seed_image_turn(&pool, session_id).await;

        let mock = MockServer::start().await;
        mount_editor(&mock).await;
        let state = with_composer(test_state(pool.clone()), &mock.uri(), EDIT_TASK_TOML);
        // Hold every slot, exactly as the compose endpoint's 429 test does.
        let _g: Vec<_> = (0..CONCURRENT_STREAMS_PER_USER)
            .map(|_| {
                state
                    .stream_slots
                    .try_acquire(user_id, CONCURRENT_STREAMS_PER_USER)
                    .expect("slot")
            })
            .collect();
        let mut app = build_router(state);
        let token = mint_test_jwt(user_id);
        let (status, _) = send_request(
            &mut app,
            edit_req(session_id, mid, &token, json!({"instruction": "换套衣服"})),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Voice channel — thin per-turn generator and prompt.
//!
//! Spec: docs/superpowers/specs/2026-07-07-voice-call-parts-design.md

use std::sync::Arc;
use ulid::Ulid;
use uuid::Uuid;

use eros_engine_core::affinity::{Affinity, RelationshipLabel};
use eros_engine_core::persona::PersonaGenome;
use eros_engine_llm::model_config::ResolvedVoice;
use eros_engine_llm::openrouter::{ChatMessage as WireMessage, ChatRequest, UsageBlock};
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::persona::PersonaRepo;

use crate::pipeline::stream::{ulid_string, ProtocolFrame, StreamErrorCode};
use crate::state::AppState;

/// Assemble the thin voice system prompt: persona + voice directive + one
/// optional relationship line. Deliberately excludes recall, memories, traits,
/// scopes, and every heavy block the text path's `build_prompt` composes.
pub fn build_voice_prompt(
    genome: &PersonaGenome,
    directive: &str,
    affinity: Option<&Affinity>,
) -> String {
    let mut s = String::with_capacity(genome.system_prompt.len() + directive.len() + 96);
    s.push_str(&genome.system_prompt);
    s.push_str("\n\n");
    s.push_str(directive);
    if let Some(line) = affinity.and_then(relationship_line) {
        s.push_str("\n\n");
        s.push_str(&line);
    }
    s
}

/// One short relationship-tone line from the cached `relationship_label`.
/// `None` (fresh affinity, no label yet) ⇒ no line.
fn relationship_line(affinity: &Affinity) -> Option<String> {
    let phrase = match &affinity.relationship_label {
        Some(RelationshipLabel::Stranger) => {
            "You two are still getting to know each other; keep it light."
        }
        Some(RelationshipLabel::Friend) => "You two are close friends; be warm and familiar.",
        Some(RelationshipLabel::Romantic) => "You share a romantic bond; be affectionate.",
        Some(RelationshipLabel::Frenemy) => "Your dynamic is playful and a little combative.",
        Some(RelationshipLabel::SlowBurn) => "There's a slow-building closeness between you.",
        None => return None,
    };
    Some(phrase.to_string())
}

/// Inputs for one voice turn. The user utterance is already persisted (by the
/// route) as the latest history row, so the generator reads it from history —
/// it is not passed again here.
pub struct VoiceTurn {
    pub session_id: Uuid,
    pub instance_id: Uuid,
    pub user_message_id: Uuid,
}

/// Recent turns fed to the model on a voice turn. Shorter than the text path to
/// keep latency/tokens down.
pub const VOICE_HISTORY_WINDOW: i64 = 12;

/// Drive one voice turn: load persona + (optional) affinity + recent history,
/// assemble the thin prompt, stream a single-model completion (walking the
/// outage fallback chain ourselves, since `execute_stream` is single-model),
/// emit `delta`* then `done`, and persist the assistant turn. `error` only when
/// no candidate produced anything.
pub fn run_voice_turn(
    state: Arc<AppState>,
    turn: VoiceTurn,
    resolved: ResolvedVoice,
) -> impl futures_util::Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        let chat_repo = ChatRepo { pool: &state.pool };
        let persona_repo = PersonaRepo { pool: &state.pool };
        let affinity_repo = AffinityRepo { pool: &state.pool };

        let persona = match persona_repo.load_companion(turn.instance_id).await {
            Ok(Some(p)) => p,
            _ => {
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::Internal,
                    retryable: false,
                    message: "persona instance not found".into(),
                    user_message: "服务出现问题，请稍后再试".into(),
                };
                return;
            }
        };

        // Read-only affinity load (never creates a row on the voice path).
        let affinity = affinity_repo.load(turn.session_id).await.unwrap_or(None);

        // Chronological history, includes the just-persisted user turn.
        let history = match chat_repo
            .history(turn.session_id, VOICE_HISTORY_WINDOW, 0)
            .await
        {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "voice: history read failed");
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::Internal,
                    retryable: true,
                    message: "history read failed".into(),
                    user_message: "服务出现问题，请稍后再试".into(),
                };
                return;
            }
        };

        let system_prompt =
            build_voice_prompt(&persona.genome, &resolved.directive, affinity.as_ref());

        let mut messages = Vec::with_capacity(history.len() + 1);
        messages.push(WireMessage { role: "system".into(), content: system_prompt });
        for m in history {
            if m.content.is_empty() {
                continue; // defensive: never emit an empty-content wire message
                          // (e.g. a caption-less image turn from a mixed session) —
                          // some providers reject empty messages.
            }
            let role = match m.role.as_str() {
                "assistant" => "assistant",
                "user" | "gift_user" => "user",
                _ => continue,
            };
            messages.push(WireMessage { role: role.into(), content: m.content });
        }

        // Candidate chain: primary + outage fallbacks (single-model each).
        let mut candidates = Vec::with_capacity(1 + resolved.fallback_model.len());
        candidates.push(resolved.model.clone());
        candidates.extend(resolved.fallback_model.iter().cloned());

        let mid = Ulid::new();
        let message_id = ulid_string(mid);
        let assistant_uuid: Uuid = mid.into();

        let mut acc = String::new();
        let mut last_usage: Option<UsageBlock> = None;
        let mut last_gen_id: Option<String> = None;
        let mut served_model: Option<String> = None;
        let mut truncated = false;

        'candidates: for model_id in candidates {
            let req = ChatRequest {
                model: model_id.clone(),
                messages: messages.clone(),
                temperature: resolved.temperature as f32,
                max_tokens: resolved.max_tokens,
                reasoning: resolved.reasoning.clone(),
                ..Default::default()
            };
            let stream = match state.openrouter.execute_stream(req).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(model = %model_id, error = %e, "voice: open stream failed");
                    if acc.is_empty() { continue 'candidates; }
                    truncated = true;
                    break 'candidates;
                }
            };
            futures_util::pin_mut!(stream);
            loop {
                match futures_util::StreamExt::next(&mut stream).await {
                    Some(Ok(chunk)) => {
                        if chunk.usage.is_some() { last_usage = chunk.usage.clone(); }
                        if chunk.generation_id.is_some() { last_gen_id = chunk.generation_id.clone(); }
                        if chunk.model.is_some() { served_model = chunk.model.clone(); }
                        if let Some(text) = chunk.content {
                            acc.push_str(&text);
                            yield ProtocolFrame::Delta { message_id: message_id.clone(), content: text };
                        }
                        if chunk.finish_reason.as_deref() == Some("length") { truncated = true; }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(model = %model_id, error = %e, "voice: mid-stream error");
                        if acc.is_empty() { continue 'candidates; }
                        truncated = true;
                        break 'candidates;
                    }
                    None => {
                        // Clean stream end. If this candidate streamed nothing, treat it as an
                        // empty/failed candidate and try the next one (fallbacks apply to empty
                        // completions too); otherwise we have a reply — stop.
                        if acc.is_empty() {
                            continue 'candidates;
                        }
                        break 'candidates;
                    }
                }
            }
        }

        // Any empty accumulation — no successful open, or a stream that sent
        // only metadata (id/model) and then errored or ended without content
        // — is an upstream failure. Never emit an empty `done`.
        if acc.is_empty() {
            yield ProtocolFrame::Error {
                code: StreamErrorCode::UpstreamUnavailable,
                retryable: true,
                message: "voice generation failed on all candidates".into(),
                user_message: "对方暂时说不出话，请稍后再试".into(),
            };
            return;
        }

        // Persist the assistant turn only when it carries text.
        if !acc.is_empty() {
            let usage_json = last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
            if let Err(e) = chat_repo
                .insert_voice_assistant_message(
                    turn.session_id,
                    turn.user_message_id,
                    assistant_uuid,
                    &acc,
                    served_model.as_deref(),
                    usage_json.as_ref(),
                    last_gen_id.as_deref(),
                    truncated,
                )
                .await
            {
                tracing::warn!(error = %e, "voice: assistant persist failed");
            }
        }

        yield ProtocolFrame::Done {
            message_id,
            truncated,
            usage: last_usage.and_then(|u| serde_json::to_value(u).ok()),
            generation_id: last_gen_id,
            ghost_fallback: false,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn genome() -> PersonaGenome {
        PersonaGenome {
            id: Uuid::new_v4(),
            name: "Mia".into(),
            system_prompt: "You are Mia.".into(),
            tip_personality: None,
            art_metadata: serde_json::json!({}),
        }
    }

    fn affinity_with(label: Option<RelationshipLabel>) -> Affinity {
        let now = Utc::now();
        Affinity {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            instance_id: Uuid::new_v4(),
            warmth: 0.0,
            trust: 0.0,
            intrigue: 0.0,
            intimacy: 0.0,
            patience: 0.0,
            tension: 0.0,
            ghost_streak: 0,
            last_ghost_at: None,
            total_ghosts: 0,
            relationship_label: label,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn includes_persona_and_directive() {
        let p = build_voice_prompt(&genome(), "DIRECTIVE", None);
        assert!(p.contains("You are Mia."));
        assert!(p.contains("DIRECTIVE"));
        // No affinity ⇒ no relationship line.
        assert!(!p.contains("romantic"));
    }

    #[test]
    fn appends_relationship_line_when_labelled() {
        let a = affinity_with(Some(RelationshipLabel::Romantic));
        let p = build_voice_prompt(&genome(), "DIRECTIVE", Some(&a));
        assert!(p.contains("romantic bond"));
    }

    #[test]
    fn no_relationship_line_when_label_none() {
        let a = affinity_with(None);
        let p = build_voice_prompt(&genome(), "DIRECTIVE", Some(&a));
        assert_eq!(p, "You are Mia.\n\nDIRECTIVE");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_streams_delta_then_done_and_persists(pool: sqlx::PgPool) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3},\"id\":\"gen-v\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance + session.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Persist the user turn as the route would.
        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", "01J9000000000000000000VOICE")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // State with a chat_voice task + mock OpenRouter.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let resolved = state.model_config.resolve_voice().unwrap();
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_message_id: umid,
            },
            resolved,
        )
        .collect()
        .await;

        // delta(s) carry the text; terminal frame is Done; no Error.
        let text: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hi there");
        assert!(matches!(frames.last(), Some(ProtocolFrame::Done { .. })));
        assert!(!frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Error { .. })));

        // Assistant row persisted on the voice channel.
        let (content, channel): (String, Option<String>) = sqlx::query_as(
            "SELECT content, channel FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "hi there");
        assert_eq!(channel.as_deref(), Some("voice"));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_empty_completion_is_error(pool: sqlx::PgPool) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Metadata-only frame (no content delta) then a clean [DONE] — the
        // stream ends without ever producing text.
        let body = "\
data: {\"choices\":[{\"delta\":{}}],\"id\":\"gen-e\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance + session.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Persist the user turn as the route would.
        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", "01J9000000000000000000VOIC3")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // State with a chat_voice task + mock OpenRouter.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let resolved = state.model_config.resolve_voice().unwrap();
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_message_id: umid,
            },
            resolved,
        )
        .collect()
        .await;

        // An empty completion must yield an Error frame, never a Done.
        assert!(
            frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Error { .. })),
            "expected an Error frame, got {frames:?}"
        );
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, ProtocolFrame::Done { .. })),
            "must not emit Done on an empty completion; got {frames:?}"
        );

        // No assistant row persisted.
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            n, 0,
            "no assistant row should be persisted on empty completion"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_falls_back_to_content_on_empty_primary(pool: sqlx::PgPool) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;

        // First request (PRIMARY) — metadata-only + clean [DONE]: an empty
        // completion. Limited to one match so the SECOND request (the
        // fallback candidate) falls through to the content mock below.
        let empty_body = "\
data: {\"choices\":[{\"delta\":{}}],\"id\":\"gen-empty\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(empty_body, "text/event-stream"),
            )
            .up_to_n_times(1)
            .mount(&mock)
            .await;

        // Second request onward (the fallback candidate) — normal content.
        let content_body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3},\"id\":\"gen-backup\",\"model\":\"backup\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(content_body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance + session.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // Persist the user turn as the route would.
        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", "01J9000000000000000000VOIC4")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // State with a chat_voice task configured with a fallback model, +
        // mock OpenRouter.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nfallback = [\"backup\"]\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let resolved = state.model_config.resolve_voice().unwrap();
        assert_eq!(resolved.fallback_model, vec!["backup".to_string()]);
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_message_id: umid,
            },
            resolved,
        )
        .collect()
        .await;

        // The empty PRIMARY must not surface as an error — the fallback
        // candidate's content wins: a Delta carrying "recovered", a terminal
        // Done, and no Error frame.
        let text: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "recovered");
        assert!(matches!(frames.last(), Some(ProtocolFrame::Done { .. })));
        assert!(!frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Error { .. })));

        // Assistant row persisted with the fallback's content.
        let (content, channel): (String, Option<String>) = sqlx::query_as(
            "SELECT content, channel FROM engine.chat_messages \
             WHERE session_id = $1 AND role = 'assistant'",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "recovered");
        assert_eq!(channel.as_deref(), Some("voice"));

        // Sanity: both candidates were actually hit (one empty, one content).
        let received = mock
            .received_requests()
            .await
            .expect("recording enabled by default");
        assert_eq!(received.len(), 2, "expected primary + fallback requests");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn run_voice_turn_skips_empty_content_history_rows(pool: sqlx::PgPool) {
        use eros_engine_llm::model_config::ModelConfig;
        use futures_util::StreamExt;
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let body = "\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\" there\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3},\"id\":\"gen-v\",\"model\":\"primary\"}\n\n\
data: [DONE]\n\n";
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(body, "text/event-stream"),
            )
            .mount(&mock)
            .await;

        // Seed persona + instance + session.
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('V', 'You are V.', '{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id).bind(user_id).fetch_one(&pool).await.unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        // A stray empty-content assistant row (e.g. a caption-less image turn
        // from a mixed session) landing BEFORE the voice user turn. It must be
        // skipped in the wire-message mapping, never sent upstream.
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'assistant', '')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        // Persist the user turn as the route would.
        let repo = ChatRepo { pool: &pool };
        let umid = match repo
            .insert_voice_user_message(session_id, "hello", "01J9000000000000000000VOIC2")
            .await
            .unwrap()
        {
            eros_engine_store::chat::VoiceUserInsert::Inserted(id) => id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // State with a chat_voice task + mock OpenRouter.
        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.chat_voice]\nmodel = \"primary\"\nmax_tokens = 100\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                eros_engine_llm::openrouter::AppAttribution::default(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let resolved = state.model_config.resolve_voice().unwrap();
        let frames: Vec<ProtocolFrame> = run_voice_turn(
            Arc::new(state),
            VoiceTurn {
                session_id,
                instance_id,
                user_message_id: umid,
            },
            resolved,
        )
        .collect()
        .await;

        // Stream still completes cleanly: delta(s) carry the text, terminal
        // frame is Done, no Error — the empty history row must not break the
        // turn.
        let text: String = frames
            .iter()
            .filter_map(|f| match f {
                ProtocolFrame::Delta { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hi there");
        assert!(matches!(frames.last(), Some(ProtocolFrame::Done { .. })));
        assert!(!frames
            .iter()
            .any(|f| matches!(f, ProtocolFrame::Error { .. })));

        // The outgoing request body must NOT contain the empty-content row —
        // proof the skip guard actually dropped it from the wire mapping.
        let received = mock
            .received_requests()
            .await
            .expect("recording enabled by default");
        assert!(
            !received.is_empty(),
            "expected at least one upstream request"
        );
        for req in &received {
            let req_body = String::from_utf8_lossy(&req.body);
            assert!(
                !req_body.contains("\"content\":\"\""),
                "request body must not contain an empty-content message; body={req_body}",
            );
        }
    }
}

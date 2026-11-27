// SPDX-License-Identifier: AGPL-3.0-only
//! Speech markup for chat messages: her stored lines, prepared for a
//! text-to-speech voice.
//!
//! Spec: docs/superpowers/specs/2026-11-27-chat-speech-markup-design.md

use std::pin::Pin;
use std::sync::Arc;

use futures_util::{Stream, StreamExt};
use uuid::Uuid;

use eros_engine_core::affinity::Affinity;
use eros_engine_core::scope::AffinityScope;
use eros_engine_llm::model_config::{
    ResolvedSpeech, SPEECH_MODE_REWRITE, SPEECH_MODE_VERBATIM, SPEECH_TAGS_GUIDANCE,
};
use eros_engine_llm::openrouter::{ChatMessage as WireMessage, ChatRequest, UsageBlock};
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::persona::PersonaRepo;

use crate::pipeline::stream::{
    ProtocolFrame, StreamErrorCode, STREAM_OPEN_TIMEOUT, STREAM_TOTAL_TIMEOUT,
};
use crate::state::AppState;

const TASK: &str = "chat_speech";
/// The rows before the line that go into the transcript (spec §5).
const EARLIER_ROWS: usize = 4;

const TONE_EMOTIONAL: &str =
    "She said this in the grip of a feeling, unguarded: let it show in the delivery.";
const TONE_EROTIC_ADDS: &str = "She wants the other person: soft, slow, close, alluring.";
const TONE_FOCUSED: &str = "She said this with her mind on her work: brisk, even, brief.";

/// The one sentence that tells the model which tone the line was written in
/// (spec §5.1), from the row's `metadata.tone`. `erotic` is a superset of
/// `emotional` here as in the chat prompt. `default`, an unknown value and no
/// key say nothing.
pub(crate) fn tone_sentence(tone: Option<&str>) -> Option<String> {
    match tone? {
        "emotional" => Some(TONE_EMOTIONAL.to_string()),
        "erotic" => Some(format!("{TONE_EMOTIONAL} {TONE_EROTIC_ADDS}")),
        "focused" => Some(TONE_FOCUSED.to_string()),
        _ => None,
    }
}

/// The affinity scope the row recorded (`metadata.affinity_scope`); a row with
/// none, or with one that does not read, takes the chat stream's default
/// scope (spec §5.2).
pub(crate) fn row_scope(metadata: Option<&serde_json::Value>) -> AffinityScope {
    metadata
        .and_then(|m| m.get("affinity_scope"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// What one line is judged from (spec §5).
pub(crate) struct SpeechInput<'a> {
    /// The persona's authored system prompt.
    pub persona_head: &'a str,
    /// The base directive: the deployment's, else the built-in one.
    pub base: &'a str,
    /// Whether the line may be rephrased (spec §6.2).
    pub rewrite: bool,
    /// The row's `metadata.tone`.
    pub tone: Option<&'a str>,
    /// The relationship line, already rendered.
    pub relationship: Option<&'a str>,
    /// The rows before the line, oldest first, as `(role, content)`.
    pub earlier: &'a [(String, String)],
    /// The stored text of the message.
    pub line: &'a str,
}

/// The request's two messages. System: the persona head, the base directive,
/// the mode sentence and the tag guidance, then the tone sentence and the
/// relationship line when there are any. User: the earlier rows as a plain
/// transcript (blank rows skipped; the block left out when none remain), then
/// the line.
pub(crate) fn build_speech_messages(i: &SpeechInput) -> Vec<WireMessage> {
    let mode = if i.rewrite {
        SPEECH_MODE_REWRITE
    } else {
        SPEECH_MODE_VERBATIM
    };
    let mut system = vec![
        i.persona_head.to_string(),
        i.base.to_string(),
        mode.to_string(),
        SPEECH_TAGS_GUIDANCE.to_string(),
    ];
    system.extend(tone_sentence(i.tone));
    system.extend(i.relationship.map(str::to_string));

    let transcript: Vec<String> = i
        .earlier
        .iter()
        .filter(|(_, content)| !content.trim().is_empty())
        .map(|(role, content)| {
            let who = if role == "assistant" { "Her" } else { "User" };
            format!("{who}: {content}")
        })
        .collect();
    let mut user = String::new();
    if !transcript.is_empty() {
        user.push_str("Earlier lines (context only):\n");
        user.push_str(&transcript.join("\n"));
        user.push_str("\n\n");
    }
    user.push_str("The line to prepare:\n");
    user.push_str(i.line);

    vec![
        WireMessage {
            role: "system".into(),
            content: system.join("\n\n"),
        },
        WireMessage {
            role: "user".into(),
            content: user,
        },
    ]
}

/// One speech-markup request, its session already checked by the route.
pub(crate) struct SpeechJob {
    pub session_id: Uuid,
    /// The session whose affinity a chat turn here reads: the parent of a date
    /// sub-session, the session itself otherwise.
    pub relationship: Uuid,
    pub instance_id: Uuid,
    pub message_ids: Vec<Uuid>,
    pub rewrite: bool,
}

/// What every item of a request shares.
struct Shared {
    session_id: Uuid,
    head: String,
    rewrite: bool,
    affinity: Option<Affinity>,
    resolved: ResolvedSpeech,
}

type Frames = Pin<Box<dyn Stream<Item = ProtocolFrame> + Send>>;

/// Every item, run at once; their frames interleave and each names its
/// source message. Then one `final`. A persona that cannot be read ends the
/// whole request with an `error` frame.
pub(crate) fn run_speech(
    state: Arc<AppState>,
    job: SpeechJob,
    resolved: ResolvedSpeech,
) -> impl Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        let persona = match (PersonaRepo { pool: &state.pool }).load_companion(job.instance_id).await {
            Ok(Some(p)) => p,
            _ => {
                yield ProtocolFrame::Error {
                    code: StreamErrorCode::Internal,
                    retryable: false,
                    message: "persona instance not found".into(),
                    user_message: "服务出现问题，请稍后再试".into(),
                    upstream_status: None,
                    provider_code: None,
                };
                return;
            }
        };
        // Read-only and decayed, as the chat turn reads it; no row, no line.
        let affinity = (AffinityRepo { pool: &state.pool })
            .load(job.relationship)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "speech: affinity read failed; no relationship line");
                None
            })
            .map(|mut a| {
                a.apply_time_decay();
                a.refresh_endpoints(&state.config.affinity_tuning);
                a
            });
        let shared = Arc::new(Shared {
            session_id: job.session_id,
            head: persona.genome.system_prompt.clone(),
            rewrite: job.rewrite,
            affinity,
            resolved,
        });
        let items: Vec<Frames> = job
            .message_ids
            .into_iter()
            .map(|id| Box::pin(item(state.clone(), shared.clone(), id)) as Frames)
            .collect();
        let mut all = futures_util::stream::select_all(items);
        while let Some(frame) = all.next().await {
            yield frame;
        }
        yield ProtocolFrame::SpeechFinal;
    }
}

fn item_error(id: Uuid, code: &'static str, message: &str) -> ProtocolFrame {
    ProtocolFrame::ItemError {
        message_id: id.to_string(),
        code,
        message: message.into(),
    }
}

/// One item: the stored line, its request, then the model chain walked as the
/// voice turn walks it.
fn item(
    state: Arc<AppState>,
    shared: Arc<Shared>,
    id: Uuid,
) -> impl Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        let repo = ChatRepo { pool: &state.pool };
        let row = match repo.message_by_id_in_session(shared.session_id, id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                yield item_error(id, "not_found", "no such message in this session");
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, "speech: message read failed");
                yield item_error(id, "generation_failed", "the message could not be read");
                return;
            }
        };
        if row.role != "assistant" || row.content.trim().is_empty() {
            yield item_error(id, "not_speakable", "only her messages with text can be spoken");
            return;
        }
        let earlier: Vec<(String, String)> = match repo
            .history_up_to(shared.session_id, id, (EARLIER_ROWS + 1) as i64)
            .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter(|m| m.id != id && (m.role == "user" || m.role == "assistant"))
                .map(|m| (m.role, m.content))
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "speech: earlier rows read failed; no transcript");
                Vec::new()
            }
        };
        let earlier = &earlier[earlier.len().saturating_sub(EARLIER_ROWS)..];
        let metadata = row.metadata.as_ref();
        let tone = metadata.and_then(|m| m.get("tone")).and_then(|v| v.as_str());
        let relationship = shared
            .affinity
            .as_ref()
            .and_then(|a| crate::pipeline::voice::relationship_line(a, row_scope(metadata)));
        let resolved = &shared.resolved;
        let req = ChatRequest {
            model: String::new(),
            messages: build_speech_messages(&SpeechInput {
                persona_head: &shared.head,
                base: &resolved.base,
                rewrite: shared.rewrite,
                tone,
                relationship: relationship.as_deref(),
                earlier,
                line: &row.content,
            }),
            temperature: resolved.temperature as f32,
            max_tokens: resolved.max_tokens,
            sampling: resolved.sampling,
            task: Some(TASK.into()),
            ..Default::default()
        };

        let message_id = id.to_string();
        let mut candidates = Vec::with_capacity(1 + resolved.fallback_model.len());
        candidates.push(resolved.model.clone());
        candidates.extend(resolved.fallback_model.iter().cloned());
        let mut acc = String::new();
        let mut last_usage: Option<UsageBlock> = None;
        let mut last_gen_id: Option<String> = None;
        let mut served_model: Option<String> = None;
        let mut truncated = false;

        'candidates: for model_id in candidates {
            // An abandoned hop's generation is still spend: record it.
            if let Some(gid) = last_gen_id.take() {
                let usage = last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
                let _ = crate::pipeline::record_generation(
                    &state.pool,
                    crate::pipeline::GenerationRecord {
                        task: TASK,
                        session_id: Some(shared.session_id),
                        generation_id: Some(&gid),
                        model: served_model.as_deref(),
                        usage: usage.as_ref(),
                    },
                )
                .await;
            }
            last_usage = None;
            served_model = None;
            truncated = false;
            let stream = match tokio::time::timeout(
                STREAM_OPEN_TIMEOUT,
                state.openrouter.execute_stream_as(&req, &model_id),
            )
            .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::warn!(model = %model_id, error = %e, "speech: open stream failed");
                    if acc.is_empty() { continue 'candidates; }
                    truncated = true;
                    break 'candidates;
                }
                Err(_) => {
                    tracing::warn!(model = %model_id, "speech: open timeout");
                    if acc.is_empty() { continue 'candidates; }
                    truncated = true;
                    break 'candidates;
                }
            };
            futures_util::pin_mut!(stream);
            let deadline = tokio::time::Instant::now() + STREAM_TOTAL_TIMEOUT;
            loop {
                let next = match tokio::time::timeout_at(deadline, stream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        tracing::warn!(model = %model_id, "speech: total timeout");
                        if acc.is_empty() { continue 'candidates; }
                        truncated = true;
                        break 'candidates;
                    }
                };
                match next {
                    Some(Ok(chunk)) => {
                        if chunk.usage.is_some() { last_usage = chunk.usage.clone(); }
                        if chunk.generation_id.is_some() { last_gen_id = chunk.generation_id.clone(); }
                        if chunk.model.is_some() { served_model = chunk.model.clone(); }
                        if let Some(text) = chunk.content {
                            acc.push_str(&text);
                            yield ProtocolFrame::Delta { message_id: message_id.clone(), content: text };
                        }
                        if matches!(chunk.finish_reason.as_deref(), Some("length") | Some("content_filter")) {
                            truncated = true;
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(model = %model_id, error = %e, "speech: mid-stream error");
                        if acc.is_empty() { continue 'candidates; }
                        truncated = true;
                        break 'candidates;
                    }
                    None => {
                        if acc.is_empty() { continue 'candidates; }
                        break 'candidates;
                    }
                }
            }
        }

        let usage = last_usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
        if last_gen_id.is_some() {
            last_gen_id = crate::pipeline::record_generation(
                &state.pool,
                crate::pipeline::GenerationRecord {
                    task: TASK,
                    session_id: Some(shared.session_id),
                    generation_id: last_gen_id.as_deref(),
                    model: served_model.as_deref(),
                    usage: usage.as_ref(),
                },
            )
            .await;
        }
        let text = acc.trim();
        if text.is_empty() {
            yield item_error(id, "generation_failed", "no model produced the line");
            return;
        }
        let mut usage_wire = usage;
        crate::routes::companion::filter_usage_keys(
            &mut usage_wire,
            &state.config.openrouter_usage_hidden_keys,
        );
        yield ProtocolFrame::SpeechDone {
            message_id,
            text: text.to_string(),
            truncated,
            usage: usage_wire,
            generation_id: last_gen_id,
            model: served_model,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eros_engine_llm::model_config::{
        DEFAULT_SPEECH_BASE, SPEECH_MODE_REWRITE, SPEECH_MODE_VERBATIM, SPEECH_TAGS_GUIDANCE,
    };

    const EMOTIONAL: &str =
        "She said this in the grip of a feeling, unguarded: let it show in the delivery.";
    const WANTS: &str = "She wants the other person: soft, slow, close, alluring.";
    const FOCUSED: &str = "She said this with her mind on her work: brisk, even, brief.";

    #[test]
    fn the_tone_sentences() {
        assert_eq!(tone_sentence(Some("emotional")).as_deref(), Some(EMOTIONAL));
        assert_eq!(
            tone_sentence(Some("erotic")),
            Some(format!("{EMOTIONAL} {WANTS}"))
        );
        assert_eq!(tone_sentence(Some("focused")).as_deref(), Some(FOCUSED));
        for none in [None, Some("default"), Some("cheerful"), Some("")] {
            assert_eq!(tone_sentence(none), None, "{none:?}");
        }
    }

    #[test]
    fn the_row_scope_defaults() {
        let full = AffinityScope::full();
        assert_eq!(row_scope(None), AffinityScope::default());
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"affinity_scope": 7}),
            serde_json::json!({"affinity_scope": null}),
        ] {
            assert_eq!(row_scope(Some(&bad)), AffinityScope::default(), "{bad}");
        }
        let stored = serde_json::json!({"affinity_scope": serde_json::to_value(full).unwrap()});
        assert_eq!(row_scope(Some(&stored)), full);
    }

    fn input<'a>(
        rewrite: bool,
        tone: Option<&'a str>,
        relationship: Option<&'a str>,
        earlier: &'a [(String, String)],
    ) -> SpeechInput<'a> {
        SpeechInput {
            persona_head: "PERSONA-HEAD",
            base: DEFAULT_SPEECH_BASE,
            rewrite,
            tone,
            relationship,
            earlier,
            line: "你怎么才来😊",
        }
    }

    #[test]
    fn the_request_is_a_system_and_a_user_message() {
        let m = build_speech_messages(&input(false, None, None, &[]));
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].role.as_str(), m[1].role.as_str()), ("system", "user"));
        assert_eq!(
            m[0].content,
            format!(
                "PERSONA-HEAD\n\n{DEFAULT_SPEECH_BASE}\n\n{SPEECH_MODE_VERBATIM}\n\n{SPEECH_TAGS_GUIDANCE}"
            )
        );

        // `rewrite` swaps the mode sentence and nothing else.
        let r = build_speech_messages(&input(true, None, None, &[]));
        assert_eq!(
            r[0].content,
            m[0].content
                .replace(SPEECH_MODE_VERBATIM, SPEECH_MODE_REWRITE)
        );
        assert_eq!(r[1].content, m[1].content);

        // The tone sentence, then the relationship line, after the guidance.
        let both = build_speech_messages(&input(false, Some("erotic"), Some("REL-LINE"), &[]));
        assert_eq!(
            both[0].content,
            format!("{}\n\n{EMOTIONAL} {WANTS}\n\nREL-LINE", m[0].content)
        );
        let tone_only = build_speech_messages(&input(false, Some("focused"), None, &[]));
        assert_eq!(
            tone_only[0].content,
            format!("{}\n\n{FOCUSED}", m[0].content)
        );
        let rel_only = build_speech_messages(&input(false, Some("default"), Some("REL-LINE"), &[]));
        assert_eq!(rel_only[0].content, format!("{}\n\nREL-LINE", m[0].content));
    }

    #[test]
    fn the_transcript_skips_blank_rows_and_may_be_empty() {
        let none = build_speech_messages(&input(false, None, None, &[]));
        assert_eq!(none[1].content, "The line to prepare:\n你怎么才来😊");

        let earlier = [
            ("user".to_string(), "在吗".to_string()),
            ("user".to_string(), "  ".to_string()),
            ("assistant".to_string(), "在呀".to_string()),
        ];
        let some = build_speech_messages(&input(false, None, None, &earlier));
        assert_eq!(
            some[1].content,
            "Earlier lines (context only):\nUser: 在吗\nHer: 在呀\n\nThe line to prepare:\n你怎么才来😊"
        );

        // Only blank rows: as none at all.
        let blank = [("user".to_string(), String::new())];
        assert_eq!(
            build_speech_messages(&input(false, None, None, &blank))[1].content,
            none[1].content
        );
    }
}

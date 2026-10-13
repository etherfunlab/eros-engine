// SPDX-License-Identifier: AGPL-3.0-only

//! Group chat turns (spec 2026-10-12-group-chat-design.md §5): how one
//! speaker sees the group, and the turn that casts and relays the speakers.

use std::collections::HashMap;
use std::sync::Arc;

use eros_engine_core::affinity::Affinity;
use eros_engine_core::group::{
    fallback_cues, handle, normalize, render_line, Cue, LineSpeaker, UNKNOWN_SPEAKER,
};
use eros_engine_core::persona::CompanionPersona;
use eros_engine_core::types::{
    ActionType, ConversationSignals, DecisionInput, Event, ImageRef, LlmAudit, PromptTrait,
};
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::{ChatMessage, ChatRepo};
use eros_engine_store::group::{GroupMember, GroupRepo};
use eros_engine_store::persona::PersonaRepo;
use rand::Rng as _;
use ulid::Ulid;
use uuid::Uuid;

use crate::holiday::UserLocale;
use crate::pipeline::chat_queue::{classify_outcome, TurnOutcome};
use crate::pipeline::group_director::{
    director_context, record_director_event, run_director, DirectorSeat,
};
use crate::pipeline::post_process::{GroupSpoken, ProducedMessage};
use crate::pipeline::reasoning_call::JudgeStep;
use crate::pipeline::stream::{
    build_delegated_image_marker, build_delegated_image_prompt, build_final_frame,
    build_image_request_frame, delegated_image_only_frames, drive_chat_burst_for, frame_action_for,
    image_capability_available, sanitize_inner_state, split_failures_typed, ulid_string,
    AbortOnDrop, BurstOutcome, CastEntry, PdeStatus, ProtocolFrame, StreamErrorCode,
};
use crate::repetition::Injected;
use crate::routes::companion_stream::ImageReplyParams;
use crate::state::AppState;

/// One speaker's view of the group, handed to `build_reply_request_with`.
pub(crate) struct GroupSpeakerView {
    pub group_session_id: Uuid,
    pub speaker_session_id: Uuid,
    /// Persona names by member session, for every speaker in the window.
    pub names: HashMap<Uuid, String>,
    /// The `[群聊]` block appended to the system prompt.
    pub block: String,
}

impl GroupSpeakerView {
    fn name_of(&self, s: Uuid) -> &str {
        self.names
            .get(&s)
            .map(String::as_str)
            .unwrap_or(UNKNOWN_SPEAKER)
    }
}

/// Relabel the model-facing history for one speaker. `raw` is the history
/// without the noise strip and `stripped` with it; own lines take the
/// stripped text (dropped if the strip dropped them), everyone else's keep
/// the raw text.
pub(crate) fn speaker_view(
    raw: Vec<Injected>,
    stripped: Vec<Injected>,
    speakers: &HashMap<Uuid, Option<Uuid>>,
    view: &GroupSpeakerView,
) -> Vec<Injected> {
    let own: HashMap<Uuid, String> = stripped.into_iter().map(|m| (m.id, m.text)).collect();
    raw.into_iter()
        .filter_map(|m| {
            let speaker = speakers.get(&m.id).copied().flatten();
            let line = match (m.role.as_str(), speaker) {
                ("user", _) => LineSpeaker::User,
                ("assistant", Some(s)) if s == view.speaker_session_id => LineSpeaker::Me,
                ("assistant", Some(s)) => LineSpeaker::Other(view.name_of(s)),
                ("assistant", None) => LineSpeaker::Other(UNKNOWN_SPEAKER),
                _ => return None,
            };
            let text = match line {
                LineSpeaker::Me => own.get(&m.id)?.clone(),
                _ => m.text,
            };
            if text.trim().is_empty() {
                return None;
            }
            let (role, text) = render_line(line, &text);
            Some(Injected {
                id: m.id,
                role: role.to_string(),
                text,
            })
        })
        .collect()
}

/// The speaker's `[群聊]` block. `others` are the other seated members'
/// names and one-line personalities.
pub(crate) fn group_block(title: Option<&str>, others: &[(String, Option<String>)]) -> String {
    let mut s = String::from("\n\n[群聊]\n");
    match title {
        Some(t) => s.push_str(&format!("你现在在群聊「{t}」里。\n")),
        None => s.push_str("你现在在一个群聊里。\n"),
    }
    if others.is_empty() {
        s.push_str("群里只有你和对方。\n");
    } else {
        s.push_str("在场的还有：\n");
        for (name, personality) in others {
            match personality
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
            {
                Some(p) => s.push_str(&format!("- {name}：{p}\n")),
                None => s.push_str(&format!("- {name}\n")),
            }
        }
    }
    s.push_str("只说你自己的话，不替别人说话，不在开头写名字。");
    s
}

/// The director's and the composer's plain-text transcript. An empty assistant
/// row is a photo when it carries the `image` marker; any other empty row (a
/// failed attempt's truncated bubble, an empty-reply ghost) said nothing.
pub(crate) fn transcript_text(rows: &[ChatMessage], names: &HashMap<Uuid, String>) -> String {
    rows.iter()
        .filter(|m| m.channel.is_none())
        .filter_map(|m| match m.role.as_str() {
            "user" => Some(format!("用户：{}", m.content)),
            "assistant" => {
                let name = m
                    .speaker_session_id
                    .and_then(|s| names.get(&s))
                    .map(String::as_str)
                    .unwrap_or(UNKNOWN_SPEAKER);
                let text = if !m.content.trim().is_empty() {
                    m.content.as_str()
                } else if m
                    .metadata
                    .as_ref()
                    .is_some_and(|md| md.get("image").is_some())
                {
                    "（发了一张照片）"
                } else {
                    return None;
                };
                Some(format!("{name}：{text}"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) struct GroupTurn {
    pub group_session_id: Uuid,
    pub user_id: Uuid,
    pub user_message_id: Uuid,
    pub content: String,
    pub title: Option<String>,
    pub members: Vec<GroupMember>,
    pub mentions: Vec<Uuid>,
    pub prompt_traits: Vec<PromptTrait>,
    pub audit: Option<LlmAudit>,
    pub tier: Option<String>,
    pub user_locale: UserLocale,
    pub image: Option<ImageReplyParams>,
}

struct Seat {
    member_session_id: Uuid,
    instance_id: Uuid,
    persona: CompanionPersona,
    affinity: Affinity,
    signals: ConversationSignals,
}

impl GroupTurn {
    fn event(&self) -> Event {
        Event::UserMessage {
            content: self.content.clone(),
            message_id: self.user_message_id,
            prompt_traits: self.prompt_traits.clone(),
            audit: self.audit.clone(),
            tier: self.tier.clone(),
            memory_scope: Default::default(),
            affinity_scope: Default::default(),
            quote: None,
            action: None,
            outfit: None,
        }
    }
}

/// Drive one group turn (spec §5). Never writes `companion_affinity`.
pub(crate) fn run_group_turn(
    state: Arc<AppState>,
    turn: GroupTurn,
) -> impl futures_util::Stream<Item = ProtocolFrame> + Send + 'static {
    async_stream::stream! {
        use futures_util::StreamExt as _;
        let chat_repo = ChatRepo { pool: &state.pool };

        // ── seats: persona + read-only, decayed affinity per member ──
        let mut seats: Vec<Seat> = Vec::new();
        for m in &turn.members {
            let persona = match (PersonaRepo { pool: &state.pool })
                .load_companion(m.instance_id)
                .await
            {
                Ok(Some(p)) => p,
                // Inactive or missing persona: not seated this turn.
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        group_session_id = %turn.group_session_id,
                        member_session_id = %m.member_session_id,
                        "group: persona load failed, member not seated: {e}"
                    );
                    continue;
                }
            };
            let mut affinity = match (AffinityRepo { pool: &state.pool })
                .load(m.member_session_id)
                .await
            {
                Ok(Some(a)) => a,
                Ok(None) => Affinity::unrecorded(),
                Err(e) => {
                    tracing::warn!("group: affinity read failed: {e}");
                    Affinity::unrecorded()
                }
            };
            affinity.apply_time_decay();
            affinity.refresh_endpoints(&state.config.affinity_tuning);
            let signals = match super::compute_signals_for_session(
                &state.pool,
                m.member_session_id,
                &affinity,
            )
            .await
            {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        member_session_id = %m.member_session_id,
                        "group: signals read failed, using defaults: {e}"
                    );
                    ConversationSignals::default()
                }
            };
            seats.push(Seat {
                member_session_id: m.member_session_id,
                instance_id: m.instance_id,
                persona,
                affinity,
                signals,
            });
        }
        if seats.is_empty() {
            yield ProtocolFrame::Error {
                code: StreamErrorCode::Internal,
                retryable: false,
                message: "no seatable group member".into(),
                user_message: "群里暂时没有可以回复的角色".into(),
                upstream_status: None,
                provider_code: None,
            };
            return;
        }

        // ── names and the transcript the director and composer read ──
        let window = match chat_repo
            .history_up_to(
                turn.group_session_id,
                turn.user_message_id,
                crate::pipeline::handlers::HISTORY_WINDOW,
            )
            .await
        {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(
                    group_session_id = %turn.group_session_id,
                    "group: history read failed, director and composer see no transcript: {e}"
                );
                Vec::new()
            }
        };
        let mut names: HashMap<Uuid, String> = seats
            .iter()
            .map(|s| (s.member_session_id, s.persona.genome.name.clone()))
            .collect();
        let unknown: Vec<Uuid> = window
            .iter()
            .filter_map(|m| m.speaker_session_id)
            .filter(|s| !names.contains_key(s))
            .collect();
        if !unknown.is_empty() {
            match (GroupRepo { pool: &state.pool })
                .speaker_names(&unknown)
                .await
            {
                Ok(more) => names.extend(more),
                Err(e) => tracing::warn!(
                    group_session_id = %turn.group_session_id,
                    "group: speaker name read failed, departed speakers read as {UNKNOWN_SPEAKER}: {e}"
                ),
            }
        }
        let before_now: Vec<ChatMessage> = window
            .iter()
            .filter(|m| m.id != turn.user_message_id)
            .cloned()
            .collect();
        let transcript = transcript_text(&before_now, &names);
        let image_available = image_capability_available(turn.image.is_some(), &state.model_config);

        // ── director (or the rule fallback) ──
        let mut turn_failures: Vec<eros_engine_llm::failure::AttemptFailure> = Vec::new();
        let cues: Vec<Cue> = match state.model_config.resolve_group_director() {
            None => fallback_cues(seats.len()),
            Some(p) => {
                let dseats: Vec<DirectorSeat<'_>> = seats
                    .iter()
                    .map(|s| DirectorSeat {
                        name: &s.persona.genome.name,
                        personality: s.persona.genome.tip_personality.as_deref(),
                        bond: s.affinity.bond_label(),
                        chemistry: s.affinity.chemistry_label(),
                        rung: s.affinity.intimacy_rung(),
                    })
                    .collect();
                let mention_names: Vec<String> = turn
                    .mentions
                    .iter()
                    .filter_map(|m| names.get(m).cloned())
                    .collect();
                let ctx = director_context(
                    turn.title.as_deref(),
                    &dseats,
                    image_available,
                    &transcript,
                    &mention_names,
                    &turn.content,
                );
                let inputs = serde_json::json!({
                    "members": seats.iter().enumerate().map(|(i, s)| serde_json::json!({
                        "handle": handle(i),
                        "member_session_id": s.member_session_id,
                        "bond": s.affinity.bond_label().as_key(),
                        "chemistry": s.affinity.chemistry_label().as_key(),
                        "rung": s.affinity.intimacy_rung(),
                    })).collect::<Vec<_>>(),
                    "image_available": image_available,
                    "mentions": turn.mentions,
                });
                let mut run = None;
                let steps = run_director(state.clone(), turn.group_session_id, p, ctx, seats.len());
                futures_util::pin_mut!(steps);
                while let Some(step) = steps.next().await {
                    match step {
                        JudgeStep::Reasoning(content) => {
                            yield ProtocolFrame::Reasoning { content };
                        }
                        JudgeStep::Finished(r) => run = Some(r),
                    }
                }
                let run = run.expect("run_director always finishes");
                let cues = run
                    .verdict
                    .as_ref()
                    .map(|v| normalize(v, seats.len(), image_available))
                    .unwrap_or_default();
                let status = if run.status == PdeStatus::Ok && cues.is_empty() {
                    "empty"
                } else {
                    run.status.as_str()
                };
                turn_failures.extend(run.failures.iter().cloned());
                record_director_event(
                    &state.pool,
                    turn.user_id,
                    turn.group_session_id,
                    turn.user_message_id,
                    status,
                    &run,
                    inputs,
                )
                .await;
                if cues.is_empty() {
                    fallback_cues(seats.len())
                } else {
                    cues
                }
            }
        };

        yield ProtocolFrame::Cast {
            speakers: cues
                .iter()
                .map(|c| CastEntry {
                    member_session_id: seats[c.seat].member_session_id,
                    action_type: frame_action_for(c.action),
                })
                .collect(),
        };

        // ── relay ──
        let now_ctx = crate::prompt::NowContext::for_user(&turn.user_locale, chrono::Utc::now());
        let latest = crate::prompt::composer_user_text(None, None, &turn.content);
        let mut spoken: Vec<GroupSpoken> = Vec::new();
        let (mut any_filtered, mut retries_chat, mut retries_filter) = (false, 0u32, 0u32);
        let mut prompt_injected: Option<Vec<String>> = None;
        for cue in &cues {
            let seat = &seats[cue.seat];
            let input = DecisionInput {
                event: turn.event(),
                affinity: seat.affinity.clone(),
                persona: seat.persona.clone(),
                signals: seat.signals.clone(),
            };
            let hints: Vec<String> = cue
                .inner_state
                .as_deref()
                .map(sanitize_inner_state)
                .filter(|s| !s.is_empty())
                .into_iter()
                .collect();
            let clothing = cue
                .clothing
                .as_deref()
                .map(sanitize_inner_state)
                .filter(|s| !s.is_empty());
            let mut plan = eros_engine_core::pde::plan_for(
                &input,
                cue.action,
                hints,
                cue.reply_mode,
                clothing,
                ImageRef::Face,
                None,
                None,
            );
            if plan.reply_mode.is_none() && plan.action_type.bears_text() {
                plan.nudges = crate::prompt::roll_nudges(
                    Some(&seat.affinity),
                    Default::default(),
                    &mut rand::thread_rng(),
                );
            }

            // Image only: compose, persist an image row, emit meta/done/image_request.
            if plan.action_type == ActionType::ReplyImage {
                let img = build_delegated_image_prompt(
                    &state,
                    &seat.persona,
                    &plan,
                    turn.image.as_ref(),
                    &transcript,
                    &latest,
                    turn.user_id,
                    turn.group_session_id,
                    None,
                )
                .await;
                turn_failures.extend(img.failures.iter().cloned());
                let msg_ulid = Ulid::new();
                let msg_uuid: Uuid = msg_ulid.into();
                let marker = build_delegated_image_marker(
                    &img.subject,
                    img.caption.as_deref(),
                    img.aspect_ratio.as_deref(),
                    img.compose_variant.as_deref(),
                    img.compose_model.as_deref(),
                    img.compose_generation_id.as_deref(),
                    img.compose_event_id,
                    plan.image_ref,
                    None,
                );
                let row = eros_engine_store::chat::AssistantInsert {
                    id: msg_uuid,
                    content: String::new(),
                    assistant_action_type: "reply".into(),
                    continues_from_message_id: None,
                    truncated: false,
                    generation_id: None,
                    filter_audit: None,
                    metadata: Some(serde_json::json!({ "image": marker })),
                    llm_attempts: None,
                    gateway_errors: None,
                    speaker_session_id: Some(seat.member_session_id),
                };
                if let Err(e) = chat_repo
                    .insert_assistant_batch(
                        turn.group_session_id,
                        turn.user_message_id,
                        std::slice::from_ref(&row),
                    )
                    .await
                {
                    tracing::warn!("group(image): persist failed: {e}");
                    yield ProtocolFrame::SpeakerError {
                        speaker_session_id: seat.member_session_id,
                        code: StreamErrorCode::Internal,
                        message: "image persist failed".into(),
                    };
                    continue;
                }
                for mut frame in delegated_image_only_frames(
                    ulid_string(msg_ulid),
                    &img.composed_prompt,
                    plan.image_ref,
                    img.aspect_ratio.as_deref(),
                ) {
                    if let ProtocolFrame::Meta {
                        speaker_session_id, ..
                    } = &mut frame
                    {
                        *speaker_session_id = Some(seat.member_session_id);
                    }
                    yield frame;
                }
                spoken.push(GroupSpoken {
                    speaker_session_id: seat.member_session_id,
                    instance_id: seat.instance_id,
                    name: seat.persona.genome.name.clone(),
                    produced: vec![ProducedMessage {
                        message_id: msg_uuid,
                        full_text: String::new(),
                        action: ActionType::ReplyImage,
                    }],
                });
                continue;
            }

            // Text, or text + image: the composer starts before the chat call.
            let mut compose = if plan.action_type == ActionType::ReplyTextImage {
                let (st, persona, plan_c, image_c, scene, latest_c) = (
                    (*state).clone(),
                    seat.persona.clone(),
                    plan.clone(),
                    turn.image.clone(),
                    transcript.clone(),
                    latest.clone(),
                );
                let (uid, gid) = (turn.user_id, turn.group_session_id);
                AbortOnDrop(Some(tokio::spawn(async move {
                    build_delegated_image_prompt(
                        &st,
                        &persona,
                        &plan_c,
                        image_c.as_ref(),
                        &scene,
                        &latest_c,
                        uid,
                        gid,
                        None,
                    )
                    .await
                })))
            } else {
                AbortOnDrop(None)
            };

            let others: Vec<(String, Option<String>)> = seats
                .iter()
                .filter(|s| s.member_session_id != seat.member_session_id)
                .map(|s| {
                    (
                        s.persona.genome.name.clone(),
                        s.persona.genome.tip_personality.clone(),
                    )
                })
                .collect();
            let view = GroupSpeakerView {
                group_session_id: turn.group_session_id,
                speaker_session_id: seat.member_session_id,
                names: names.clone(),
                block: group_block(turn.title.as_deref(), &others),
            };
            let (req, injected_tags) = match crate::pipeline::handlers::build_reply_request_with(
                &state,
                &input,
                &plan,
                seat.member_session_id,
                turn.user_id,
                seat.instance_id,
                turn.user_message_id,
                &now_ctx,
                false,
                None,
                None,
                Some(&view),
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    yield ProtocolFrame::SpeakerError {
                        speaker_session_id: seat.member_session_id,
                        code: StreamErrorCode::Internal,
                        message: format!("build_reply_request failed: {e}"),
                    };
                    continue;
                }
            };
            if !injected_tags.is_empty() {
                prompt_injected = Some(injected_tags.clone());
            }
            let chain = crate::byok::reply_chain(&req, None);
            if let Some(dir) = state.config.prompt_log_dir.as_ref() {
                let labels: Vec<String> = chain.iter().map(|h| h.audit.clone()).collect();
                crate::prompt_log::spawn_write(
                    dir.clone(),
                    &req,
                    &labels,
                    turn.group_session_id,
                    turn.user_message_id,
                    Some(seat.member_session_id),
                );
            }
            let filter = state
                .model_config
                .resolve_output_filter(turn.tier.as_deref());
            let random_draw = filter
                .as_ref()
                .and_then(|f| f.trigger.random)
                .map(|_| rand::thread_rng().gen::<f64>());
            let text_action = if plan.action_type == ActionType::ReplyTextImage {
                ActionType::ReplyTextImage
            } else {
                ActionType::ReplyText
            };
            let outcome = Arc::new(std::sync::Mutex::new(BurstOutcome::default()));
            let burst = drive_chat_burst_for(
                state.clone(),
                turn.group_session_id,
                turn.user_message_id,
                frame_action_for(text_action),
                "reply",
                text_action,
                req,
                state.model_config.display_override("chat_companion"),
                filter,
                injected_tags,
                turn.tier.clone(),
                Default::default(),
                Default::default(),
                random_draw,
                outcome.clone(),
                chain,
                Some(seat.member_session_id),
            );
            futures_util::pin_mut!(burst);
            let mut failed = false;
            while let Some(frame) = burst.next().await {
                if let ProtocolFrame::Error { code, message, .. } = frame {
                    yield ProtocolFrame::SpeakerError {
                        speaker_session_id: seat.member_session_id,
                        code,
                        message,
                    };
                    failed = true;
                    break;
                }
                yield frame;
            }
            if failed {
                continue;
            }
            let (produced, filtered, rc, rf, failures) = {
                let g = outcome.lock().unwrap();
                (
                    g.produced.clone(),
                    g.filtered,
                    g.retries_chat,
                    g.retries_filter,
                    g.failures.clone(),
                )
            };
            any_filtered |= filtered;
            retries_chat += rc;
            retries_filter += rf;
            turn_failures.extend(failures);

            if plan.action_type == ActionType::ReplyTextImage {
                if let Some(last) = produced.last() {
                    let img = match compose.join().await {
                        Some(Ok(v)) => v,
                        failed => {
                            if let Some(Err(e)) = failed {
                                tracing::warn!("image-compose task failed: {e}");
                            }
                            build_delegated_image_prompt(
                                &state,
                                &seat.persona,
                                &plan,
                                turn.image.as_ref(),
                                &transcript,
                                &latest,
                                turn.user_id,
                                turn.group_session_id,
                                None,
                            )
                            .await
                        }
                    };
                    turn_failures.extend(img.failures.iter().cloned());
                    let marker = build_delegated_image_marker(
                        &img.subject,
                        img.caption.as_deref(),
                        img.aspect_ratio.as_deref(),
                        img.compose_variant.as_deref(),
                        img.compose_model.as_deref(),
                        img.compose_generation_id.as_deref(),
                        img.compose_event_id,
                        plan.image_ref,
                        None,
                    );
                    if let Err(e) = chat_repo
                        .merge_assistant_image_meta(turn.group_session_id, last.message_id, &marker)
                        .await
                    {
                        tracing::warn!("group(text_image): merge marker failed: {e}");
                    }
                    yield build_image_request_frame(
                        ulid_string(Ulid::from(last.message_id)),
                        &img.composed_prompt,
                        plan.image_ref,
                        img.aspect_ratio.as_deref(),
                    );
                }
            }
            if !produced.is_empty() {
                spoken.push(GroupSpoken {
                    speaker_session_id: seat.member_session_id,
                    instance_id: seat.instance_id,
                    name: seat.persona.genome.name.clone(),
                    produced,
                });
            }
        }

        if spoken.is_empty() {
            yield ProtocolFrame::Error {
                code: StreamErrorCode::UpstreamUnavailable,
                retryable: true,
                message: "no group member produced a reply".into(),
                user_message: "暂时没有人回复，请稍后再试".into(),
                upstream_status: None,
                provider_code: None,
            };
            return;
        }

        let (attempts, gateways) = split_failures_typed(&turn_failures);
        yield build_final_frame(
            any_filtered,
            prompt_injected,
            turn.tier.clone(),
            retries_chat,
            retries_filter,
            attempts,
            gateways,
        );

        let members: Vec<(Uuid, Uuid)> = seats
            .iter()
            .map(|s| (s.member_session_id, s.instance_id))
            .collect();
        let (st, gid, uid, msg) = (
            (*state).clone(),
            turn.group_session_id,
            turn.user_id,
            turn.content.clone(),
        );
        tokio::spawn(async move {
            crate::pipeline::post_process::run_group(st, gid, uid, msg, members, spoken).await;
        });
    }
}

/// The stream handler's driver: forwards every frame to the SSE tap and
/// classifies the turn for `settle_turn` (any `done` ⇒ success).
pub(crate) async fn drive_group_to_exhaustion(
    state: Arc<AppState>,
    turn: GroupTurn,
    tap: tokio::sync::mpsc::UnboundedSender<ProtocolFrame>,
) -> TurnOutcome {
    use futures_util::StreamExt as _;
    let s = run_group_turn(state, turn);
    futures_util::pin_mut!(s);
    let mut done = 0usize;
    let mut last_error: Option<String> = None;
    while let Some(frame) = s.next().await {
        match &frame {
            ProtocolFrame::Done { .. } => done += 1,
            ProtocolFrame::Error { message, .. } => last_error = Some(message.clone()),
            _ => {}
        }
        let _ = tap.send(frame);
    }
    classify_outcome(done, last_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repetition::Injected;

    fn inj(id: Uuid, role: &str, text: &str) -> Injected {
        Injected {
            id,
            role: role.into(),
            text: text.into(),
        }
    }

    #[test]
    fn speaker_view_relabels_by_speaker() {
        let (me, other) = (Uuid::new_v4(), Uuid::new_v4());
        let (u, a_me, a_other, a_gone) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let raw = vec![
            inj(u, "user", "早"),
            inj(a_other, "assistant", "早上好呀。今天去哪"),
            inj(a_me, "assistant", "哈喽。我也在"),
            inj(a_gone, "assistant", "我先走了"),
        ];
        // The noise strip only ever applies to the speaker's own lines.
        let stripped = vec![
            inj(u, "user", "早"),
            inj(a_other, "assistant", "今天去哪"),
            inj(a_me, "assistant", "我也在"),
        ];
        let speakers = HashMap::from([
            (u, None),
            (a_other, Some(other)),
            (a_me, Some(me)),
            (a_gone, None),
        ]);
        let view = GroupSpeakerView {
            group_session_id: Uuid::new_v4(),
            speaker_session_id: me,
            names: HashMap::from([(other, "小美".to_string())]),
            block: String::new(),
        };
        let out = speaker_view(raw, stripped, &speakers, &view);
        let pairs: Vec<(&str, &str)> = out
            .iter()
            .map(|m| (m.role.as_str(), m.text.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("user", "用户：早"),
                ("user", "小美：早上好呀。今天去哪"),
                ("assistant", "我也在"),
                ("user", "某位群友：我先走了"),
            ]
        );
    }

    #[test]
    fn speaker_view_drops_own_lines_the_strip_emptied() {
        let me = Uuid::new_v4();
        let a = Uuid::new_v4();
        let raw = vec![inj(a, "assistant", "嗯。")];
        let stripped = vec![]; // model_facing_history dropped it
        let speakers = HashMap::from([(a, Some(me))]);
        let view = GroupSpeakerView {
            group_session_id: Uuid::new_v4(),
            speaker_session_id: me,
            names: HashMap::new(),
            block: String::new(),
        };
        assert!(speaker_view(raw, stripped, &speakers, &view).is_empty());
    }

    fn row(
        role: &str,
        content: &str,
        speaker: Option<Uuid>,
        metadata: Option<serde_json::Value>,
    ) -> ChatMessage {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "session_id": Uuid::nil(),
            "role": role,
            "content": content,
            "sent_at": chrono::Utc::now(),
            "speaker_session_id": speaker,
            "metadata": metadata,
        }))
        .unwrap()
    }

    #[test]
    fn transcript_marks_photos_and_skips_empty_lines() {
        let a = Uuid::new_v4();
        let rows = vec![
            row("user", "早", None, None),
            row("assistant", "", Some(a), None),
            row(
                "assistant",
                "",
                Some(a),
                Some(serde_json::json!({"image": {"prompt": "x"}})),
            ),
            row("assistant", "嗨", Some(a), None),
        ];
        let names = HashMap::from([(a, "A".to_string())]);
        let text = transcript_text(&rows, &names);
        assert_eq!(
            text.split('\n').collect::<Vec<_>>(),
            vec!["用户：早", "A：（发了一张照片）", "A：嗨"]
        );
    }

    #[test]
    fn group_block_is_byte_exact() {
        let b = group_block(
            Some("周末爬山"),
            &[("小美".into(), Some("活泼".into())), ("阿哲".into(), None)],
        );
        assert_eq!(
            b,
            "\n\n[群聊]\n你现在在群聊「周末爬山」里。\n在场的还有：\n- 小美：活泼\n- 阿哲\n只说你自己的话，不替别人说话，不在开头写名字。"
        );
        let alone = group_block(None, &[]);
        assert_eq!(
            alone,
            "\n\n[群聊]\n你现在在一个群聊里。\n群里只有你和对方。\n只说你自己的话，不替别人说话，不在开头写名字。"
        );
    }
}

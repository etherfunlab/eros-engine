// SPDX-License-Identifier: AGPL-3.0-only
//! Chat request builders — assemble an `eros_engine_llm::openrouter::ChatRequest`
//! for the streaming pipeline based on the PDE's `ActionPlan`.
//!
//! OSS specifics: all DB I/O goes through `eros_engine_store` repos, and the
//! model / fallback / allow_traits are resolved via `state.model_config`
//! (task + per-request tier).

use sqlx::PgPool;
use uuid::Uuid;

use eros_engine_core::scope::{AffinityScope, InsightMode, MemoryScope};
use eros_engine_core::types::{ActionPlan, DecisionInput, Event, LlmAudit, PromptTrait};
use eros_engine_llm::model_config::{style_preset, ResolvedModel, StyleKey};
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
use eros_engine_store::affinity::AffinityRepo;
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::human_insight::{HumanInsightRepo, HumanInsightsRow};
use eros_engine_store::memory::MemoryRepo;

use crate::error::AppError;
use crate::prompt::build_prompt;
use crate::state::AppState;

/// Memory recall fan-out sizes — mirror the gateway's Mem0 era defaults
/// (`profile=4`, `relationship=3`). Tunable later if recall quality drifts.
const PROFILE_RECALL_K: i32 = 4;
const RELATIONSHIP_RECALL_K: i32 = 3;
/// Per-category top-K for the dreaming-lite categorised profile rows.
/// Five categories × 2 = at most 10 lines of grouped profile context;
/// kept small so the prompt doesn't bloat once classification fills in.
const K_PER_CATEGORY: i32 = 2;

/// Recall fan-out sizes for one call site of `recall_memory` /
/// `recall_memory_with_embedding`. Parameterised so callers other than the
/// text-chat path (e.g. voice) can reuse the same search + grouping logic
/// with a smaller tier without touching the constants below.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecallTier {
    pub grouped_k: i32,
    pub raw_k: i32,
    pub relationship_k: i32,
}

/// Text-chat recall tier — same values as the historical unparameterised
/// constants, now passed explicitly.
pub(crate) const TEXT_RECALL_TIER: RecallTier = RecallTier {
    grouped_k: K_PER_CATEGORY,
    raw_k: PROFILE_RECALL_K,
    relationship_k: RELATIONSHIP_RECALL_K,
};

/// World-memories fragment recall size (spec §3.2).
const WORLD_RECALL_K: i32 = 3;
/// World-stories episode recall size (stories spec §5.3).
const STORY_RECALL_K: i32 = 3;

/// Task key used by all chat handlers. Matches the gateway's task router.
const CHAT_TASK: &str = "chat_companion";

/// Maximum number of recent messages pulled into the prompt.
const HISTORY_WINDOW: i64 = 20;

/// Partition caller traits by a tier's resolved allow-list.
/// - `allow == None` → no gating: all kept, none dropped.
/// - `allow == Some(set)` → keep only traits whose `tag` ∈ `set`; the rest
///   are dropped and their tags returned for logging (text is never logged).
fn filter_traits(
    traits: &[PromptTrait],
    allow: Option<&[String]>,
) -> (Vec<PromptTrait>, Vec<String>) {
    match allow {
        None => (traits.to_vec(), Vec::new()),
        Some(set) => {
            let mut kept = Vec::new();
            let mut dropped = Vec::new();
            for t in traits {
                if set.iter().any(|a| a == &t.tag) {
                    kept.push(t.clone());
                } else {
                    dropped.push(t.tag.clone());
                }
            }
            (kept, dropped)
        }
    }
}

/// Extract the caller-supplied OpenRouter audit passthrough off the
/// `Event` driving this turn. Returns `None` for non-`UserMessage` events
/// (gift / proactive paths cannot supply audit today — out of scope for
/// the v1 audit feature).
pub(in crate::pipeline) fn audit_from_event(event: &Event) -> Option<&LlmAudit> {
    match event {
        Event::UserMessage { audit, .. } => audit.as_ref(),
        _ => None,
    }
}

/// Effective model-facing text for a user-side history row: the input-filter
/// rewrite (`pre_filter_content`) when present and non-blank, else the original
/// `content`. Assistant rows must NOT use this (their `pre_filter_content` is
/// the pre-OUTPUT-filter original); `model_facing_history` routes assistant
/// rows to `content` directly.
pub(crate) fn effective_user_text(msg: &eros_engine_store::chat::ChatMessage) -> &str {
    match msg.pre_filter_content.as_deref() {
        Some(s) if !s.trim().is_empty() => s,
        _ => &msg.content,
    }
}

/// Model-facing text for an assistant history row: the stored `content`, with a
/// `[你的照片：{caption}]` marker appended when `metadata.image` is present.
/// Used by `model_facing_history` so the model knows it previously sent an
/// image in that turn. The marker is a possessive noun phrase, not a sentence
/// about sending: a verb here reads as an example of the persona doing it, and
/// models take it as one.
pub(crate) fn model_facing_assistant_text(msg: &eros_engine_store::chat::ChatMessage) -> String {
    let mut text = msg.content.clone();
    if let Some(img) = msg.metadata.as_ref().and_then(|md| md.get("image")) {
        let caption = img
            .get("caption")
            .and_then(|p| p.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // Caption, never `prompt`: the prompt is the image-generation string
        // (style boilerplate + appearance + subject) and injecting it here put
        // long English prompt text into a Chinese roleplay history, which
        // models then echoed. No caption ⇒ bare marker, no fallback.
        let marker = match caption {
            Some(c) => format!("[你的照片：{c}]"),
            None => "[你的照片]".to_string(),
        };
        if text.trim().is_empty() {
            text = marker;
        } else {
            text.push_str("\n\n");
            text.push_str(&marker);
        }
    }
    text
}

/// Build the `[用户发送了一张图片]` preamble from a stored `metadata.vision`
/// object. Returns `None` when `description` is absent/blank (not a usable
/// describe). Blank optional fields are omitted line-by-line.
fn build_image_preamble(vision: &serde_json::Value) -> Option<String> {
    let field = |k: &str| {
        vision
            .get(k)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let description = field("description")?;
    let mut lines = vec![
        "[用户发送了一张图片]".to_string(),
        format!("画面：{description}"),
    ];
    if let Some(t) = field("ocr_text") {
        lines.push(format!("文字：{t}"));
    }
    if let Some(p) = field("people") {
        lines.push(format!("人物：{p}"));
    }
    if let Some(s) = field("scene") {
        lines.push(format!("场景：{s}"));
    }
    Some(lines.join("\n"))
}

/// What the MAIN chat model should see for a user row: an optional image
/// preamble (from `metadata.vision`, or a neutral placeholder when an image was
/// sent but not described) folded onto `effective_user_text(msg)`. A plain text
/// turn (no `vision`, no `image_url`) returns the effective text unchanged.
pub(crate) fn model_facing_user_text(msg: &eros_engine_store::chat::ChatMessage) -> String {
    let base = effective_user_text(msg);
    let meta = msg.metadata.as_ref();
    let preamble = meta
        .and_then(|m| m.get("vision"))
        .and_then(build_image_preamble)
        .or_else(|| {
            // Image sent but not described (vision failed) → neutral placeholder.
            meta.and_then(|m| m.get("image_url"))
                .map(|_| "[用户发送了一张图片，但内容无法识别]".to_string())
        });
    match preamble {
        Some(p) => {
            let body = if base.trim().is_empty() {
                "[用户未附文字]"
            } else {
                base
            };
            format!("{p}\n\n{body}")
        }
        None => base.to_string(),
    }
}

/// Recall query text for a user row: the caption (`effective_user_text`) when
/// non-blank, else the vision `description` for an image-only turn so memory
/// recall can match the photo's content instead of running on empty text. Used
/// ONLY for the recall/embedding query — the prompt path uses
/// `model_facing_user_text` (which folds the full preamble).
pub(crate) fn recall_query_text(msg: &eros_engine_store::chat::ChatMessage) -> String {
    let caption = effective_user_text(msg);
    if !caption.trim().is_empty() {
        return caption.to_string();
    }
    msg.metadata
        .as_ref()
        .and_then(|m| m.get("vision"))
        .and_then(|v| v.get("description"))
        .and_then(|d| d.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

/// Map chronological history rows to what will actually reach the model.
///
/// Channel-marked rows (voice / product_qa) are out of companion context —
/// they must never re-enter the model's own conversation history, even though
/// `history()` itself stays unfiltered for the client route and the voice
/// window. Unknown roles are skipped. `user` and TIP `gift_user` rows both
/// fold to the "user" role: a tip turn IS a user turn to the model
/// (OpenRouter only knows system/user/assistant), and gift_user is tip-only
/// now (the legacy in-app Gift Event endpoint was removed), so no tip/legacy
/// gate is needed. Assistant rows feed `content` (their `pre_filter_content`
/// is the pre-output-filter original and must never re-enter the prompt),
/// then have their leading sentence stripped (spec §4.3 — the noise carrier,
/// e.g. `唔`/`啊`); a row with nothing left after stripping is omitted rather
/// than injected as empty content, which some providers reject. User rows are
/// never stripped.
///
/// Split out of `assemble_chat_request` so echo cancellation can key on the
/// exact string the provider receives — no other layer can compute it.
pub(crate) fn model_facing_history(
    history: Vec<eros_engine_store::chat::ChatMessage>,
) -> Vec<crate::repetition::Injected> {
    let mut out = Vec::with_capacity(history.len());
    for msg in history {
        if msg.channel.is_some() {
            continue;
        }
        let (role, text) = match msg.role.as_str() {
            "user" | "gift_user" => ("user", model_facing_user_text(&msg)),
            "assistant" => {
                // Spec §4.3: the leading sentence is the noise carrier. Strip
                // it; a row with nothing left is dropped rather than injected
                // as empty content, which some providers reject.
                let stripped =
                    crate::repetition::strip_leading_sentence(&model_facing_assistant_text(&msg));
                if stripped.trim().is_empty() {
                    continue;
                }
                ("assistant", stripped)
            }
            _ => continue,
        };
        out.push(crate::repetition::Injected {
            id: msg.id,
            role: role.to_string(),
            text,
        });
    }
    out
}

/// Apply echo cancellation to a turn's injected history unless the operator
/// disabled it, logging one line when anything was dropped. `session_id` is
/// carried for that log line only.
///
/// No content and no content hash is logged: production sessions are real
/// conversations.
fn apply_echo_cancellation(
    injected: Vec<crate::repetition::Injected>,
    current_id: Uuid,
    disabled: bool,
    session_id: Uuid,
) -> Vec<crate::repetition::Injected> {
    if disabled {
        return injected;
    }
    let (kept, stats) = crate::repetition::cancel_echo(injected, current_id);
    if stats.dropped > 0 {
        tracing::info!(
            dropped = stats.dropped,
            kept = stats.kept,
            groups = stats.groups,
            max_occ = stats.max_occ,
            session_id = %session_id,
            "echo cancellation: duplicate history messages dropped"
        );
    }
    kept
}

/// Materialise a ChatRequest from a pre-resolved model + system prompt +
/// already-materialised history (see `model_facing_history`). `audit` carries
/// the caller's OpenRouter passthrough when the driving event was a
/// `UserMessage`; gift / proactive pass `None`.
fn assemble_chat_request(
    resolved: ResolvedModel,
    system_prompt: String,
    injected: Vec<crate::repetition::Injected>,
    audit: Option<&LlmAudit>,
) -> ChatRequest {
    let mut messages = Vec::with_capacity(injected.len() + 1);
    messages.push(ChatMessage {
        role: "system".to_string(),
        content: system_prompt,
    });
    for m in injected {
        messages.push(ChatMessage {
            role: m.role,
            content: m.text,
        });
    }

    let (audit_user, audit_session, audit_metadata) = audit
        .map(|a| (a.user.clone(), a.session_id.clone(), a.metadata.clone()))
        .unwrap_or_default();

    ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages,
        temperature: resolved.temperature as f32,
        sampling: resolved.sampling,
        max_tokens: resolved.max_tokens,
        user: audit_user,
        session_id: audit_session,
        metadata: audit_metadata,
        reasoning: resolved.reasoning,
        task: Some(CHAT_TASK.into()),
        ..Default::default()
    }
}

/// Compose the final image-gen prompt: style preset + optional persona
/// appearance + subject. Pure.
pub(crate) fn compose_image_prompt(
    style: StyleKey,
    persona: &eros_engine_core::persona::CompanionPersona,
    subject: &str,
) -> String {
    let mut parts: Vec<String> = vec![style_preset(style).to_string()];
    if let Some(a) = crate::prompt::meta_str(persona, "appearance")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        parts.push(a.to_string());
    }
    let subject = subject.trim();
    if !subject.is_empty() {
        parts.push(subject.to_string());
    }
    parts.join("\n")
}

// ─── Memory recall + insight injection helpers ────────────────────

/// Embed `query_text` once, then delegate to `recall_memory_with_embedding`.
/// Returns (empty, empty, None) without hitting Voyage when both layers are
/// off or the query is blank. Voyage failure also degrades silently to
/// (empty, empty, None) — recall failure must never block a chat reply (the
/// persona just looks slightly less "with it" for that turn). The third
/// element is the computed query embedding (`Some` only on success), reused
/// by `fetch_world_context` so world-fragment recall doesn't pay a second
/// Voyage call.
pub(crate) async fn recall_memory(
    state: &AppState,
    user_id: Uuid,
    instance_id: Uuid,
    query_text: &str,
    x_on: bool,
    y_on: bool,
    tier: RecallTier,
) -> (Vec<(String, Vec<String>)>, Vec<String>, Option<Vec<f32>>) {
    if (!x_on && !y_on) || query_text.trim().is_empty() {
        return (vec![], vec![], None);
    }
    let embedding = match state.embed.embed_query(query_text).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("embed_query failed: {e}");
            return (vec![], vec![], None);
        }
    };
    tracing::debug!(
        user_id = %user_id,
        query_len = query_text.chars().count(),
        embedding_dim = embedding.len(),
        x_on,
        y_on,
        "recall_memory: embedded query, dispatching pgvector search"
    );
    let (profile_groups, relationship) = recall_memory_with_embedding(
        &state.pool,
        user_id,
        instance_id,
        &embedding,
        x_on,
        y_on,
        tier,
    )
    .await;
    (profile_groups, relationship, Some(embedding))
}

/// Pure-DB inner half of memory recall. Takes a pre-computed embedding and
/// layer-enable flags, then returns:
/// - profile_groups: `Vec<(label, bullets)>` — categorised rows grouped by
///   `category` if any exist; otherwise a single `("近况", raw_rows)` group
///   so users with no classified sessions yet still get profile context.
///   Empty when `x_on` is false.
/// - relationship: flat `Vec<String>` — relationship rows are full turn
///   dumps and not categorised by the dreaming-lite pass. Empty when `y_on`
///   is false.
///
/// Hot path (`x_on` ⇒ `y_on`): the three profile + relationship searches run
/// in parallel via `tokio::join!`. Relationship-only (`!x_on && y_on`): only
/// the relationship search runs. Both off: no DB round-trip.
pub(crate) async fn recall_memory_with_embedding(
    pool: &PgPool,
    user_id: Uuid,
    instance_id: Uuid,
    embedding: &[f32],
    x_on: bool,
    y_on: bool,
    tier: RecallTier,
) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    let repo = MemoryRepo { pool };

    let (profile_groups, relationship): (Vec<(String, Vec<String>)>, Vec<String>) = if x_on {
        // X on ⇒ Y on: original three-way parallel recall (hot path).
        let (grouped_res, raw_res, rel_res) = tokio::join!(
            repo.search_profile_grouped(user_id, embedding, tier.grouped_k),
            repo.search(user_id, None, embedding, tier.raw_k),
            repo.search(user_id, Some(instance_id), embedding, tier.relationship_k),
        );
        let grouped_rows = grouped_res.unwrap_or_else(|e| {
            tracing::warn!("profile-layer grouped search failed: {e}");
            vec![]
        });
        let raw_rows = raw_res.unwrap_or_else(|e| {
            tracing::warn!("profile-layer raw search failed: {e}");
            vec![]
        });
        let rel = match rel_res {
            Ok(rows) => rows.into_iter().map(|r| r.content).collect(),
            Err(e) => {
                tracing::warn!("relationship-layer memory search failed: {e}");
                vec![]
            }
        };
        (build_profile_groups(grouped_rows, raw_rows), rel)
    } else if y_on {
        // relationship_only: skip both profile-layer searches.
        let rel = match repo
            .search(user_id, Some(instance_id), embedding, tier.relationship_k)
            .await
        {
            Ok(rows) => rows.into_iter().map(|r| r.content).collect(),
            Err(e) => {
                tracing::warn!("relationship-layer memory search failed: {e}");
                vec![]
            }
        };
        (vec![], rel)
    } else {
        // Unreachable via MemoryScope::resolve() (x_on ⇒ y_on); defensive for
        // any direct caller that passes both layers off.
        (vec![], vec![])
    };

    let profile_total_chars: usize = profile_groups
        .iter()
        .flat_map(|(_, items)| items.iter().map(|s| s.chars().count()))
        .sum();
    tracing::debug!(
        user_id = %user_id,
        instance_id = %instance_id,
        x_on,
        y_on,
        profile_groups = profile_groups.len(),
        profile_total_chars,
        relationship_hits = relationship.len(),
        relationship_total_chars = relationship.iter().map(|s| s.chars().count()).sum::<usize>(),
        "recall_memory_with_embedding: completed"
    );
    (profile_groups, relationship)
}

/// Map a raw category tag (`fact` / `preference` / ...) to its Chinese
/// section label as it should appear in the prompt. Unknown tags fall
/// back to "其他" — the dreaming-lite classifier already normalises to a
/// fixed vocabulary, so this branch should be unreachable in practice.
fn category_label(category: &str) -> &'static str {
    match category {
        "fact" => "客观事实",
        "preference" => "偏好",
        "event" => "最近发生",
        "emotion" => "情绪倾向",
        "relation" => "人际关系",
        _ => "其他",
    }
}

/// Turn the SQL outputs into the grouped shape `build_prompt` expects.
///
/// - If any categorised rows exist: render only those, grouped by
///   category, in the order returned by SQL (already sorted by category
///   then per-category proximity).
/// - Otherwise: fall back to the flat top-K raw rows under a single
///   "近况" label so newly-onboarded users still get profile context
///   before their first dreaming sweep runs.
fn build_profile_groups(
    grouped_rows: Vec<eros_engine_store::memory::MemoryRow>,
    raw_rows: Vec<eros_engine_store::memory::MemoryRow>,
) -> Vec<(String, Vec<String>)> {
    if !grouped_rows.is_empty() {
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for row in grouped_rows {
            let cat = row.category.clone().unwrap_or_default();
            let label = category_label(&cat).to_string();
            match out.last_mut() {
                Some((existing, items)) if existing == &label => items.push(row.content),
                _ => out.push((label, vec![row.content])),
            }
        }
        return out;
    }
    if !raw_rows.is_empty() {
        return vec![(
            "近况".into(),
            raw_rows.into_iter().map(|r| r.content).collect(),
        )];
    }
    vec![]
}

/// Render a `human_insights` row as 基础画像 bullets. `InsightMode::Full`
/// renders every field in canonical label order; `Neutral` drops the
/// intimate fields (love_values / relationship_history / interests /
/// emotional_needs / family / finance_status).
/// Matching-only columns (preferred_gender / age / deal_breakers) are never
/// rendered. `Off` → empty (defensive; loaders gate it before calling).
pub(crate) fn human_insights_to_bullets(row: &HumanInsightsRow, mode: InsightMode) -> Vec<String> {
    if matches!(mode, InsightMode::Off) {
        return vec![];
    }
    let mut out = Vec::new();
    let push_str = |out: &mut Vec<String>, val: &Option<String>, label: &str| {
        if let Some(s) = val {
            let s = s.trim();
            if !s.is_empty() {
                out.push(format!("{label}：{s}"));
            }
        }
    };
    let push_arr = |out: &mut Vec<String>, val: &[String], label: &str| {
        let parts: Vec<&str> = val
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !parts.is_empty() {
            out.push(format!("{label}：{}", parts.join("、")));
        }
    };
    let intimate = matches!(mode, InsightMode::Full);

    push_str(&mut out, &row.city, "城市");
    push_str(&mut out, &row.location, "所在地");
    push_str(&mut out, &row.hometown, "老家");
    push_str(&mut out, &row.nationality, "国籍");
    push_str(&mut out, &row.occupation, "职业");
    push_str(&mut out, &row.education, "教育");
    push_str(&mut out, &row.mbti_guess, "MBTI");
    if intimate {
        push_str(&mut out, &row.love_values, "感情观");
        push_str(&mut out, &row.relationship_history, "感情经历");
        push_arr(&mut out, &row.interests, "兴趣");
        push_str(&mut out, &row.emotional_needs, "情感需求");
        push_str(&mut out, &row.family, "家庭");
        push_str(&mut out, &row.finance_status, "经济状况");
    }
    push_str(&mut out, &row.life_rhythm, "作息");
    push_str(&mut out, &row.social_pattern, "社交模式");
    push_arr(&mut out, &row.personality_traits, "性格特质");
    push_str(&mut out, &row.future_plans, "未来计划");
    out
}

/// Load + render 基础画像 from the flat `human_insights` mirror. `Off` → empty.
async fn load_human_insight_bullets(
    pool: &PgPool,
    user_id: Uuid,
    mode: InsightMode,
) -> Vec<String> {
    if matches!(mode, InsightMode::Off) {
        return vec![];
    }
    let repo = HumanInsightRepo { pool };
    match repo.load(user_id).await {
        Ok(Some(row)) => human_insights_to_bullets(&row, mode),
        Ok(None) => vec![],
        Err(e) => {
            tracing::warn!("human_insights load failed: {e}");
            vec![]
        }
    }
}

/// World-memories chat-time fetch (spec §3.2). Returns `None` — and the
/// prompt stays byte-identical — when the subsystem or injection is
/// disabled, the owner isn't enrolled, or this persona has no digest yet.
/// Fragment recall REUSES the query embedding computed by the standard
/// memory recall; when that path was skipped, world degrades to digest-only
/// rather than paying a second Voyage call. Any DB error degrades to `None`
/// with a warn — world data must never block a chat reply.
async fn fetch_world_context(
    state: &AppState,
    user_id: Uuid,
    instance_id: Uuid,
    query_embedding: Option<&[f32]>,
) -> Option<crate::prompt::WorldContext> {
    if state.config.world.disabled || state.config.world.prompt_disabled || !state.world_configured
    {
        return None;
    }
    let repo = eros_engine_store::world::WorldRepo { pool: &state.pool };
    let digest = match repo.fetch_digest(user_id, instance_id).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("world digest fetch failed: {e}");
            return None;
        }
    };
    let digest = digest?; // unenrolled / no state / no entry ⇒ no injection
    let fragments = match query_embedding {
        Some(emb) => repo
            .search_fragments(user_id, instance_id, emb, WORLD_RECALL_K)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("world fragment search failed: {e}");
                vec![]
            }),
        None => vec![],
    };
    Some(crate::prompt::WorldContext { digest, fragments })
}

/// World-stories chat-time fetch (stories spec §5.3). Same degradation
/// ladder as `fetch_world_context`: disabled/unconfigured/unflagged/no-digest
/// ⇒ `None` and the prompt stays byte-identical; episode recall reuses the
/// turn's query embedding (digest-only when absent); any DB error degrades
/// with a warn — story data must never block a reply. Also requires the
/// World Memories base (`world_configured`), mirroring how the story
/// sweeper itself refuses to run without `[tasks.world_director]`
/// configured — stories are an add-on layer on top of WM, not standalone.
async fn fetch_stories_context(
    state: &AppState,
    user_id: Uuid,
    instance_id: Uuid,
    query_embedding: Option<&[f32]>,
) -> Option<crate::prompt::StoriesContext> {
    if state.config.world.disabled
        || state.config.world.stories_disabled
        || state.config.world.stories_prompt_disabled
        || !state.world_configured
        || !state.stories_configured
    {
        return None;
    }
    let repo = eros_engine_store::story::StoryRepo { pool: &state.pool };
    let digest = match repo.fetch_story_digest(user_id, instance_id).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("story digest fetch failed: {e}");
            return None;
        }
    };
    let digest = digest?;
    let episodes = match query_embedding {
        Some(emb) => repo
            .search_story_memories(user_id, instance_id, emb, STORY_RECALL_K)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("story memory search failed: {e}");
                vec![]
            }),
        None => vec![],
    };
    Some(crate::prompt::StoriesContext { digest, episodes })
}

// ─── Reply ──────────────────────────────────────────────────────────

/// Build a ChatRequest for the Reply action. Called by the streaming
/// pipeline (`pipeline::stream::run_stream`).
pub(super) async fn build_reply_request(
    state: &AppState,
    input: &DecisionInput,
    plan: &ActionPlan,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    user_message_id: Uuid,
) -> Result<(ChatRequest, Vec<String>), AppError> {
    let chat_repo = ChatRepo { pool: &state.pool };
    // A quote points at one line; it never narrows the window. Every turn gets
    // the same newest-N history, so quoting something from last week does not
    // cost the model everything said since.
    let quote = match &input.event {
        eros_engine_core::types::Event::UserMessage { quote, .. } => quote.as_ref(),
        _ => None,
    };
    let history = {
        let mut rows = chat_repo.history(session_id, HISTORY_WINDOW, 0).await?;
        // The prompt's model-facing messages come ONLY from this vector, so
        // a driving row missing from it means the model answers a message
        // it never sees. On the stream path the driving row is always the
        // newest row and this is a no-op; on the async worker path a LIFO
        // burst (or anything that landed while the turn waited) can bury it
        // past HISTORY_WINDOW. Pin it back at its chronological position —
        // older than everything in the window, so it goes first.
        if !rows.iter().any(|m| m.id == user_message_id) {
            if let Some(driving) = chat_repo
                .message_by_id_in_session(session_id, user_message_id)
                .await?
            {
                rows.insert(0, driving);
            }
        }
        rows
    };

    // Recall query for the current user turn: the effective caption, or — for an
    // image-only turn — the vision description (recall_query_text), so a photo
    // with no caption still retrieves relevant memories. The MAIN prompt path
    // separately folds the full image preamble via model_facing_user_text.
    let query_text: String = history
        .iter()
        .rev()
        .find(|m| m.id == user_message_id && m.role == "user")
        .map(recall_query_text)
        .unwrap_or_else(|| match &input.event {
            Event::UserMessage { content, .. } => content.clone(),
            _ => String::new(),
        });

    let (memory_scope, affinity_scope) = match &input.event {
        Event::UserMessage {
            memory_scope,
            affinity_scope,
            ..
        } => (*memory_scope, *affinity_scope),
        _ => (MemoryScope::default(), AffinityScope::default()),
    };
    let (mem_mode, x_on, y_on) = memory_scope.resolve();
    // Routine turns use the defaults — keep those at debug. Surface only
    // caller-overridden scopes at info, where they're actually notable.
    if memory_scope != MemoryScope::default() || affinity_scope != AffinityScope::default() {
        tracing::info!(
            memory_scope = ?memory_scope,
            affinity_axes_active = affinity_scope.active_count(),
            x_on,
            y_on,
            "chat scopes resolved (non-default)"
        );
    } else {
        tracing::debug!(
            memory_scope = ?memory_scope,
            affinity_axes_active = affinity_scope.active_count(),
            "chat scopes resolved (defaults)"
        );
    }

    let (mut profile_groups, relationship_facts, query_embedding) = recall_memory(
        state,
        user_id,
        instance_id,
        &query_text,
        x_on,
        y_on,
        TEXT_RECALL_TIER,
    )
    .await;

    let insight_bullets = load_human_insight_bullets(&state.pool, user_id, mem_mode).await;
    if !insight_bullets.is_empty() {
        profile_groups.insert(0, ("基础画像".into(), insight_bullets));
    }

    let tier = match &input.event {
        Event::UserMessage { tier, .. } => tier.as_deref(),
        _ => None,
    };
    let resolved = state.model_config.resolve(CHAT_TASK, tier);

    let requested_traits: &[PromptTrait] = match &input.event {
        Event::UserMessage { prompt_traits, .. } => prompt_traits.as_slice(),
        _ => &[],
    };
    let (kept_traits, dropped_tags) =
        filter_traits(requested_traits, resolved.allow_traits.as_deref());
    if !dropped_tags.is_empty() {
        tracing::info!(
            tier = tier.unwrap_or("<none>"),
            kept = kept_traits.len(),
            dropped_tags = ?dropped_tags,
            "prompt_traits: dropped tags not allowed for tier"
        );
    }

    // Most recent affinity reason for [emotional_context] (single row, not a trajectory).
    let emotional_context = AffinityRepo { pool: &state.pool }
        .recent_emotional_reasons(session_id, user_message_id, 1)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                session_id = %session_id,
                "recent_emotional_reasons fetch failed; [emotional_context] omitted"
            );
            Vec::new()
        });

    // #113: dedup recalled memories (mainly the cross-layer 用户：{u} / {u}
    // overlap) before they enter the prompt. Pure; no new DB calls.
    let (profile_groups, relationship_facts) =
        crate::memory_hygiene::prune_recalled(profile_groups, relationship_facts);

    let world = fetch_world_context(state, user_id, instance_id, query_embedding.as_deref()).await;
    let stories =
        fetch_stories_context(state, user_id, instance_id, query_embedding.as_deref()).await;

    // Spec §4.5 / §4.6: one PK read. Non-fatal — a DB hiccup omits the block
    // and falls back to the widest window, which is the safe direction.
    let character_state =
        eros_engine_store::character_insight::CharacterInsightRepo { pool: &state.pool }
            .load(instance_id)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = %e,
                    instance_id = %instance_id,
                    "character_insights load failed; [character_state] omitted"
                );
                None
            });

    let mut system_prompt = build_prompt(
        &input.persona,
        &profile_groups,
        &relationship_facts,
        Some(&input.affinity),
        plan.reply_style,
        &plan.context_hints,
        plan.reply_tone.as_deref(),
        &kept_traits,
        affinity_scope,
        &emotional_context,
        world.as_ref(),
        stories.as_ref(),
        quote,
        character_state.as_ref(),
    );

    if let Event::UserMessage {
        tips_amount_usd: Some(amount),
        ..
    } = &input.event
    {
        // Raw Option from the genome: tips_reaction_context renders Some vs None as
        // different prose, so the distinction must survive.
        let tp = input.persona.genome.tip_personality.as_deref();
        system_prompt.push_str(&crate::prompt::tips_reaction_context(*amount, tp));
    }

    let injected_history = apply_echo_cancellation(
        model_facing_history(history),
        user_message_id,
        state.config.chat_echo_cancellation_disabled,
        session_id,
    );
    let injected_tags: Vec<String> = kept_traits.iter().map(|t| t.tag.clone()).collect();
    Ok((
        assemble_chat_request(
            resolved,
            system_prompt,
            injected_history,
            audit_from_event(&input.event),
        ),
        injected_tags,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Integration tests: recall_memory_with_embedding ──────────────────
    //
    // These exercise the pure-DB half of the recall pipeline against a
    // live Postgres (via `#[sqlx::test]`). The Voyage-dependent outer
    // wrapper `recall_memory` is intentionally not tested here — it would
    // either need a live Voyage key or a trait-mock indirection that
    // doesn't justify its weight for a single thin function.

    use crate::routes::companion::testutil::seed_persona_instance;
    use eros_engine_store::human_insight::HumanInsightRepo;
    use eros_engine_store::memory::{MemoryLayer, MemoryRepo};
    use sqlx::PgPool;

    /// Deterministic 512-dim "unit" vector with a single hot index. Two
    /// different seeds produce orthogonal vectors → cosine distance = 1.0;
    /// same seed → distance = 0.0. Lets us prove nearest-neighbour ordering
    /// without floating-point fuzz.
    fn unit_embedding(seed: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; 512];
        v[seed % 512] = 1.0;
        v
    }

    async fn make_session(pool: &PgPool, user_id: Uuid, instance_id: Option<Uuid>) -> Uuid {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn recall_memory_with_embedding_empty_db_returns_empty(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let (profile, relationship) = recall_memory_with_embedding(
            &pool,
            user_id,
            instance_id,
            &unit_embedding(7),
            true,
            true,
            TEXT_RECALL_TIER,
        )
        .await;
        assert!(profile.is_empty());
        assert!(relationship.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn recall_memory_with_embedding_isolates_layers(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, Some(instance_id)).await;
        let repo = MemoryRepo { pool: &pool };

        // Same content text + same seed embedding written to BOTH layers
        // — differentiated only by instance_id presence. Both have
        // category=NULL so the profile side hits the raw-fallback branch,
        // surfacing under the "近况" group label.
        repo.upsert(
            MemoryLayer::Profile,
            session_id,
            user_id,
            None,
            "profile fact",
            &unit_embedding(11),
            None,
            None,
        )
        .await
        .unwrap();
        repo.upsert(
            MemoryLayer::Relationship,
            session_id,
            user_id,
            Some(instance_id),
            "relationship fact",
            &unit_embedding(11),
            None,
            None,
        )
        .await
        .unwrap();

        let (profile_groups, relationship) = recall_memory_with_embedding(
            &pool,
            user_id,
            instance_id,
            &unit_embedding(11),
            true,
            true,
            TEXT_RECALL_TIER,
        )
        .await;
        assert_eq!(
            profile_groups,
            vec![("近况".to_string(), vec!["profile fact".to_string()])]
        );
        assert_eq!(relationship, vec!["relationship fact".to_string()]);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn recall_memory_with_embedding_groups_categorised_rows(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, Some(instance_id)).await;
        let repo = MemoryRepo { pool: &pool };

        // A categorised profile row trumps the raw-fallback branch.
        // Mix categorised + raw to confirm the raw row is excluded once
        // any categorised row exists.
        repo.upsert(
            MemoryLayer::Profile,
            session_id,
            user_id,
            None,
            "lives in shanghai",
            &unit_embedding(7),
            Some("fact"),
            None,
        )
        .await
        .unwrap();
        repo.upsert(
            MemoryLayer::Profile,
            session_id,
            user_id,
            None,
            "loves coffee",
            &unit_embedding(8),
            Some("preference"),
            None,
        )
        .await
        .unwrap();
        repo.upsert(
            MemoryLayer::Profile,
            session_id,
            user_id,
            None,
            "raw turn dump — should be filtered out",
            &unit_embedding(9),
            None,
            None,
        )
        .await
        .unwrap();

        let (profile_groups, _relationship) = recall_memory_with_embedding(
            &pool,
            user_id,
            instance_id,
            &unit_embedding(7),
            true,
            true,
            TEXT_RECALL_TIER,
        )
        .await;

        // Categorised rows surfaced; raw row dropped because grouped path won.
        let labels: Vec<&str> = profile_groups.iter().map(|(l, _)| l.as_str()).collect();
        assert!(labels.contains(&"客观事实"));
        assert!(labels.contains(&"偏好"));
        assert!(!labels.contains(&"近况"));
        let all_contents: Vec<&String> = profile_groups
            .iter()
            .flat_map(|(_, items)| items.iter())
            .collect();
        assert!(all_contents
            .iter()
            .any(|s| s.as_str() == "lives in shanghai"));
        assert!(all_contents.iter().any(|s| s.as_str() == "loves coffee"));
        assert!(!all_contents.iter().any(|s| s.contains("raw turn dump")));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn recall_memory_with_embedding_respects_top_k(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, Some(instance_id)).await;
        let repo = MemoryRepo { pool: &pool };

        // Insert 6 profile rows (K=4) and 5 relationship rows (K=3) with
        // distinct embeddings so cosine ordering is well-defined.
        for i in 0..6 {
            repo.upsert(
                MemoryLayer::Profile,
                session_id,
                user_id,
                None,
                &format!("profile-{i}"),
                &unit_embedding(100 + i),
                None,
                None,
            )
            .await
            .unwrap();
        }
        for i in 0..5 {
            repo.upsert(
                MemoryLayer::Relationship,
                session_id,
                user_id,
                Some(instance_id),
                &format!("relationship-{i}"),
                &unit_embedding(200 + i),
                None,
                None,
            )
            .await
            .unwrap();
        }

        let (profile_groups, relationship) = recall_memory_with_embedding(
            &pool,
            user_id,
            instance_id,
            &unit_embedding(100),
            true,
            true,
            TEXT_RECALL_TIER,
        )
        .await;

        // No categorised rows exist → raw fallback fires under "近况"
        // with PROFILE_RECALL_K entries from the cosine top-K.
        assert_eq!(profile_groups.len(), 1);
        assert_eq!(profile_groups[0].0, "近况");
        assert_eq!(profile_groups[0].1.len(), PROFILE_RECALL_K as usize);
        assert_eq!(relationship.len(), RELATIONSHIP_RECALL_K as usize);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn recall_memory_with_embedding_picks_nearest_per_layer(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, Some(instance_id)).await;
        let repo = MemoryRepo { pool: &pool };

        // Profile-layer target at seed 42, with two decoys.
        repo.upsert(
            MemoryLayer::Profile,
            session_id,
            user_id,
            None,
            "profile target",
            &unit_embedding(42),
            None,
            None,
        )
        .await
        .unwrap();
        for i in 0..2 {
            repo.upsert(
                MemoryLayer::Profile,
                session_id,
                user_id,
                None,
                &format!("profile decoy-{i}"),
                &unit_embedding(300 + i),
                None,
                None,
            )
            .await
            .unwrap();
        }

        // Relationship-layer target at seed 99, with one decoy.
        repo.upsert(
            MemoryLayer::Relationship,
            session_id,
            user_id,
            Some(instance_id),
            "relationship target",
            &unit_embedding(99),
            None,
            None,
        )
        .await
        .unwrap();
        repo.upsert(
            MemoryLayer::Relationship,
            session_id,
            user_id,
            Some(instance_id),
            "relationship decoy",
            &unit_embedding(400),
            None,
            None,
        )
        .await
        .unwrap();

        // Query embedding hits the profile target seed exactly. All rows
        // here are uncategorised, so the raw fallback fires under "近况"
        // and its first item is the cosine-nearest one.
        let (profile_groups, _relationship) = recall_memory_with_embedding(
            &pool,
            user_id,
            instance_id,
            &unit_embedding(42),
            true,
            true,
            TEXT_RECALL_TIER,
        )
        .await;
        assert_eq!(profile_groups.len(), 1);
        assert_eq!(profile_groups[0].0, "近况");
        assert_eq!(
            profile_groups[0].1.first().map(String::as_str),
            Some("profile target"),
        );

        // Query at the relationship target seed.
        let (_profile2, relationship2) = recall_memory_with_embedding(
            &pool,
            user_id,
            instance_id,
            &unit_embedding(99),
            true,
            true,
            TEXT_RECALL_TIER,
        )
        .await;
        assert_eq!(
            relationship2.first().map(String::as_str),
            Some("relationship target"),
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn recall_gating_skips_layers_per_flags(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, Some(instance_id)).await;
        let repo = MemoryRepo { pool: &pool };
        repo.upsert(
            MemoryLayer::Profile,
            session_id,
            user_id,
            None,
            "profile fact",
            &unit_embedding(11),
            None,
            None,
        )
        .await
        .unwrap();
        repo.upsert(
            MemoryLayer::Relationship,
            session_id,
            user_id,
            Some(instance_id),
            "relationship fact",
            &unit_embedding(11),
            None,
            None,
        )
        .await
        .unwrap();

        // relationship_only: x off, y on → profile groups empty, relationship present
        let (prof, rel) = recall_memory_with_embedding(
            &pool,
            user_id,
            instance_id,
            &unit_embedding(11),
            false,
            true,
            TEXT_RECALL_TIER,
        )
        .await;
        assert!(prof.is_empty(), "profile groups must be empty when X off");
        assert_eq!(rel, vec!["relationship fact".to_string()]);

        // both off → nothing
        let (prof2, rel2) = recall_memory_with_embedding(
            &pool,
            user_id,
            instance_id,
            &unit_embedding(11),
            false,
            false,
            TEXT_RECALL_TIER,
        )
        .await;
        assert!(prof2.is_empty() && rel2.is_empty());

        // both on → both layers (sanity that the hot path still works)
        let (prof3, rel3) = recall_memory_with_embedding(
            &pool,
            user_id,
            instance_id,
            &unit_embedding(11),
            true,
            true,
            TEXT_RECALL_TIER,
        )
        .await;
        assert!(
            !prof3.is_empty(),
            "profile groups should be present when X on"
        );
        assert!(!rel3.is_empty(), "relationship should be present when Y on");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn recall_memory_with_embedding_voice_tier_respects_k(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, Some(instance_id)).await;
        let repo = MemoryRepo { pool: &pool };

        // Two categorised profile rows in each of two categories — more than
        // the voice tier's grouped_k=1, so the cap must actually bite.
        for (i, content) in ["lives in shanghai", "works remote"].iter().enumerate() {
            repo.upsert(
                MemoryLayer::Profile,
                session_id,
                user_id,
                None,
                content,
                &unit_embedding(500 + i),
                Some("fact"),
                None,
            )
            .await
            .unwrap();
        }
        for (i, content) in ["loves coffee", "loves tea"].iter().enumerate() {
            repo.upsert(
                MemoryLayer::Profile,
                session_id,
                user_id,
                None,
                content,
                &unit_embedding(510 + i),
                Some("preference"),
                None,
            )
            .await
            .unwrap();
        }

        // Three relationship rows — more than the voice tier's
        // relationship_k=2, so the cap must actually bite.
        for i in 0..3 {
            repo.upsert(
                MemoryLayer::Relationship,
                session_id,
                user_id,
                Some(instance_id),
                &format!("relationship-{i}"),
                &unit_embedding(600 + i),
                None,
                None,
            )
            .await
            .unwrap();
        }

        let voice_tier = crate::pipeline::voice::VOICE_RECALL_TIER;

        let (profile_groups, relationship) = recall_memory_with_embedding(
            &pool,
            user_id,
            instance_id,
            &unit_embedding(500),
            true,
            true,
            voice_tier,
        )
        .await;

        // grouped_k=1 ⇒ each of the two categories is capped to a single
        // bullet even though 2 rows exist per category.
        assert_eq!(profile_groups.len(), 2, "both categories present");
        for (label, items) in &profile_groups {
            assert_eq!(
                items.len(),
                1,
                "grouped_k=1 must cap category {label:?} to 1 row"
            );
        }

        // relationship_k=2 ⇒ capped below the 3 rows seeded.
        assert_eq!(relationship.len(), 2);
    }

    /// `recall_memory_with_embedding_voice_tier_respects_k` above seeds only
    /// categorised profile rows, so `build_profile_groups` always takes the
    /// grouped branch and the raw-fallback `raw_res` — though computed with
    /// `tier.raw_k` — is discarded unread. That test alone never observes
    /// whether `raw_k` is actually threaded through. Grouped and raw are
    /// mutually-exclusive fallback paths (`build_profile_groups` returns the
    /// grouped rows whenever any exist), so a single seeded scenario can't
    /// exercise both; this sibling test seeds *only* uncategorised rows to
    /// force the raw-fallback branch and assert its cap directly.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn recall_memory_with_embedding_voice_tier_raw_fallback_cap(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id = make_session(&pool, user_id, Some(instance_id)).await;
        let repo = MemoryRepo { pool: &pool };

        // Three uncategorised profile rows, zero categorised — forces the
        // "近况" raw-fallback branch, more than the voice tier's raw_k=2.
        for i in 0..3 {
            repo.upsert(
                MemoryLayer::Profile,
                session_id,
                user_id,
                None,
                &format!("raw-fact-{i}"),
                &unit_embedding(700 + i),
                None,
                None,
            )
            .await
            .unwrap();
        }

        let voice_tier = crate::pipeline::voice::VOICE_RECALL_TIER;

        let (profile_groups, _relationship) = recall_memory_with_embedding(
            &pool,
            user_id,
            instance_id,
            &unit_embedding(700),
            true,
            true,
            voice_tier,
        )
        .await;

        // No categorised rows exist ⇒ raw fallback fires under "近况",
        // capped to raw_k=2 even though 3 rows were seeded.
        assert_eq!(profile_groups.len(), 1);
        assert_eq!(profile_groups[0].0, "近况");
        assert_eq!(
            profile_groups[0].1.len(),
            2,
            "raw_k=2 must cap the 近况 fallback group to 2 rows"
        );
    }

    // ─── human_insights_to_bullets ──────────────────────────────────────

    fn sample_human_row() -> HumanInsightsRow {
        HumanInsightsRow {
            user_id: Uuid::new_v4(),
            city: Some("上海".into()),
            location: None,
            hometown: None,
            nationality: None,
            occupation: Some("设计师".into()),
            mbti_guess: Some("INFP".into()),
            love_values: Some("慢热".into()),
            emotional_needs: Some("被理解".into()),
            life_rhythm: Some("夜猫子".into()),
            interests: vec!["登山".into(), "摄影".into()],
            personality_traits: vec!["温柔".into()],
            preferred_gender: Some("female".into()),
            age_min: Some(25),
            age_max: Some(35),
            deal_breakers: vec!["抽烟".into()],
            education: Some("美院本科".into()),
            family: Some("独生女，父母在杭州".into()),
            relationship_history: Some("单身两年".into()),
            social_pattern: Some("小圈子聚会".into()),
            future_plans: Some("想开工作室".into()),
            finance_status: Some("攒钱中".into()),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn human_insights_full_renders_all_fields_in_order() {
        let bullets = human_insights_to_bullets(&sample_human_row(), InsightMode::Full);
        assert_eq!(
            bullets,
            vec![
                "城市：上海",
                "职业：设计师",
                "教育：美院本科",
                "MBTI：INFP",
                "感情观：慢热",
                "感情经历：单身两年",
                "兴趣：登山、摄影",
                "情感需求：被理解",
                "家庭：独生女，父母在杭州",
                "经济状况：攒钱中",
                "作息：夜猫子",
                "社交模式：小圈子聚会",
                "性格特质：温柔",
                "未来计划：想开工作室",
            ]
        );
    }

    #[test]
    fn human_insights_neutral_drops_intimate_fields() {
        let bullets = human_insights_to_bullets(&sample_human_row(), InsightMode::Neutral);
        assert_eq!(
            bullets,
            vec![
                "城市：上海",
                "职业：设计师",
                "教育：美院本科",
                "MBTI：INFP",
                "作息：夜猫子",
                "社交模式：小圈子聚会",
                "性格特质：温柔",
                "未来计划：想开工作室",
            ]
        );
        // Intimate additions (感情经历/家庭/经济状况) join love_values/interests/
        // emotional_needs in the Full-only cluster; matching-only columns are
        // never rendered in any mode — proven by the exact vec above.
    }

    fn sample_geo_row() -> HumanInsightsRow {
        HumanInsightsRow {
            user_id: Uuid::new_v4(),
            city: Some("深圳".into()),
            location: Some("台北".into()),
            hometown: Some("新界".into()),
            nationality: Some("中国香港".into()),
            occupation: None,
            mbti_guess: None,
            love_values: None,
            emotional_needs: None,
            life_rhythm: None,
            interests: vec![],
            personality_traits: vec![],
            preferred_gender: None,
            age_min: None,
            age_max: None,
            deal_breakers: vec![],
            education: None,
            family: None,
            relationship_history: None,
            social_pattern: None,
            future_plans: None,
            finance_status: None,
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn human_insights_renders_geo_cluster_in_both_modes() {
        for mode in [InsightMode::Full, InsightMode::Neutral] {
            let bullets = human_insights_to_bullets(&sample_geo_row(), mode);
            assert_eq!(
                bullets,
                vec!["城市：深圳", "所在地：台北", "老家：新界", "国籍：中国香港"]
            );
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn load_human_insight_bullets_returns_empty_for_unknown_user(pool: PgPool) {
        let bullets = load_human_insight_bullets(&pool, Uuid::new_v4(), InsightMode::Full).await;
        assert!(bullets.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn load_human_insight_bullets_neutral_vs_full(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let insights = serde_json::json!({
            "city": "北京", "occupation": "工程师",
            "love_values": "认真", "interests": ["爬山"], "emotional_needs": "陪伴"
        });
        HumanInsightRepo { pool: &pool }
            .apply_extraction(user_id, &insights)
            .await
            .unwrap();

        let full = load_human_insight_bullets(&pool, user_id, InsightMode::Full).await;
        assert!(full.iter().any(|b| b == "感情观：认真"));
        assert!(full.iter().any(|b| b == "兴趣：爬山"));

        let neutral = load_human_insight_bullets(&pool, user_id, InsightMode::Neutral).await;
        assert!(neutral.iter().any(|b| b == "城市：北京"));
        assert!(neutral
            .iter()
            .all(|b| !b.contains("认真") && !b.contains("爬山") && !b.contains("陪伴")));

        let off = load_human_insight_bullets(&pool, user_id, InsightMode::Off).await;
        assert!(off.is_empty());
    }

    // ─── audit_from_event ───────────────────────────────────────────────

    #[test]
    fn extract_audit_from_user_message() {
        let mut metadata = serde_json::Map::new();
        metadata.insert("feature".into(), serde_json::Value::String("chat".into()));
        let audit = LlmAudit {
            user: Some("u_abc".into()),
            session_id: Some("conv_xyz".into()),
            metadata: Some(metadata.clone()),
        };
        let ev = Event::UserMessage {
            content: "hi".into(),
            message_id: Uuid::new_v4(),
            prompt_traits: vec![],
            audit: Some(audit.clone()),
            tier: None,
            memory_scope: Default::default(),
            affinity_scope: Default::default(),
            tips_amount_usd: None,
            quote: Default::default(),
        };
        let extracted = audit_from_event(&ev);
        assert_eq!(extracted, Some(&audit));
    }

    #[test]
    fn extract_audit_from_non_user_message_is_none() {
        let ev = Event::ProactiveTrigger;
        assert!(audit_from_event(&ev).is_none());
    }

    fn pt(tag: &str) -> PromptTrait {
        PromptTrait {
            tag: tag.into(),
            text: "x".into(),
        }
    }

    #[test]
    fn filter_traits_none_keeps_all() {
        let traits = vec![pt("allow_nsfw"), pt("allow_politics")];
        let (kept, dropped) = filter_traits(&traits, None);
        assert_eq!(kept.len(), 2);
        assert!(dropped.is_empty());
    }

    #[test]
    fn filter_traits_whitelist_drops_outside() {
        let traits = vec![pt("allow_politics"), pt("allow_nsfw")];
        let allow = vec!["allow_politics".to_string()];
        let (kept, dropped) = filter_traits(&traits, Some(&allow));
        assert_eq!(
            kept.iter().map(|t| t.tag.as_str()).collect::<Vec<_>>(),
            vec!["allow_politics"]
        );
        assert_eq!(dropped, vec!["allow_nsfw".to_string()]);
    }

    #[test]
    fn filter_traits_empty_whitelist_drops_all() {
        let traits = vec![pt("allow_politics"), pt("allow_nsfw")];
        let allow: Vec<String> = vec![];
        let (kept, dropped) = filter_traits(&traits, Some(&allow));
        assert!(kept.is_empty());
        assert_eq!(dropped.len(), 2);
    }

    #[test]
    fn filter_traits_whitelist_keeps_all_when_all_allowed() {
        let traits = vec![pt("allow_politics"), pt("allow_nsfw")];
        let allow = vec!["allow_nsfw".to_string(), "allow_politics".to_string()];
        let (kept, dropped) = filter_traits(&traits, Some(&allow));
        assert_eq!(kept.len(), 2);
        assert!(dropped.is_empty());
    }

    fn user_row(content: &str, pre: Option<&str>) -> eros_engine_store::chat::ChatMessage {
        eros_engine_store::chat::ChatMessage {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::new_v4(),
            role: "user".into(),
            content: content.into(),
            sent_at: chrono::Utc::now(),
            client_msg_id: None,
            ghost_decision: false,
            user_message_id: None,
            continues_from_message_id: None,
            truncated: false,
            generation_id: None,
            assistant_action_type: None,
            channel: None,
            pre_filter_content: pre.map(|s| s.to_string()),
            metadata: None,
            read_at: None,
        }
    }

    #[test]
    fn effective_user_text_prefers_nonblank_rewrite() {
        let mut row = user_row("1111", None);
        assert_eq!(effective_user_text(&row), "1111");
        row.pre_filter_content = Some("有意义的问题".into());
        assert_eq!(effective_user_text(&row), "有意义的问题");
        row.pre_filter_content = Some("   ".into()); // blank → fall back to content
        assert_eq!(effective_user_text(&row), "1111");
    }

    fn user_row_meta(
        content: &str,
        metadata: serde_json::Value,
    ) -> eros_engine_store::chat::ChatMessage {
        let mut r = user_row(content, None);
        r.metadata = Some(metadata);
        r
    }

    #[test]
    fn model_facing_text_folds_vision_preamble() {
        let row = user_row_meta(
            "看看这个",
            serde_json::json!({
                "image_url": "https://x/y.png",
                "vision": { "description": "一只猫", "ocr_text": "", "people": "", "scene": "客厅" }
            }),
        );
        let t = model_facing_user_text(&row);
        assert!(t.contains("[用户发送了一张图片]"));
        assert!(t.contains("画面：一只猫"));
        assert!(t.contains("场景：客厅"));
        assert!(!t.contains("文字：")); // blank ocr dropped
        assert!(t.ends_with("看看这个"));
    }

    #[test]
    fn model_facing_text_image_only_uses_placeholder_body() {
        let row = user_row_meta(
            "",
            serde_json::json!({ "image_url": "https://x/y.png", "vision": { "description": "日落" } }),
        );
        let t = model_facing_user_text(&row);
        assert!(t.contains("画面：日落"));
        assert!(t.ends_with("[用户未附文字]"));
    }

    #[test]
    fn model_facing_text_undescribed_image_placeholder() {
        let row = user_row_meta("hi", serde_json::json!({ "image_url": "https://x/y.png" }));
        let t = model_facing_user_text(&row);
        assert!(t.contains("无法识别"));
        assert!(t.ends_with("hi"));
    }

    #[test]
    fn model_facing_text_plain_turn_unchanged() {
        let row = user_row("普通消息", None);
        assert_eq!(model_facing_user_text(&row), "普通消息");
    }

    #[test]
    fn assemble_includes_all_gift_user_rows() {
        use eros_engine_llm::model_config::ResolvedModel;

        // gift_user is tip-only now — all gift_user rows are promoted to the
        // "user" role. The legacy in-app Gift Event endpoint was removed, so
        // there is no longer a legacy row type to gate out.
        let mut tip = user_row("(打赏 $5)", None);
        tip.role = "gift_user".into();
        tip.metadata = Some(serde_json::json!({ "tips_amount_usd": 5.0 }));
        let plain = user_row("普通消息", None);
        let mut assistant = user_row("回复", None);
        assistant.role = "assistant".into();

        let resolved = ResolvedModel {
            model: "m".into(),
            fallback_model: vec![],
            temperature: 0.7,
            sampling: eros_engine_llm::model_config::Sampling {
                top_p: Some(0.9),
                frequency_penalty: Some(0.4),
                presence_penalty: Some(0.2),
                repetition_penalty: Some(1.15),
            },
            max_tokens: 100,
            allow_traits: None,
            reasoning: None,
            retry_depth: 0,
        };
        let req = assemble_chat_request(
            resolved,
            "SYS".into(),
            model_facing_history(vec![tip, plain, assistant]),
            None,
        );

        // Sampling knobs flow from ResolvedModel onto the ChatRequest.
        assert_eq!(req.sampling.top_p, Some(0.9));
        assert_eq!(req.sampling.frequency_penalty, Some(0.4));
        assert_eq!(req.sampling.presence_penalty, Some(0.2));
        assert_eq!(req.sampling.repetition_penalty, Some(1.15));

        let user_contents: Vec<&str> = req
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            user_contents.contains(&"(打赏 $5)"),
            "gift_user must be promoted under the user role: {user_contents:?}"
        );
        assert!(user_contents.contains(&"普通消息"));
        // No `gift_user` role ever reaches the wire (OpenRouter knows only
        // system/user/assistant).
        assert!(req
            .messages
            .iter()
            .all(|m| matches!(m.role.as_str(), "system" | "user" | "assistant")));
    }

    /// `channel`-marked rows (voice / product_qa) are out of companion context —
    /// they must never re-enter the model's own conversation history, mirroring
    /// the isolation invariant already enforced for signals/dreaming/judge.
    #[test]
    fn assemble_excludes_channel_marked_rows() {
        use eros_engine_llm::model_config::ResolvedModel;

        let mut product_qa_user = user_row("这个产品是什么", None);
        product_qa_user.channel = Some("product_qa".into());
        let mut product_qa_assistant = user_row("这是官方说明", None);
        product_qa_assistant.role = "assistant".into();
        product_qa_assistant.channel = Some("product_qa".into());
        let plain = user_row("普通消息", None);

        let resolved = ResolvedModel {
            model: "m".into(),
            fallback_model: vec![],
            temperature: 0.7,
            sampling: Default::default(),
            max_tokens: 100,
            allow_traits: None,
            reasoning: None,
            retry_depth: 0,
        };
        let req = assemble_chat_request(
            resolved,
            "SYS".into(),
            model_facing_history(vec![product_qa_user, product_qa_assistant, plain]),
            None,
        );

        let contents: Vec<&str> = req.messages.iter().map(|m| m.content.as_str()).collect();
        assert!(
            !contents.contains(&"这个产品是什么"),
            "channel-marked user row must not enter the model's messages: {contents:?}"
        );
        assert!(
            !contents.contains(&"这是官方说明"),
            "channel-marked assistant row must not enter the model's messages: {contents:?}"
        );
        assert!(contents.contains(&"普通消息"));
        // system prompt + the one surviving (unmarked) turn only.
        assert_eq!(req.messages.len(), 2);
    }

    #[test]
    fn recall_query_prefers_caption() {
        let row = user_row_meta(
            "你看这个",
            serde_json::json!({ "vision": { "description": "一只猫" } }),
        );
        assert_eq!(recall_query_text(&row), "你看这个");
    }

    #[test]
    fn recall_query_falls_back_to_description_when_caption_blank() {
        let row = user_row_meta(
            "",
            serde_json::json!({ "image_url": "https://x/y.png", "vision": { "description": "一只猫在沙滩" } }),
        );
        assert_eq!(recall_query_text(&row), "一只猫在沙滩");
    }

    #[test]
    fn recall_query_empty_when_no_caption_no_vision() {
        let row = user_row("", None);
        assert_eq!(recall_query_text(&row), "");
    }

    #[test]
    fn recall_query_plain_text_turn() {
        let row = user_row("普通消息", None);
        assert_eq!(recall_query_text(&row), "普通消息");
    }

    // ─── compose_image_prompt ───────────────────────────────────────────

    /// Build a `CompanionPersona` with arbitrary `art_metadata` key-value pairs,
    /// matching the construction pattern from `pde_test_persona` in stream.rs.
    fn test_persona_with_meta(
        pairs: &[(&str, &str)],
    ) -> eros_engine_core::persona::CompanionPersona {
        use eros_engine_core::persona::{CompanionPersona, PersonaGenome, PersonaInstance};
        let iid = uuid::Uuid::new_v4();
        let gid = uuid::Uuid::new_v4();
        let oid = uuid::Uuid::new_v4();
        let meta: serde_json::Map<String, serde_json::Value> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect();
        CompanionPersona {
            instance_id: iid,
            genome: PersonaGenome {
                id: gid,
                name: "TestPersona".into(),
                system_prompt: "You are TestPersona.".into(),
                tip_personality: None,
                art_metadata: serde_json::Value::Object(meta),
            },
            instance: PersonaInstance {
                id: iid,
                genome_id: gid,
                owner_uid: oid,
                status: "active".into(),
            },
        }
    }

    #[test]
    fn compose_image_prompt_layers_style_appearance_subject() {
        let persona = test_persona_with_meta(&[("appearance", "auburn hair, green eyes")]);
        let out = compose_image_prompt(StyleKey::Anime, &persona, "smiling in a cafe");
        assert!(out.starts_with("High-quality Japanese anime"));
        assert!(out.contains("auburn hair, green eyes"));
        assert!(out.contains("smiling in a cafe"));
    }

    #[test]
    fn compose_image_prompt_omits_absent_appearance() {
        let persona = test_persona_with_meta(&[]);
        let out = compose_image_prompt(StyleKey::Realistic, &persona, "a cat");
        assert!(out.starts_with("Photorealistic"));
        assert!(out.contains("a cat"));
    }

    // ─── model_facing_assistant_text / history fold ──────────────────────

    fn assistant_row(
        content: &str,
        metadata: Option<serde_json::Value>,
    ) -> eros_engine_store::chat::ChatMessage {
        eros_engine_store::chat::ChatMessage {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::new_v4(),
            role: "assistant".into(),
            content: content.into(),
            sent_at: chrono::Utc::now(),
            client_msg_id: None,
            ghost_decision: false,
            user_message_id: None,
            continues_from_message_id: None,
            truncated: false,
            generation_id: None,
            assistant_action_type: None,
            channel: None,
            pre_filter_content: None,
            metadata,
            read_at: None,
        }
    }

    #[test]
    fn assistant_row_with_image_caption_appends_marker() {
        let row = assistant_row(
            "这是我的回复",
            Some(serde_json::json!({ "image": {
                "prompt": "Photorealistic, ultra-detailed, smiling in a cafe",
                "caption": "在咖啡店笑着"
            }})),
        );
        let out = model_facing_assistant_text(&row);
        assert!(out.contains("这是我的回复"));
        assert!(out.contains("[你的照片：在咖啡店笑着]"));
        assert!(
            !out.contains("Photorealistic"),
            "the image prompt must never reach the chat model's history: {out}"
        );
        assert!(out.contains("这是我的回复\n\n[你的照片：在咖啡店笑着]"));
    }

    #[test]
    fn assistant_row_without_caption_uses_bare_marker() {
        let row = assistant_row(
            "",
            Some(serde_json::json!({ "image": { "prompt": "a long english image prompt" } })),
        );
        let out = model_facing_assistant_text(&row);
        assert_eq!(out, "[你的照片]");
    }

    #[test]
    fn assistant_row_without_image_metadata_unchanged() {
        let row = assistant_row("普通回复", None);
        assert_eq!(model_facing_assistant_text(&row), "普通回复");
    }

    /// Even when `image` carries neither `prompt` nor `caption` (e.g. an older
    /// row shape, or a `url`-only marker), the mere presence of the `image`
    /// key means a picture was sent — the bare marker still appends. This
    /// mirrors `assistant_transcript_line`'s guard (keyed on `image` presence,
    /// not on any particular field inside it).
    #[test]
    fn assistant_row_image_metadata_without_caption_or_prompt_appends_bare_marker() {
        let row = assistant_row(
            "普通回复",
            Some(serde_json::json!({ "image": { "url": "https://x/y.png" } })),
        );
        assert_eq!(model_facing_assistant_text(&row), "普通回复\n\n[你的照片]");
    }

    #[test]
    fn model_facing_history_keeps_distinct_photos_distinct() {
        // Same prose, different photo captions ⇒ different injected strings.
        // Echo cancellation keys on this string (design §4.2), so collapsing
        // these two would delete a photo the persona actually sent.
        let mut a = user_row("给你看~", None);
        a.role = "assistant".into();
        a.metadata = Some(serde_json::json!({ "image": { "caption": "海边的黄昏" } }));
        let mut b = user_row("给你看~", None);
        b.role = "assistant".into();
        b.metadata = Some(serde_json::json!({ "image": { "caption": "厨房里的猫" } }));

        let out = model_facing_history(vec![a, b]);
        assert_eq!(out.len(), 2);
        assert_ne!(
            out[0].text, out[1].text,
            "different captions must not collapse to one injected string"
        );
        assert!(out[0].text.contains("海边的黄昏"));
        assert!(out[1].text.contains("厨房里的猫"));
        assert_eq!(out[0].role, "assistant");
    }

    #[test]
    fn model_facing_history_drops_channel_rows_and_folds_gift_user() {
        // Same contract assemble_chat_request had before the split: channel-
        // marked rows never reach the model, gift_user is promoted to "user".
        let mut qa = user_row("这个产品是什么", None);
        qa.channel = Some("product_qa".into());
        let mut tip = user_row("(打赏 $5)", None);
        tip.role = "gift_user".into();
        let plain = user_row("普通消息", None);

        let out = model_facing_history(vec![qa, tip, plain]);
        let texts: Vec<&str> = out.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["(打赏 $5)", "普通消息"]);
        assert!(out.iter().all(|m| m.role == "user"));
    }

    #[test]
    fn apply_echo_cancellation_respects_the_operator_flag() {
        let dup = || crate::repetition::Injected {
            id: uuid::Uuid::new_v4(),
            role: "assistant".into(),
            text: "同一句话".into(),
        };
        let a = dup();
        let b = dup();
        // The exemption id is row `b`'s — a real row in the input, so the
        // assertions below go red the moment the exemption stops working. What
        // that pins is THIS function's parameter semantics: feed `session_id`
        // to `cancel_echo` instead of `current_id` and `b` no longer survives.
        // It does not pin the production call site — `build_reply_request` has
        // no test coverage, so a swap made only there is caught by review.
        let current = b.id;
        let session = uuid::Uuid::new_v4();
        let input = vec![a, b];

        // disabled=true ⇒ byte-for-byte what we had before this change.
        let untouched = apply_echo_cancellation(input.clone(), current, true, session);
        assert_eq!(untouched, input);

        // disabled=false ⇒ the duplicate group collapses to the exempt row only.
        let cancelled = apply_echo_cancellation(input, current, false, session);
        assert_eq!(
            cancelled.len(),
            1,
            "only the current turn survives: {cancelled:?}"
        );
        assert_eq!(cancelled[0].id, current);
    }

    #[test]
    fn distinct_photos_both_survive_cancellation() {
        // Same prose, different captions ⇒ different keys ⇒ neither is a
        // duplicate. A key built from `content` instead of the model-facing
        // string would delete one of the two photos.
        let mut a = user_row("给你看~", None);
        a.role = "assistant".into();
        a.metadata = Some(serde_json::json!({ "image": { "caption": "海边的黄昏" } }));
        let mut b = user_row("给你看~", None);
        b.role = "assistant".into();
        b.metadata = Some(serde_json::json!({ "image": { "caption": "厨房里的猫" } }));
        let current = user_row("嗯", None);
        let current_id = current.id;

        let kept = apply_echo_cancellation(
            model_facing_history(vec![a, b, current]),
            current_id,
            false,
            uuid::Uuid::new_v4(),
        );
        assert_eq!(kept.len(), 3, "no row may be dropped: {kept:?}");
    }

    // ─── model_facing_history leading-sentence strip (spec §4.3) ─────────

    fn chat_row(role: &str, content: &str) -> eros_engine_store::chat::ChatMessage {
        eros_engine_store::chat::ChatMessage {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::new_v4(),
            role: role.to_string(),
            content: content.to_string(),
            sent_at: chrono::Utc::now(),
            client_msg_id: None,
            ghost_decision: false,
            user_message_id: None,
            continues_from_message_id: None,
            truncated: false,
            generation_id: None,
            assistant_action_type: None,
            channel: None,
            pre_filter_content: None,
            metadata: None,
            read_at: None,
        }
    }

    #[test]
    fn injected_assistant_rows_lose_their_leading_sentence() {
        let rows = vec![
            chat_row("user", "在吗"),
            chat_row("assistant", "唔。我在呢，刚洗完澡。"),
        ];
        let out = model_facing_history(rows);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "在吗", "user rows are untouched");
        assert_eq!(out[1].text, "我在呢，刚洗完澡。");
    }

    #[test]
    fn assistant_rows_that_strip_to_empty_are_dropped() {
        let rows = vec![
            chat_row("user", "在吗"),
            chat_row("assistant", "唔。"),
            chat_row("user", "怎么了"),
        ];
        let out = model_facing_history(rows);
        let texts: Vec<&str> = out.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["在吗", "怎么了"]);
    }

    #[test]
    fn no_injected_row_is_ever_empty() {
        // The invariant assemble_chat_request depends on: an empty `content`
        // is rejected by some providers.
        let rows = vec![
            chat_row("assistant", "唔。"),
            chat_row("assistant", "   "),
            chat_row("assistant", "。。。"),
            chat_row("user", "hi"),
        ];
        for m in model_facing_history(rows) {
            assert!(
                !m.text.trim().is_empty(),
                "empty injected row: role={}",
                m.role
            );
        }
    }

    #[test]
    fn user_rows_are_never_stripped_even_when_single_sentence() {
        let rows = vec![chat_row("user", "在吗？")];
        let out = model_facing_history(rows);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "在吗？");
    }

    // ─── fetch_world_context ────────────────────────────────────────────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn fetch_world_context_gates_on_enrollment_and_flags(pool: sqlx::PgPool) {
        let owner = uuid::Uuid::new_v4();
        let genome_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('H','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1,$2) RETURNING id",
        )
        .bind(genome_id)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        // test_state defaults world_configured=false (pins the unconfigured-
        // deployment gate, see below); flip it on so this test exercises the
        // enrollment/flag gating instead.
        state.world_configured = true;

        // No enrollment, no state ⇒ None.
        assert!(fetch_world_context(&state, owner, instance, None)
            .await
            .is_none());

        // Enrolled + digest present ⇒ Some, digest-only without an embedding.
        sqlx::query("INSERT INTO engine.world_enrollments (owner_uid) VALUES ($1)")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests) \
             VALUES ($1, '{}'::jsonb, $2)",
        )
        .bind(owner)
        .bind(serde_json::json!({ instance.to_string(): "小圈子近况" }))
        .execute(&pool)
        .await
        .unwrap();
        let ctx = fetch_world_context(&state, owner, instance, None)
            .await
            .expect("enrolled with digest ⇒ Some");
        assert_eq!(ctx.digest, "小圈子近况");
        assert!(ctx.fragments.is_empty(), "no embedding ⇒ digest-only");

        // world_configured=false (no [tasks.world_director] section) ⇒ None
        // even when enrolled+digest present — the unconfigured-deployment gate.
        let mut unconfigured = state.clone();
        unconfigured.world_configured = false;
        assert!(fetch_world_context(&unconfigured, owner, instance, None)
            .await
            .is_none());

        // WORLD_PROMPT_DISABLED ⇒ None even when data exists.
        let mut muted = state.clone();
        muted.config.world.prompt_disabled = true;
        assert!(fetch_world_context(&muted, owner, instance, None)
            .await
            .is_none());

        // WORLD_DISABLED ⇒ None.
        let mut off = state.clone();
        off.config.world.disabled = true;
        assert!(fetch_world_context(&off, owner, instance, None)
            .await
            .is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn fetch_world_context_recalls_fragments_with_embedding(pool: sqlx::PgPool) {
        let owner = uuid::Uuid::new_v4();
        let genome_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('F','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1,$2) RETURNING id",
        )
        .bind(genome_id)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO engine.world_enrollments (owner_uid) VALUES ($1)")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO engine.world_worldviews (owner_uid, content) VALUES ($1, '现代都市')",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests) \
             VALUES ($1, '{}'::jsonb, $2)",
        )
        .bind(owner)
        .bind(serde_json::json!({ instance.to_string(): "d" }))
        .execute(&pool)
        .await
        .unwrap();

        let mut emb = vec![0.0_f32; 512];
        emb[42] = 1.0;
        let repo = eros_engine_store::world::WorldRepo { pool: &pool };
        // Claim first so persist_round's ownership-token guard has a real
        // claimed_at to match (the row above was inserted directly, so
        // claimed_at is still NULL).
        let claimed = repo
            .claim_due(
                std::time::Duration::from_secs(24 * 3600),
                std::time::Duration::from_secs(1800),
                5,
            )
            .await
            .unwrap();
        let (_o, token) = claimed[0];
        let wv_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "SELECT updated_at FROM engine.world_worldviews WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        repo.persist_round(
            owner,
            &serde_json::json!({}),
            &serde_json::json!({ instance.to_string(): "d" }),
            &[eros_engine_store::world::FragmentInsert {
                instance_id: instance,
                content: "剧本片段A".into(),
                embedding: emb.clone(),
            }],
            &[],
            chrono::Utc::now().date_naive(),
            30,
            "h",
            false,
            wv_at,
            token,
        )
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.world_configured = true;
        let ctx = fetch_world_context(&state, owner, instance, Some(&emb))
            .await
            .expect("Some");
        assert_eq!(ctx.fragments, vec!["剧本片段A".to_string()]);
    }

    // ─── fetch_stories_context ───────────────────────────────────────────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn fetch_stories_context_gates_on_flag_and_config(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('S','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1,$2) RETURNING id",
        )
        .bind(genome_id)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO engine.world_enrollments (owner_uid, stories_enabled) VALUES ($1, true)",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.persona_story_insights (instance_id, owner_uid, digest) \
             VALUES ($1, $2, '近况')",
        )
        .bind(instance)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.stories_configured = true;
        state.world_configured = true;
        let ctx = fetch_stories_context(&state, owner, instance, None)
            .await
            .expect("some");
        assert_eq!(ctx.digest, "近况");
        assert!(ctx.episodes.is_empty(), "no embedding ⇒ digest-only");

        // Gating matrix ⇒ None:
        let mut unconfigured = crate::routes::companion::test_state(pool.clone());
        unconfigured.stories_configured = false;
        unconfigured.world_configured = true;
        assert!(fetch_stories_context(&unconfigured, owner, instance, None)
            .await
            .is_none());

        let mut off = crate::routes::companion::test_state(pool.clone());
        off.stories_configured = true;
        off.world_configured = true;
        off.config.world.stories_disabled = true;
        assert!(fetch_stories_context(&off, owner, instance, None)
            .await
            .is_none());

        let mut muted = crate::routes::companion::test_state(pool.clone());
        muted.stories_configured = true;
        muted.world_configured = true;
        muted.config.world.stories_prompt_disabled = true;
        assert!(fetch_stories_context(&muted, owner, instance, None)
            .await
            .is_none());

        let mut world_off = crate::routes::companion::test_state(pool.clone());
        world_off.stories_configured = true;
        world_off.world_configured = true;
        world_off.config.world.disabled = true;
        assert!(fetch_stories_context(&world_off, owner, instance, None)
            .await
            .is_none());

        // stories_configured=true but world_configured=false ⇒ None: stories
        // injection requires the WM base, mirroring the sweeper's
        // [tasks.world_director] prerequisite.
        let mut wm_off = crate::routes::companion::test_state(pool.clone());
        wm_off.stories_configured = true;
        wm_off.world_configured = false;
        assert!(
            fetch_stories_context(&wm_off, owner, instance, None)
                .await
                .is_none(),
            "stories injection requires the world-memories base (world_configured)"
        );

        // stories_enabled=false ⇒ None (digest query's JOIN filters it).
        sqlx::query(
            "UPDATE engine.world_enrollments SET stories_enabled = false WHERE owner_uid = $1",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        assert!(fetch_stories_context(&state, owner, instance, None)
            .await
            .is_none());
    }
}

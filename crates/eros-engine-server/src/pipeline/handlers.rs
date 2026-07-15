// SPDX-License-Identifier: AGPL-3.0-only
//! Chat request builders — assemble an `eros_engine_llm::openrouter::ChatRequest`
//! for the streaming pipeline based on the PDE's `ActionPlan`.
//!
//! OSS specifics: all DB I/O goes through `eros_engine_store` repos, and the
//! model / fallback / allow_traits are resolved via `state.model_config`
//! (task + per-request tier).

use sqlx::PgPool;
use uuid::Uuid;

#[cfg(test)]
use serde_json::Value;

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

/// Task key used by all chat handlers. Matches the gateway's task router.
const CHAT_TASK: &str = "chat_companion";

/// Maximum number of recent messages pulled into the prompt.
const HISTORY_WINDOW: i64 = 20;

/// Resolved fetch strategy for the main history of a turn. Pure mapping of the
/// event's `HistoryAnchor`, factored out so it can be unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryFetch {
    Latest,
    Anchored(Option<chrono::DateTime<chrono::Utc>>),
}

fn history_fetch_for(anchor: eros_engine_core::types::HistoryAnchor) -> HistoryFetch {
    use eros_engine_core::types::HistoryAnchor;
    match anchor {
        HistoryAnchor::Latest => HistoryFetch::Latest,
        HistoryAnchor::At { sent_at, .. } => HistoryFetch::Anchored(Some(sent_at)),
        HistoryAnchor::DropHistory => HistoryFetch::Anchored(None),
    }
}

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
/// the pre-OUTPUT-filter original); `assemble_chat_request` routes assistant
/// rows to `content` directly.
pub(crate) fn effective_user_text(msg: &eros_engine_store::chat::ChatMessage) -> &str {
    match msg.pre_filter_content.as_deref() {
        Some(s) if !s.trim().is_empty() => s,
        _ => &msg.content,
    }
}

/// Model-facing text for an assistant history row: the stored `content`, with a
/// `[你给对方发送了一张照片：{prompt}]` marker appended when `metadata.image.prompt`
/// is present. Used by `assemble_chat_request` so the model knows it previously
/// sent an image in that turn.
pub(crate) fn model_facing_assistant_text(msg: eros_engine_store::chat::ChatMessage) -> String {
    let mut text = msg.content;
    if let Some(prompt) = msg
        .metadata
        .as_ref()
        .and_then(|md| md.get("image"))
        .and_then(|img| img.get("prompt"))
        .and_then(|p| p.as_str())
    {
        let marker = format!("[你给对方发送了一张照片：{prompt}]");
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

/// Materialise a ChatRequest from a pre-resolved model + system prompt +
/// chronological history. `audit` carries the caller's OpenRouter passthrough
/// when the driving event was a `UserMessage`; gift / proactive pass `None`.
fn assemble_chat_request(
    resolved: ResolvedModel,
    system_prompt: String,
    history: Vec<eros_engine_store::chat::ChatMessage>,
    audit: Option<&LlmAudit>,
) -> ChatRequest {
    let mut messages = Vec::with_capacity(history.len() + 1);
    messages.push(ChatMessage {
        role: "system".to_string(),
        content: system_prompt,
    });
    for msg in history {
        // User and TIP gift_user rows feed the MODEL-FACING text under the "user"
        // role — a tip turn IS a user turn to the model (OpenRouter only knows
        // system/user/assistant). gift_user is tip-only now (the legacy in-app
        // Gift Event endpoint was removed), so no tip/legacy gate is needed.
        // Assistant rows always feed `content` (their pre_filter_content is the
        // pre-output-filter original and must never re-enter the prompt).
        let (role, content) = match msg.role.as_str() {
            "user" => ("user", model_facing_user_text(&msg)),
            "gift_user" => ("user", model_facing_user_text(&msg)),
            "assistant" => ("assistant", model_facing_assistant_text(msg)),
            _ => continue,
        };
        messages.push(ChatMessage {
            role: role.to_string(),
            content,
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
        top_p: resolved.top_p,
        frequency_penalty: resolved.frequency_penalty,
        presence_penalty: resolved.presence_penalty,
        max_tokens: resolved.max_tokens,
        user: audit_user,
        session_id: audit_session,
        metadata: audit_metadata,
        reasoning: resolved.reasoning,
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
/// Returns (empty, empty) without hitting Voyage when both layers are off or
/// the query is blank. Voyage failure also degrades silently to (empty, empty)
/// — recall failure must never block a chat reply (the persona just looks
/// slightly less "with it" for that turn).
async fn recall_memory(
    state: &AppState,
    user_id: Uuid,
    instance_id: Uuid,
    query_text: &str,
    x_on: bool,
    y_on: bool,
) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    if (!x_on && !y_on) || query_text.trim().is_empty() {
        return (vec![], vec![]);
    }
    let embedding = match state.voyage.embed_query(query_text).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("voyage embed_query failed: {e}");
            return (vec![], vec![]);
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
    recall_memory_with_embedding(&state.pool, user_id, instance_id, &embedding, x_on, y_on).await
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
async fn recall_memory_with_embedding(
    pool: &PgPool,
    user_id: Uuid,
    instance_id: Uuid,
    embedding: &[f32],
    x_on: bool,
    y_on: bool,
) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    let repo = MemoryRepo { pool };

    let (profile_groups, relationship): (Vec<(String, Vec<String>)>, Vec<String>) = if x_on {
        // X on ⇒ Y on: original three-way parallel recall (hot path).
        let (grouped_res, raw_res, rel_res) = tokio::join!(
            repo.search_profile_grouped(user_id, embedding, K_PER_CATEGORY),
            repo.search(user_id, None, embedding, PROFILE_RECALL_K),
            repo.search(user_id, Some(instance_id), embedding, RELATIONSHIP_RECALL_K),
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
            .search(user_id, Some(instance_id), embedding, RELATIONSHIP_RECALL_K)
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

/// Render the `companion_insights` JSONB blob as bullet strings. Skips
/// empty / missing fields. `matching_preferences` is intentionally omitted
/// — it's a structured sub-object that doesn't fit a single-line bullet
/// and isn't useful in chat tone anyway.
///
/// Test-only parity reference: the live path renders the flat human_insights
/// mirror via human_insights_to_bullets; this renders companion_insights JSONB
/// directly and the parity tests pin the two together. No production caller
/// remains (the gift path that used it was removed).
#[cfg(test)]
fn insights_to_bullets(insights: &Value) -> Vec<String> {
    let Some(obj) = insights.as_object() else {
        return vec![];
    };
    let mut out = Vec::new();

    let push_str = |out: &mut Vec<String>, key: &str, label: &str| {
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            let s = s.trim();
            if !s.is_empty() {
                out.push(format!("{label}：{s}"));
            }
        }
    };
    let push_arr = |out: &mut Vec<String>, key: &str, label: &str| {
        if let Some(arr) = obj.get(key).and_then(Value::as_array) {
            let parts: Vec<&str> = arr
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if !parts.is_empty() {
                out.push(format!("{label}：{}", parts.join("、")));
            }
        }
    };

    push_str(&mut out, "city", "城市");
    push_str(&mut out, "location", "所在地");
    push_str(&mut out, "hometown", "老家");
    push_str(&mut out, "nationality", "国籍");
    push_str(&mut out, "occupation", "职业");
    push_str(&mut out, "education", "教育");
    push_str(&mut out, "mbti_guess", "MBTI");
    push_str(&mut out, "love_values", "感情观");
    push_str(&mut out, "relationship_history", "感情经历");
    push_arr(&mut out, "interests", "兴趣");
    push_str(&mut out, "emotional_needs", "情感需求");
    push_str(&mut out, "family", "家庭");
    push_str(&mut out, "finance_status", "经济状况");
    push_str(&mut out, "life_rhythm", "作息");
    push_str(&mut out, "social_pattern", "社交模式");
    push_arr(&mut out, "personality_traits", "性格特质");
    push_str(&mut out, "future_plans", "未来计划");

    out
}

/// Render a `human_insights` row as 基础画像 bullets. Mirrors
/// `insights_to_bullets`' labels / order / trim / empty-skip exactly so that
/// `InsightMode::Full` reproduces the legacy output byte-for-byte. `Neutral`
/// drops the intimate fields (love_values / relationship_history / interests /
/// emotional_needs / family / finance_status).
/// Matching-only columns (preferred_gender / age / deal_breakers) are never
/// rendered. `Off` → empty (defensive; loaders gate it before calling).
fn human_insights_to_bullets(row: &HumanInsightsRow, mode: InsightMode) -> Vec<String> {
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
    let history_anchor = match &input.event {
        eros_engine_core::types::Event::UserMessage { history_anchor, .. } => *history_anchor,
        _ => eros_engine_core::types::HistoryAnchor::Latest,
    };
    let history = match history_fetch_for(history_anchor) {
        HistoryFetch::Latest => chat_repo.history(session_id, HISTORY_WINDOW, 0).await?,
        HistoryFetch::Anchored(anchor) => {
            chat_repo
                .history_anchored(session_id, user_message_id, anchor, HISTORY_WINDOW)
                .await?
        }
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

    let (mut profile_groups, relationship_facts) =
        recall_memory(state, user_id, instance_id, &query_text, x_on, y_on).await;

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

    let recent_turns = fetch_recent_turn_pairs(&state.pool, session_id, user_message_id).await;

    // Mine over-used openings from the persona's own recent assistant turns
    // (non-fatal: a DB hiccup just omits the [avoid_repetition] block).
    let recent_assistant = ChatRepo { pool: &state.pool }
        .recent_assistant_contents(session_id, user_message_id, 6)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                session_id = %session_id,
                "recent_assistant_contents fetch failed; [avoid_repetition] omitted"
            );
            Vec::new()
        });
    let avoid_patterns = crate::repetition::overused_openings(&recent_assistant);

    // Recent affinity reasons for the emotional trajectory. The store returns
    // newest-first; reverse to oldest→newest for a readable [emotional_context].
    let mut emotional_context = AffinityRepo { pool: &state.pool }
        .recent_emotional_reasons(session_id, user_message_id, 5)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                session_id = %session_id,
                "recent_emotional_reasons fetch failed; [emotional_context] omitted"
            );
            Vec::new()
        });
    emotional_context.reverse();

    // #113: dedup recalled memories (mainly the cross-layer 用户：{u} / {u}
    // overlap) before they enter the prompt. Pure; no new DB calls.
    let (profile_groups, relationship_facts) =
        crate::memory_hygiene::prune_recalled(profile_groups, relationship_facts);

    let mut system_prompt = build_prompt(
        &input.persona,
        &profile_groups,
        &relationship_facts,
        Some(&input.affinity),
        plan.reply_style,
        &plan.context_hints,
        &kept_traits,
        affinity_scope,
        &recent_turns,
        &avoid_patterns,
        &emotional_context,
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

    let injected_tags: Vec<String> = kept_traits.iter().map(|t| t.tag.clone()).collect();
    Ok((
        assemble_chat_request(
            resolved,
            system_prompt,
            history,
            audit_from_event(&input.event),
        ),
        injected_tags,
    ))
}

/// Fetch the last `limit` complete (user|gift_user, assistant) pairs for the
/// session, used to render the `[recent_conversation]` short-term memory block.
///
/// Cutoff = the current turn's persisted user row's `sent_at` (looked up by
/// `user_message_id` via subquery). Using `Utc::now()` instead would be racy:
/// under concurrent streams on the same session, a later already-completed
/// turn could insert a row between wall-clock-now and the read of recent
/// rows, leaking "future" conversation into the current turn's prompt. The
/// SQL filter is strict `<`, so the current-turn row itself is excluded.
///
/// Non-fatal: a DB hiccup degrades to an empty Vec with a warn-level log so
/// short-term memory is omitted but prompt assembly still succeeds.
async fn fetch_recent_turn_pairs(
    pool: &PgPool,
    session_id: Uuid,
    user_message_id: Uuid,
) -> Vec<(String, String)> {
    ChatRepo { pool }
        .recent_turn_pairs_before_message(session_id, user_message_id, 3)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = %e,
                session_id = %session_id,
                user_message_id = %user_message_id,
                "recent_turn_pairs fetch failed; [recent_conversation] omitted"
            );
            Vec::new()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn insights_to_bullets_empty_object() {
        assert!(insights_to_bullets(&json!({})).is_empty());
    }

    #[test]
    fn insights_to_bullets_non_object_returns_empty() {
        assert!(insights_to_bullets(&json!("just a string")).is_empty());
        assert!(insights_to_bullets(&json!(["array", "not", "object"])).is_empty());
        assert!(insights_to_bullets(&json!(null)).is_empty());
    }

    #[test]
    fn insights_to_bullets_skips_empty_strings() {
        let v = json!({
            "city": "",
            "occupation": "   ",
            "mbti_guess": "INFP",
        });
        let bullets = insights_to_bullets(&v);
        assert_eq!(bullets, vec!["MBTI：INFP".to_string()]);
    }

    #[test]
    fn insights_to_bullets_renders_string_fields() {
        let v = json!({
            "city": "上海",
            "occupation": "产品经理",
            "mbti_guess": "ENFJ",
            "love_values": "重视沟通",
            "emotional_needs": "需要被认可",
            "life_rhythm": "夜猫子",
        });
        let bullets = insights_to_bullets(&v);
        assert_eq!(
            bullets,
            vec![
                "城市：上海".to_string(),
                "职业：产品经理".to_string(),
                "MBTI：ENFJ".to_string(),
                "感情观：重视沟通".to_string(),
                "情感需求：需要被认可".to_string(),
                "作息：夜猫子".to_string(),
            ]
        );
    }

    #[test]
    fn insights_to_bullets_joins_arrays_and_skips_blanks() {
        let v = json!({
            "interests": ["登山", "  ", "精酿"],
            "personality_traits": ["真诚", "敏感"],
        });
        let bullets = insights_to_bullets(&v);
        assert_eq!(
            bullets,
            vec![
                "兴趣：登山、精酿".to_string(),
                "性格特质：真诚、敏感".to_string(),
            ]
        );
    }

    #[test]
    fn insights_to_bullets_omits_matching_preferences() {
        let v = json!({
            "city": "北京",
            "matching_preferences": {
                "preferred_gender": "female",
                "age_range": [25, 35],
                "deal_breakers": ["smoking"],
            },
        });
        let bullets = insights_to_bullets(&v);
        assert_eq!(bullets, vec!["城市：北京".to_string()]);
    }

    #[test]
    fn insights_to_bullets_preserves_canonical_field_order() {
        // Field order matters for prompt readability — city before MBTI,
        // emotional_needs before life_rhythm, etc.
        let v = json!({
            "personality_traits": ["真诚"],
            "city": "上海",
            "interests": ["登山"],
            "occupation": "工程师",
        });
        let bullets = insights_to_bullets(&v);
        assert_eq!(
            bullets,
            vec![
                "城市：上海".to_string(),
                "职业：工程师".to_string(),
                "兴趣：登山".to_string(),
                "性格特质：真诚".to_string(),
            ]
        );
    }

    // ─── Integration tests: recall_memory_with_embedding ──────────────────
    //
    // These exercise the pure-DB half of the recall pipeline against a
    // live Postgres (via `#[sqlx::test]`). The Voyage-dependent outer
    // wrapper `recall_memory` is intentionally not tested here — it would
    // either need a live Voyage key or a trait-mock indirection that
    // doesn't justify its weight for a single thin function.

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
        )
        .await;
        assert!(profile.is_empty());
        assert!(relationship.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn recall_memory_with_embedding_isolates_layers(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
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
        let instance_id = Uuid::new_v4();
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
        let instance_id = Uuid::new_v4();
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
        let instance_id = Uuid::new_v4();
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
        let instance_id = Uuid::new_v4();
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
        )
        .await;
        assert!(
            !prof3.is_empty(),
            "profile groups should be present when X on"
        );
        assert!(!rel3.is_empty(), "relationship should be present when Y on");
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

    #[test]
    fn human_insights_full_matches_companion_insights_renderer() {
        // The byte-identical parity contract: Full mode over a human_insights row
        // must equal insights_to_bullets over the equivalent companion_insights JSON.
        let row = sample_human_row();
        let equivalent = serde_json::json!({
            "city": "上海",
            "occupation": "设计师",
            "education": "美院本科",
            "mbti_guess": "INFP",
            "love_values": "慢热",
            "relationship_history": "单身两年",
            "interests": ["登山", "摄影"],
            "emotional_needs": "被理解",
            "family": "独生女，父母在杭州",
            "finance_status": "攒钱中",
            "life_rhythm": "夜猫子",
            "social_pattern": "小圈子聚会",
            "personality_traits": ["温柔"],
            "future_plans": "想开工作室",
            // matching-only fields exist in JSON too but neither renderer emits them
            "matching_preferences": { "preferred_gender": "female", "age_range": [25, 35] }
        });
        assert_eq!(
            human_insights_to_bullets(&row, InsightMode::Full),
            insights_to_bullets(&equivalent)
        );
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

    #[test]
    fn insights_to_bullets_renders_geo_after_city() {
        let v = serde_json::json!({
            "city": "深圳", "location": "台北", "hometown": "新界",
            "nationality": "中国香港", "occupation": "工程师"
        });
        assert_eq!(
            insights_to_bullets(&v),
            vec![
                "城市：深圳",
                "所在地：台北",
                "老家：新界",
                "国籍：中国香港",
                "职业：工程师"
            ]
        );
    }

    #[test]
    fn human_insights_geo_matches_companion_insights_renderer() {
        let equivalent = serde_json::json!({
            "city": "深圳", "location": "台北", "hometown": "新界", "nationality": "中国香港"
        });
        assert_eq!(
            human_insights_to_bullets(&sample_geo_row(), InsightMode::Full),
            insights_to_bullets(&equivalent)
        );
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
            .project_from_insights(user_id, &insights)
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
            history_anchor: Default::default(),
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
            model: None,
            usage: None,
            generation_id: None,
            assistant_action_type: None,
            pre_filter_content: pre.map(|s| s.to_string()),
            metadata: None,
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
            top_p: Some(0.9),
            frequency_penalty: Some(0.4),
            presence_penalty: Some(0.2),
            max_tokens: 100,
            allow_traits: None,
            reasoning: None,
            retry_depth: 0,
        };
        let req = assemble_chat_request(resolved, "SYS".into(), vec![tip, plain, assistant], None);

        // Sampling knobs flow from ResolvedModel onto the ChatRequest.
        assert_eq!(req.top_p, Some(0.9));
        assert_eq!(req.frequency_penalty, Some(0.4));
        assert_eq!(req.presence_penalty, Some(0.2));

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
            model: None,
            usage: None,
            generation_id: None,
            assistant_action_type: None,
            pre_filter_content: None,
            metadata,
        }
    }

    #[test]
    fn assistant_row_with_image_prompt_appends_marker() {
        let row = assistant_row(
            "这是我的回复",
            Some(serde_json::json!({ "image": { "prompt": "smiling in a cafe" } })),
        );
        let out = model_facing_assistant_text(row);
        assert!(out.contains("这是我的回复"));
        assert!(out.contains("[你给对方发送了一张照片：smiling in a cafe]"));
        // marker is appended after two newlines
        assert!(out.contains("这是我的回复\n\n[你给对方发送了一张照片：smiling in a cafe]"));
    }

    #[test]
    fn assistant_row_empty_content_with_image_prompt_uses_marker_only() {
        let row = assistant_row(
            "",
            Some(serde_json::json!({ "image": { "prompt": "sunset on the beach" } })),
        );
        let out = model_facing_assistant_text(row);
        assert_eq!(out, "[你给对方发送了一张照片：sunset on the beach]");
    }

    #[test]
    fn assistant_row_without_image_metadata_unchanged() {
        let row = assistant_row("普通回复", None);
        assert_eq!(model_facing_assistant_text(row), "普通回复");
    }

    #[test]
    fn assistant_row_image_metadata_without_prompt_unchanged() {
        let row = assistant_row(
            "普通回复",
            Some(serde_json::json!({ "image": { "url": "https://x/y.png" } })),
        );
        assert_eq!(model_facing_assistant_text(row), "普通回复");
    }

    /// End-to-end check of the cutoff semantics that `fetch_recent_turn_pairs`
    /// relies on. We exercise the repo path the handler actually calls (via
    /// `ChatRepo::recent_turn_pairs_before_message` keyed on the current
    /// turn's user message id) rather than the full handler tree, because
    /// `build_reply_request` pulls in persona / model_config / voyage /
    /// openrouter wiring that isn't trivially mockable. The handler itself is
    /// verifiable by inspection — both call sites go through
    /// `fetch_recent_turn_pairs(&state.pool, session_id, user_message_id)`.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn handlers_inject_recent_conversation_into_system_prompt(pool: PgPool) {
        use eros_engine_store::chat::{AssistantInsert, ChatRepo, UpsertUserOutcome};

        let chat_repo = ChatRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session = chat_repo
            .create_session(user_id, instance_id)
            .await
            .unwrap();

        // Insert 2 prior complete (user, assistant) pairs.
        for n in 0..2u8 {
            let u_ulid = format!("01J000000000000000008{n}001A");
            let uid = match chat_repo
                .upsert_user_message_idempotent(session.id, &format!("u{n}"), &u_ulid, "user", None)
                .await
                .unwrap()
            {
                UpsertUserOutcome::Inserted { message_id } => message_id,
                other => panic!("expected Inserted, got {other:?}"),
            };
            chat_repo
                .insert_assistant_batch(
                    session.id,
                    uid,
                    &[AssistantInsert {
                        id: Uuid::new_v4(),
                        content: format!("a{n}"),
                        assistant_action_type: "reply".into(),
                        truncated: false,
                        continues_from_message_id: None,
                        model: Some("test-model".into()),
                        usage: None,
                        generation_id: None,
                        filter_audit: None,
                        metadata: None,
                    }],
                )
                .await
                .unwrap();
        }

        // Insert the "current" user row that the handler would pass to
        // `fetch_recent_turn_pairs` as `user_message_id`.
        let current_msg_id = match chat_repo
            .upsert_user_message_idempotent(
                session.id,
                "u_current",
                "01J0000000000000000080900A",
                "user",
                None,
            )
            .await
            .unwrap()
        {
            UpsertUserOutcome::Inserted { message_id } => message_id,
            other => panic!("expected Inserted, got {other:?}"),
        };

        // What the handler's fetch will see when assembling the current turn.
        // Cutoff = current_msg_id's sent_at, so its own row is excluded and
        // only the 2 prior complete pairs come back.
        let pairs = chat_repo
            .recent_turn_pairs_before_message(session.id, current_msg_id, 3)
            .await
            .unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("u0".to_string(), "a0".to_string()));
        assert_eq!(pairs[1], ("u1".to_string(), "a1".to_string()));

        // Concurrent-stream isolation: a LATER user row inserted after the
        // current turn (simulating another stream completing between this
        // turn's user-insert and the recent-turn fetch) must NOT appear.
        // Wall-clock `Utc::now()` would include it; cutoff-by-message-id
        // doesn't, because the subquery resolves to `current_msg_id`'s
        // sent_at — which is before the later row.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let _ = chat_repo
            .upsert_user_message_idempotent(
                session.id,
                "u_later",
                "01J0000000000000000080999A",
                "user",
                None,
            )
            .await
            .unwrap();
        let pairs_after = chat_repo
            .recent_turn_pairs_before_message(session.id, current_msg_id, 3)
            .await
            .unwrap();
        assert_eq!(
            pairs_after.len(),
            2,
            "later concurrent-stream row must not appear in [recent_conversation]"
        );
        assert_eq!(pairs_after[0], ("u0".to_string(), "a0".to_string()));
        assert_eq!(pairs_after[1], ("u1".to_string(), "a1".to_string()));
    }

    #[test]
    fn history_fetch_for_maps_each_anchor() {
        use eros_engine_core::types::HistoryAnchor;
        let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(123, 0).unwrap();
        assert_eq!(
            history_fetch_for(HistoryAnchor::Latest),
            HistoryFetch::Latest
        );
        assert_eq!(
            history_fetch_for(HistoryAnchor::At {
                message_id: uuid::Uuid::nil(),
                sent_at: ts
            }),
            HistoryFetch::Anchored(Some(ts)),
        );
        assert_eq!(
            history_fetch_for(HistoryAnchor::DropHistory),
            HistoryFetch::Anchored(None),
        );
    }
}

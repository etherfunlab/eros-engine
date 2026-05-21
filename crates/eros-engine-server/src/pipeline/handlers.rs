// SPDX-License-Identifier: AGPL-3.0-only
//! Action handlers — one per ActionType. Each assembles a ChatRequest
//! (or None if no LLM call is needed) based on the PDE's ActionPlan.
//!
//! Ported from `eros-gateway/src/engine/handlers/{reply,ghost,gift,proactive}.rs`
//! with these OSS-specific changes:
//!
//! - All handlers go through `eros_engine_store::chat::ChatRepo` rather than
//!   inline `sqlx::query_as` against `chat_messages`.
//! - `ChatRequest` is built around the OSS `eros_engine_llm::openrouter::ChatRequest`
//!   shape (`model` / `fallback_model` / `messages` / `temperature` / `max_tokens`),
//!   resolved via `state.model_config` at handler time. The resolver takes
//!   `task: &str` + the request's `tier: Option<&str>` and returns the
//!   per-tier model / fallback / allow_traits.
//! - `GiftHandler` carries `deltas: AffinityDeltas` directly — there is
//!   no shop item / gift-record lookup since the OSS engine has no
//!   credit ledger.

use async_trait::async_trait;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use eros_engine_core::affinity::AffinityDeltas;
use eros_engine_core::types::{ActionPlan, DecisionInput, Event, LlmAudit, PromptTrait};
use eros_engine_llm::model_config::ResolvedModel;
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
use eros_engine_store::chat::ChatRepo;
use eros_engine_store::insight::InsightRepo;
use eros_engine_store::memory::MemoryRepo;

use crate::error::AppError;
use crate::prompt::{build_prompt, PendingGift};
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

#[async_trait]
pub trait ActionHandler: Send + Sync {
    // Dispatched only by the retained-but-unreached sync `pipeline::run`
    // (the sync `/message` handler was removed); see pipeline/mod.rs note.
    #[allow(dead_code)]
    async fn handle(
        &self,
        input: &DecisionInput,
        plan: &ActionPlan,
    ) -> Result<Option<ChatRequest>, AppError>;
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
        match msg.role.as_str() {
            "user" | "assistant" => messages.push(ChatMessage {
                role: msg.role,
                content: msg.content,
            }),
            _ => continue,
        }
    }

    let (audit_user, audit_session, audit_metadata) = audit
        .map(|a| (a.user.clone(), a.session_id.clone(), a.metadata.clone()))
        .unwrap_or_default();

    ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages,
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        user: audit_user,
        session_id: audit_session,
        metadata: audit_metadata,
    }
}

// ─── Memory recall + insight injection helpers ────────────────────

/// Embed `query_text` once, then delegate to `recall_memory_with_embedding`.
/// Empty query → returns (empty, empty) without hitting Voyage. Voyage
/// failure also degrades silently to (empty, empty) — recall failure must
/// never block a chat reply (the persona just looks slightly less "with
/// it" for that turn).
async fn recall_memory(
    state: &AppState,
    user_id: Uuid,
    instance_id: Uuid,
    query_text: &str,
) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    if query_text.trim().is_empty() {
        return (vec![], vec![]);
    }
    let embedding = match state.voyage.embed_query(query_text).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("voyage embed_query failed: {e}");
            return (vec![], vec![]);
        }
    };
    // Observation hook for recall-quality investigation (RUST_LOG=...=debug).
    // Cheap fields only: scalars + lengths, no content / no PII.
    tracing::debug!(
        user_id = %user_id,
        query_len = query_text.chars().count(),
        embedding_dim = embedding.len(),
        "recall_memory: embedded query, dispatching pgvector search"
    );
    recall_memory_with_embedding(&state.pool, user_id, instance_id, &embedding).await
}

/// Pure-DB inner half of memory recall. Takes a pre-computed embedding,
/// fans out to profile (categorised + raw fallback) + relationship layers
/// in parallel via `tokio::join!`, and returns:
/// - profile_groups: `Vec<(label, bullets)>` — categorised rows grouped by
///   `category` if any exist; otherwise a single `("近况", raw_rows)` group
///   so users with no classified sessions yet still get profile context.
/// - relationship: flat `Vec<String>` — relationship rows are full turn
///   dumps and not categorised by the dreaming-lite pass.
async fn recall_memory_with_embedding(
    pool: &PgPool,
    user_id: Uuid,
    instance_id: Uuid,
    embedding: &[f32],
) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    let repo = MemoryRepo { pool };
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
    let profile_groups = build_profile_groups(grouped_rows, raw_rows);

    let relationship: Vec<String> = match rel_res {
        Ok(rows) => rows.into_iter().map(|r| r.content).collect(),
        Err(e) => {
            tracing::warn!("relationship-layer memory search failed: {e}");
            vec![]
        }
    };

    let profile_total_chars: usize = profile_groups
        .iter()
        .flat_map(|(_, items)| items.iter().map(|s| s.chars().count()))
        .sum();
    tracing::debug!(
        user_id = %user_id,
        instance_id = %instance_id,
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

/// Load `companion_insights` for the user and render the structured fields
/// as Chinese-language bullets that fit naturally into the
/// `【你对他的了解（通用画像）】` prompt section. Takes `&PgPool` directly
/// (not `&AppState`) so it's reachable from sqlx integration tests without
/// constructing the full state.
async fn load_insight_bullets(pool: &PgPool, user_id: Uuid) -> Vec<String> {
    let repo = InsightRepo { pool };
    let row = match repo.load(user_id).await {
        Ok(Some(row)) => row,
        Ok(None) => return vec![],
        Err(e) => {
            tracing::warn!("insight load failed: {e}");
            return vec![];
        }
    };
    insights_to_bullets(&row.insights)
}

/// Render the `companion_insights` JSONB blob as bullet strings. Skips
/// empty / missing fields. `matching_preferences` is intentionally omitted
/// — it's a structured sub-object that doesn't fit a single-line bullet
/// and isn't useful in chat tone anyway.
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
    push_str(&mut out, "occupation", "职业");
    push_str(&mut out, "mbti_guess", "MBTI");
    push_str(&mut out, "love_values", "感情观");
    push_arr(&mut out, "interests", "兴趣");
    push_str(&mut out, "emotional_needs", "情感需求");
    push_str(&mut out, "life_rhythm", "作息");
    push_arr(&mut out, "personality_traits", "性格特质");

    out
}

// ─── Reply ──────────────────────────────────────────────────────────

/// Build a ChatRequest for the Reply action. Shared by the sync
/// `ReplyHandler` and the streaming `pipeline::stream::run_stream`.
pub(super) async fn build_reply_request(
    state: &AppState,
    input: &DecisionInput,
    plan: &ActionPlan,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
) -> Result<ChatRequest, AppError> {
    let chat_repo = ChatRepo { pool: &state.pool };
    let history = chat_repo.history(session_id, HISTORY_WINDOW, 0).await?;

    let query_text = match &input.event {
        Event::UserMessage { content, .. } => content.as_str(),
        _ => "",
    };

    let (mut profile_groups, relationship_facts) =
        recall_memory(state, user_id, instance_id, query_text).await;

    let insight_bullets = load_insight_bullets(&state.pool, user_id).await;
    if !insight_bullets.is_empty() {
        profile_groups.insert(0, ("基础画像".into(), insight_bullets));
    }

    let tip_personality = input
        .persona
        .genome
        .tip_personality
        .as_deref()
        .unwrap_or("normal");

    let pending_gifts: Vec<PendingGift> = vec![];

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

    let system_prompt = build_prompt(
        &input.persona,
        &profile_groups,
        &relationship_facts,
        Some(&input.affinity),
        &pending_gifts,
        tip_personality,
        plan.reply_style,
        &plan.context_hints,
        &kept_traits,
    );

    Ok(assemble_chat_request(
        resolved,
        system_prompt,
        history,
        audit_from_event(&input.event),
    ))
}

/// Build a ChatRequest for the GiftReaction action. Shared by the sync
/// `GiftHandler` and the streaming `pipeline::stream::run_stream`.
pub(super) async fn build_gift_request(
    state: &AppState,
    input: &DecisionInput,
    plan: &ActionPlan,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    pending: &[PendingGift],
) -> Result<ChatRequest, AppError> {
    let chat_repo = ChatRepo { pool: &state.pool };
    let history = chat_repo.history(session_id, HISTORY_WINDOW, 0).await?;

    let query_text = history
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.as_str())
        .unwrap_or("");

    let (mut profile_groups, relationship_facts) =
        recall_memory(state, user_id, instance_id, query_text).await;

    let insight_bullets = load_insight_bullets(&state.pool, user_id).await;
    if !insight_bullets.is_empty() {
        profile_groups.insert(0, ("基础画像".into(), insight_bullets));
    }

    let tip_personality = input
        .persona
        .genome
        .tip_personality
        .as_deref()
        .unwrap_or("normal");

    let resolved = state.model_config.resolve(CHAT_TASK, None);

    let system_prompt = build_prompt(
        &input.persona,
        &profile_groups,
        &relationship_facts,
        Some(&input.affinity),
        pending,
        tip_personality,
        plan.reply_style,
        &plan.context_hints,
        &[],
    );

    Ok(assemble_chat_request(
        resolved,
        system_prompt,
        history,
        None,
    ))
}

pub struct ReplyHandler<'a> {
    pub state: &'a AppState,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
}

#[async_trait]
impl<'a> ActionHandler for ReplyHandler<'a> {
    async fn handle(
        &self,
        input: &DecisionInput,
        plan: &ActionPlan,
    ) -> Result<Option<ChatRequest>, AppError> {
        let req = build_reply_request(
            self.state,
            input,
            plan,
            self.session_id,
            self.user_id,
            self.instance_id,
        )
        .await?;
        Ok(Some(req))
    }
}

// ─── Ghost ──────────────────────────────────────────────────────────

/// Ghost handler is intentionally a no-op at the chat-request layer:
/// the affinity row update happens in `pipeline::post_process`. The
/// `state` / `session_id` fields are kept for future tracing hooks and
/// for symmetry with the other handlers.
#[allow(dead_code)]
pub struct GhostHandler<'a> {
    pub state: &'a AppState,
    pub session_id: Uuid,
}

#[async_trait]
impl<'a> ActionHandler for GhostHandler<'a> {
    async fn handle(
        &self,
        _input: &DecisionInput,
        _plan: &ActionPlan,
    ) -> Result<Option<ChatRequest>, AppError> {
        tracing::info!("Ghost decision: session={}", self.session_id);
        // Affinity mutation and DB write happen in pipeline::post_process,
        // which sees ActionType::Ghost and calls AffinityRepo::record_ghost.
        Ok(None)
    }
}

// ─── Gift ───────────────────────────────────────────────────────────

/// Gift reaction handler.
///
/// Replaces the gateway's shop-item / gift-record lookup. The OSS engine
/// has no credit ledger — the gift event endpoint (T11) injects the
/// affinity deltas and an optional pending-gift list directly.
pub struct GiftHandler<'a> {
    pub state: &'a AppState,
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    /// Caller-supplied deltas — passed through to the post-process step
    /// via the ActionPlan / event channel; not consumed inside `handle()`.
    #[allow(dead_code)]
    pub deltas: AffinityDeltas,
    /// Caller-supplied pending gifts (possibly empty) for prompt context.
    pub pending: Vec<PendingGift>,
}

#[async_trait]
impl<'a> ActionHandler for GiftHandler<'a> {
    async fn handle(
        &self,
        input: &DecisionInput,
        plan: &ActionPlan,
    ) -> Result<Option<ChatRequest>, AppError> {
        let req = build_gift_request(
            self.state,
            input,
            plan,
            self.session_id,
            self.user_id,
            self.instance_id,
            &self.pending,
        )
        .await?;
        Ok(Some(req))
    }
}

// ─── Proactive ──────────────────────────────────────────────────────

/// Proactive handler is a stub today — Phase 6 in the gateway / a
/// later OSS milestone produces an outbound message here. Fields kept
/// for the eventual implementation.
#[allow(dead_code)]
pub struct ProactiveHandler<'a> {
    pub state: &'a AppState,
    pub session_id: Uuid,
}

#[async_trait]
impl<'a> ActionHandler for ProactiveHandler<'a> {
    async fn handle(
        &self,
        _input: &DecisionInput,
        _plan: &ActionPlan,
    ) -> Result<Option<ChatRequest>, AppError> {
        // Stub — Phase 6 in the gateway / a later OSS milestone will
        // produce an outbound message here.
        Ok(None)
    }
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

    // ─── Integration tests: recall_memory_with_embedding + load_insight_bullets ───
    //
    // These exercise the pure-DB halves of the recall pipeline against a
    // live Postgres (via `#[sqlx::test]`). The Voyage-dependent outer
    // wrapper `recall_memory` is intentionally not tested here — it would
    // either need a live Voyage key or a trait-mock indirection that
    // doesn't justify its weight for a single thin function.

    use eros_engine_store::insight::InsightRepo;
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
        let (profile, relationship) =
            recall_memory_with_embedding(&pool, user_id, instance_id, &unit_embedding(7)).await;
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
        )
        .await
        .unwrap();

        let (profile_groups, relationship) =
            recall_memory_with_embedding(&pool, user_id, instance_id, &unit_embedding(11)).await;
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
        )
        .await
        .unwrap();

        let (profile_groups, _relationship) =
            recall_memory_with_embedding(&pool, user_id, instance_id, &unit_embedding(7)).await;

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
            )
            .await
            .unwrap();
        }

        let (profile_groups, relationship) =
            recall_memory_with_embedding(&pool, user_id, instance_id, &unit_embedding(100)).await;

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
        )
        .await
        .unwrap();

        // Query embedding hits the profile target seed exactly. All rows
        // here are uncategorised, so the raw fallback fires under "近况"
        // and its first item is the cosine-nearest one.
        let (profile_groups, _relationship) =
            recall_memory_with_embedding(&pool, user_id, instance_id, &unit_embedding(42)).await;
        assert_eq!(profile_groups.len(), 1);
        assert_eq!(profile_groups[0].0, "近况");
        assert_eq!(
            profile_groups[0].1.first().map(String::as_str),
            Some("profile target"),
        );

        // Query at the relationship target seed.
        let (_profile2, relationship2) =
            recall_memory_with_embedding(&pool, user_id, instance_id, &unit_embedding(99)).await;
        assert_eq!(
            relationship2.first().map(String::as_str),
            Some("relationship target"),
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn load_insight_bullets_returns_empty_when_no_row(pool: PgPool) {
        let bullets = load_insight_bullets(&pool, Uuid::new_v4()).await;
        assert!(bullets.is_empty());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn load_insight_bullets_renders_after_merge(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let repo = InsightRepo { pool: &pool };

        repo.merge(
            user_id,
            json!({
                "city": "上海",
                "mbti_guess": "INFP",
                "interests": ["登山", "精酿"],
            }),
        )
        .await
        .unwrap();

        let bullets = load_insight_bullets(&pool, user_id).await;
        assert_eq!(
            bullets,
            vec![
                "城市：上海".to_string(),
                "MBTI：INFP".to_string(),
                "兴趣：登山、精酿".to_string(),
            ]
        );
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
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Dreaming-lite: session-end memory extraction sweeper.
//!
//! Background tokio task that scans `engine.chat_sessions` for idle,
//! unclassified sessions and runs the `memory_extraction` LLM task on
//! their `chat_messages`. Extracted candidates with category tags are
//! written to `engine.companion_memories` as profile-layer rows; the
//! session's `classified_at` is then stamped to suppress re-sweeps.
//!
//! Multi-instance safety: the picker uses an atomic
//! `UPDATE ... WHERE id IN (SELECT ... FOR UPDATE SKIP LOCKED)
//! RETURNING ...` to claim rows in one statement. Concurrent sweepers
//! see disjoint sets and don't race on the same session. A separate
//! `classification_claimed_at` column carries the claim sentinel; if a
//! worker crashes after claiming but before stamping `classified_at`,
//! the row is re-eligible after `DREAMING_CLAIM_STALE_SECS` (default
//! 600s).
//!
//! Failure handling:
//! - Network/DB error during pick or stamp → propagate, retry next tick.
//! - LLM call error → propagate (no stamp), retry next tick.
//! - LLM returns garbage / empty parse → stamp anyway so a poison-pill
//!   session can't loop the sweeper forever.

use std::time::Duration;

use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
use eros_engine_store::memory::{MemoryLayer, MemoryRepo};

use crate::state::AppState;

const MEMORY_TASK: &str = "memory_extraction";
/// Sentinel OpenRouter `user` for system-initiated (non-user-triggered) LLM
/// calls. Dreaming runs in the background sweeper with no live request, so
/// per-user attribution is meaningless; this buckets all dreaming spend
/// under one id. All-ones (not the `0…1` nil-plus-one, which is already used
/// in our database). Not a real auth UUID (v4) and not a hashed client id,
/// so it cannot collide with a real user.
const SYSTEM_AUDIT_USER: &str = "11111111-1111-1111-1111-111111111111";
const PICK_BATCH: i64 = 10;

#[derive(Debug, Deserialize)]
struct MemoryCandidate {
    content: String,
    category: String,
    /// Everything else the extraction prompt emitted per memory (domain /
    /// evidence_type / temporality / persistence / confidence, plus whatever
    /// future prompts add). Captured opaquely — the engine never validates
    /// the vocabulary; it lands verbatim in companion_memories.metadata.
    #[serde(flatten)]
    metadata: serde_json::Map<String, serde_json::Value>,
}

/// Flattened remainder → the JSONB value to persist. Empty map ⇒ `None`
/// ⇒ NULL column (old-prompt deployments keep writing NULL, not `{}`).
fn candidate_metadata(cand: &MemoryCandidate) -> Option<serde_json::Value> {
    if cand.metadata.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(cand.metadata.clone()))
    }
}

/// Run forever. Spawn this once at server startup. Returns immediately
/// (and never spawns the loop) if `state.config.dreaming_tick` is zero.
pub async fn sweeper(state: AppState) {
    let interval = state.config.dreaming_tick;
    let idle = state.config.dreaming_idle_threshold;
    let claim_stale = state.config.dreaming_claim_stale_threshold;
    if interval.is_zero() {
        tracing::info!("dreaming sweeper disabled (DREAMING_DISABLED=1 or tick=0)");
        return;
    }
    // memory_extraction is omittable: a missing [tasks.memory_extraction] section
    // (or a blank filter_prompt, which would have boot-failed) resolves to None.
    // Post-boot, None ⟺ the section is absent ⟺ the feature is off — go inert so
    // we never claim sessions and retry-loop the per-row no-stamp path.
    if state.model_config.resolve_memory_extract().is_none() {
        tracing::info!("memory_extraction not configured — dreaming sweeper inert");
        return;
    }
    tracing::info!(?interval, ?idle, ?claim_stale, "dreaming sweeper starting");

    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        match scan_and_classify(&state, idle, claim_stale).await {
            Ok(0) => {} // quiet — common case on a low-traffic instance
            Ok(n) => tracing::info!(processed = n, "dreaming: sessions classified"),
            Err(e) => tracing::warn!("dreaming scan failed: {e}"),
        }
    }
}

/// Atomically claim eligible sessions and classify each in turn.
///
/// The picker fuses "find idle sessions" and "stamp them as claimed" into
/// a single `UPDATE ... WHERE id IN (SELECT ... FOR UPDATE SKIP LOCKED)
/// RETURNING ...` statement so concurrent sweeper instances can't both
/// pick the same row. The claim is set in this one quick statement — no
/// transaction stays open across the LLM / Voyage HTTP calls that follow.
///
/// Stale claim recovery: rows with `classification_claimed_at < now() -
/// claim_stale` are eligible again, so a worker that crashed mid-pass
/// gets re-tried automatically.
async fn scan_and_classify(
    state: &AppState,
    idle: Duration,
    claim_stale: Duration,
) -> Result<usize, sqlx::Error> {
    let now = Utc::now();
    let cutoff = now - chrono::Duration::from_std(idle).unwrap_or_default();
    let stale_cutoff = now - chrono::Duration::from_std(claim_stale).unwrap_or_default();

    let sessions: Vec<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(
        "UPDATE engine.chat_sessions \
         SET classification_claimed_at = now() \
         WHERE id IN ( \
             SELECT id FROM engine.chat_sessions \
             WHERE classified_at IS NULL \
               AND last_active_at < $1 \
               AND (classification_claimed_at IS NULL \
                    OR classification_claimed_at < $2) \
             ORDER BY last_active_at \
             LIMIT $3 \
             FOR UPDATE SKIP LOCKED \
         ) \
         RETURNING id, user_id, instance_id",
    )
    .bind(cutoff)
    .bind(stale_cutoff)
    .bind(PICK_BATCH)
    .fetch_all(&state.pool)
    .await?;

    let mut count = 0;
    for (session_id, user_id, instance_id) in sessions {
        match classify_session(state, session_id, user_id, instance_id).await {
            Ok(written) => {
                tracing::info!(%session_id, written, "dreaming: session classified");
                count += 1;
            }
            Err(e) => tracing::warn!(%session_id, "dreaming: classify failed: {e}"),
        }
    }
    Ok(count)
}

/// Classify one session. Returns the number of memory rows written.
/// Stamps `classified_at` on a graceful pass (including empty extraction)
/// so the picker doesn't see this session again. Network-level errors
/// propagate and skip the stamp so they retry next tick.
async fn classify_session(
    state: &AppState,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Option<Uuid>,
) -> Result<usize, String> {
    // 1. Pull the conversation log. We use chat_messages (the canonical
    // turn record) rather than the formatted companion_memories rows so
    // the LLM doesn't see the "用户：X\nAI：Y" wrapper twice.
    //
    // Channel filter: text rows (`channel IS NULL`) always; voice rows too
    // unless `DREAMING_VOICE_DISABLED` is set. Eligibility is idle-based, so
    // a voice session only reaches here after the call ended and went quiet
    // for `DREAMING_IDLE_SECS` — this never reads a live call. `product_qa`
    // rows stay excluded: out-of-character product asides are not memories.
    let channel_filter = if state.config.dreaming_voice_disabled {
        "AND channel IS NULL"
    } else {
        "AND (channel IS NULL OR channel = 'voice')"
    };
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(&format!(
        "SELECT role, content, channel FROM engine.chat_messages \
         WHERE session_id = $1 AND role IN ('user', 'assistant') \
           {channel_filter} \
         ORDER BY sent_at"
    ))
    .bind(session_id)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| format!("load chat_messages failed: {e}"))?;

    if rows.is_empty() {
        mark_classified(&state.pool, session_id)
            .await
            .map_err(|e| format!("mark classified (empty session): {e}"))?;
        return Ok(0);
    }

    // Voice assistant rows may carry inline TTS cues when the deployment runs
    // `[tasks.chat_voice] tts_audio_tags = true`. Strip them here — they are
    // stage directions, not memories. User rows and text rows pass through
    // verbatim. A turn that strips to nothing is dropped entirely rather than
    // contributing an empty "AI：" line.
    let turns: Vec<String> = rows
        .into_iter()
        .filter_map(|(role, content, channel)| {
            let is_voice_assistant = role == "assistant" && channel.as_deref() == Some("voice");
            let content = if is_voice_assistant {
                strip_audio_tags(&content)
            } else {
                content
            };
            if content.trim().is_empty() {
                return None;
            }
            let label = if role == "user" { "用户" } else { "AI" };
            Some(format!("{label}：{content}"))
        })
        .collect();

    // Everything stripped away (a call of nothing but audio tags) reduces to
    // the empty-session case: stamp, no LLM call.
    if turns.is_empty() {
        mark_classified(&state.pool, session_id)
            .await
            .map_err(|e| format!("mark classified (empty transcript): {e}"))?;
        return Ok(0);
    }

    // 2. Single LLM call, structured-JSON output. The instruction is the
    // configured system prompt; the conversation is a separate user message.
    let Some(resolved) = state.model_config.resolve_memory_extract() else {
        // Production configs always set memory_extraction.filter_prompt (enforced by
        // the boot gate added in this change set — see main.rs). If somehow unset,
        // skip WITHOUT stamping classified_at so it retries and stays visible.
        return Err("memory_extraction filter_prompt unset".to_string());
    };
    let req = ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: resolved.extract_prompt,
            },
            ChatMessage {
                role: "user".into(),
                content: crate::prompt::memories_user_message(&turns),
            },
        ],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        sampling: resolved.sampling,
        user: Some(SYSTEM_AUDIT_USER.into()),
        reasoning: resolved.reasoning,
        task: Some(MEMORY_TASK.into()),
        ..Default::default()
    };
    let raw = state
        .openrouter
        .execute(req)
        .await
        .map_err(|e| format!("memory_extraction LLM call failed: {e}"))?;

    super::log_openrouter_usage(MEMORY_TASK, Some(session_id), &raw);

    let candidates = parse_memory_candidates(&raw.reply);

    // 3. Embed + insert each candidate as a profile-layer row.
    // Profile layer is the right home for these — they're stable facts
    // about the user that should be visible across persona instances.
    let repo = MemoryRepo { pool: &state.pool };
    let mut written = 0;
    for cand in &candidates {
        let trimmed = cand.content.trim();
        if trimmed.is_empty() {
            continue;
        }
        let category = normalise_category(&cand.category);
        let metadata = candidate_metadata(cand);
        match state.embed.embed_document(trimmed).await {
            Ok(embedding) => {
                if let Err(e) = repo
                    .upsert(
                        MemoryLayer::Profile,
                        session_id,
                        user_id,
                        instance_id,
                        trimmed,
                        &embedding,
                        Some(&category),
                        metadata.as_ref(),
                    )
                    .await
                {
                    tracing::warn!(%session_id, "dreaming: insert failed: {e}");
                } else {
                    written += 1;
                }
            }
            Err(e) => {
                tracing::warn!(%session_id, "dreaming: embed failed: {e}");
            }
        }
    }

    // 4. Stamp on graceful completion. Even when `written == 0` we stamp,
    // because either the session genuinely had nothing memorable or the
    // model returned junk — both cases should not loop the sweeper.
    mark_classified(&state.pool, session_id)
        .await
        .map_err(|e| format!("mark classified (post-success): {e}"))?;
    Ok(written)
}

async fn mark_classified(pool: &sqlx::PgPool, session_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE engine.chat_sessions SET classified_at = now() WHERE id = $1")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

fn parse_memory_candidates(raw: &str) -> Vec<MemoryCandidate> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        return extract_memory_array(&v);
    }
    if let Some(block) = super::find_json_block(raw) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(block) {
            return extract_memory_array(&v);
        }
    }
    vec![]
}

fn extract_memory_array(v: &serde_json::Value) -> Vec<MemoryCandidate> {
    v.get("memories")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| serde_json::from_value::<MemoryCandidate>(x.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Restrict to the documented vocabulary. Anything else collapses to "fact" —
/// the prompt asks for one of these five but the model occasionally
/// invents new categories; we'd rather have a coarse but valid tag than
/// a high-cardinality mess.
fn normalise_category(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        s @ ("fact" | "preference" | "event" | "emotion" | "relation") => s.into(),
        _ => "fact".into(),
    }
}

/// Strip inline TTS audio tags from a voice assistant turn.
///
/// Deployments with `[tasks.chat_voice] tts_audio_tags = true` get replies
/// carrying stage directions — `[laughs]`, `[whispers]`, `[sighs]` — that the
/// TTS layer consumes. They are performance cues, not things the user said or
/// the companion learned, so they never belong in extracted memory text.
///
/// Deliberately conservative: a tag is `[` + a non-empty run of lowercase
/// ASCII letters and single spaces + `]` (the shape `AUDIO_TAGS_ADDENDUM`
/// teaches, including improvised multi-word tags like `[laughs softly]`).
/// `[Laughs]`, `[标签]`, `[3]`, `[]`, and unclosed `[` are left verbatim so
/// literal brackets in real content survive.
///
/// When nothing is stripped the input is returned byte-identical — the
/// whitespace collapse only runs on strings a tag was actually removed from.
fn strip_audio_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut stripped = false;
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let tag = after
            .find(']')
            .map(|close| &after[..close])
            .filter(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_lowercase() || c == ' '));
        match tag {
            Some(t) => {
                out.push_str(&rest[..open]);
                rest = &after[t.len() + 1..];
                stripped = true;
            }
            None => {
                // Not a tag — keep the '[' and continue scanning past it.
                out.push_str(&rest[..=open]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    if !stripped {
        return s.to_string();
    }
    collapse_spaces(&out)
}

/// Collapse runs of ASCII spaces into one and trim the ends. Newlines and
/// other whitespace are preserved — only the gaps a removed tag left behind
/// are closed up.
fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(c);
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::companion::testutil::seed_persona_instance;

    #[test]
    fn parse_memory_candidates_handles_clean_json() {
        let raw = r#"{"memories":[{"content":"住在上海","category":"fact"},
                                  {"content":"喜欢咖啡","category":"preference"}]}"#;
        let cands = parse_memory_candidates(raw);
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].content, "住在上海");
        assert_eq!(cands[0].category, "fact");
        assert_eq!(cands[1].content, "喜欢咖啡");
        assert_eq!(cands[1].category, "preference");
    }

    #[test]
    fn parse_memory_candidates_handles_fenced_block() {
        let raw = "Sure, here you go:\n```json\n\
                   {\"memories\":[{\"content\":\"养了一只猫\",\"category\":\"fact\"}]}\n\
                   ```";
        let cands = parse_memory_candidates(raw);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].content, "养了一只猫");
    }

    #[test]
    fn parse_memory_candidates_returns_empty_on_garbage() {
        assert!(parse_memory_candidates("nope, no json").is_empty());
        assert!(parse_memory_candidates(r#"{"facts":[]}"#).is_empty());
    }

    #[test]
    fn parse_memory_candidates_skips_malformed_items() {
        // Missing `category` on second item — should drop just that one.
        let raw = r#"{"memories":[
            {"content":"a","category":"fact"},
            {"content":"b"},
            {"content":"c","category":"event"}
        ]}"#;
        let cands = parse_memory_candidates(raw);
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].content, "a");
        assert_eq!(cands[1].content, "c");
    }

    #[test]
    fn parse_memory_candidates_captures_extra_keys_as_metadata() {
        let raw = r#"{"memories":[{
            "content": "用户目前正在找工作",
            "category": "fact",
            "domain": "career",
            "evidence_type": "explicit_statement",
            "temporality": "current",
            "persistence": "ongoing",
            "confidence": "high"
        }]}"#;
        let cands = parse_memory_candidates(raw);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].content, "用户目前正在找工作");
        assert_eq!(cands[0].category, "fact");
        let meta = candidate_metadata(&cands[0]).expect("metadata present");
        assert_eq!(meta["domain"], "career");
        assert_eq!(meta["confidence"], "high");
        // Named fields are NOT duplicated into the flattened remainder.
        assert!(meta.get("content").is_none());
        assert!(meta.get("category").is_none());
    }

    #[test]
    fn candidate_metadata_is_none_for_bare_candidates() {
        let raw = r#"{"memories":[{"content":"养了一只猫","category":"fact"}]}"#;
        let cands = parse_memory_candidates(raw);
        assert_eq!(cands.len(), 1);
        assert!(
            candidate_metadata(&cands[0]).is_none(),
            "no extra keys ⇒ None ⇒ NULL column"
        );
    }

    #[test]
    fn normalise_category_passes_known_values() {
        assert_eq!(normalise_category("fact"), "fact");
        assert_eq!(normalise_category("PREFERENCE"), "preference");
        assert_eq!(normalise_category("  Event  "), "event");
        assert_eq!(normalise_category("emotion"), "emotion");
        assert_eq!(normalise_category("relation"), "relation");
    }

    #[test]
    fn normalise_category_collapses_unknowns_to_fact() {
        assert_eq!(normalise_category("opinion"), "fact");
        assert_eq!(normalise_category(""), "fact");
        assert_eq!(normalise_category("分类"), "fact");
    }

    #[test]
    fn strip_audio_tags_removes_a_single_tag() {
        assert_eq!(strip_audio_tags("[laughs] 你好啊"), "你好啊");
        assert_eq!(strip_audio_tags("你好啊 [laughs]"), "你好啊");
    }

    #[test]
    fn strip_audio_tags_removes_interspersed_tags() {
        assert_eq!(
            strip_audio_tags("今天全搞砸了 [sighs] 不想说了…… [giggles] 骗你的啦"),
            "今天全搞砸了 不想说了…… 骗你的啦"
        );
    }

    #[test]
    fn strip_audio_tags_accepts_multi_word_tags() {
        // The addendum explicitly lets the model improvise beyond the listed
        // vocabulary, so `[laughs softly]` must strip too.
        assert_eq!(strip_audio_tags("好啦 [laughs softly] 别闹"), "好啦 别闹");
    }

    #[test]
    fn strip_audio_tags_yields_empty_for_tag_only_content() {
        assert_eq!(strip_audio_tags("[laughs]"), "");
        assert_eq!(strip_audio_tags(" [sighs] [giggles] "), "");
    }

    #[test]
    fn strip_audio_tags_leaves_no_doubled_spaces() {
        assert_eq!(strip_audio_tags("a [sighs] b"), "a b");
        assert_eq!(strip_audio_tags("a[sighs]b"), "ab");
    }

    #[test]
    fn strip_audio_tags_keeps_non_tag_brackets() {
        // Uppercase, CJK, digits, empty, unclosed — none of these are TTS tags.
        assert_eq!(strip_audio_tags("[Laughs] 嗨"), "[Laughs] 嗨");
        assert_eq!(strip_audio_tags("[标签] 嗨"), "[标签] 嗨");
        assert_eq!(strip_audio_tags("第 [3] 章"), "第 [3] 章");
        assert_eq!(strip_audio_tags("空 [] 括号"), "空 [] 括号");
        assert_eq!(strip_audio_tags("[laughs 没关"), "[laughs 没关");
    }

    #[test]
    fn strip_audio_tags_is_byte_identical_without_tags() {
        // No tag ⇒ no whitespace collapsing either: untagged rows must be
        // passed through untouched, doubled spaces and all.
        let s = "我  住在   上海\n真的";
        assert_eq!(strip_audio_tags(s), s);
    }

    #[test]
    fn system_audit_user_is_all_ones_sentinel() {
        assert_eq!(SYSTEM_AUDIT_USER, "11111111-1111-1111-1111-111111111111");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn classify_session_sends_configured_system_prompt(pool: sqlx::PgPool) {
        use wiremock::matchers::{body_string_contains, method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        // Empty memories → no embedding/voyage calls; we only assert the request
        // carried the configured system prompt (routed by a unique sentinel).
        let body = serde_json::json!({
            "id": "gen-mem", "model": "mem/m",
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            "choices": [{"message": {"content": "{\"memories\":[]}"}}],
        });
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_string_contains("memory-sys-prompt-sentinel"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&mock)
            .await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.memory_extraction]\nmodel=\"mem/m\"\nfilter_prompt=\"memory-sys-prompt-sentinel\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let user_id = uuid::Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', '我在深圳工作'), ($1, 'assistant', '嗯嗯')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let written = classify_session(&state, session_id, user_id, Some(instance_id))
            .await
            .expect("classify ok");
        assert_eq!(written, 0, "empty memories → no rows");
        // mock.expect(1) is verified on MockServer drop: the request carried the
        // configured system prompt sentinel.
    }

    /// Spec 2026-08-09-voice-dreaming-ingestion §1: voice rows now reach the
    /// extraction transcript. This test REPLACES the old
    /// `classify_session_excludes_voice_rows` lock — the inversion is
    /// spec-mandated, not a weakened assertion. The boundary that must not
    /// move is pinned by `classify_session_excludes_product_qa_rows` below.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn classify_session_includes_voice_rows(pool: sqlx::PgPool) {
        let (mock, state, session_id, user_id, instance_id) = seed_channel_case(&pool).await;

        let _ = classify_session(&state, session_id, user_id, Some(instance_id)).await;

        let joined = joined_request_bodies(&mock).await;
        assert!(joined.contains("TEXTLINE"), "text turn must be extracted");
        assert!(
            joined.contains("VOICELINE"),
            "voice turn must now be extracted (spec §1 flip)"
        );
    }

    /// The boundary that must NOT move: `product_qa` is an out-of-character
    /// product aside, never companion memory.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn classify_session_excludes_product_qa_rows(pool: sqlx::PgPool) {
        let (mock, state, session_id, user_id, instance_id) = seed_channel_case(&pool).await;

        let _ = classify_session(&state, session_id, user_id, Some(instance_id)).await;

        let joined = joined_request_bodies(&mock).await;
        assert!(joined.contains("TEXTLINE"), "text turn must be extracted");
        assert!(
            !joined.contains("QALINE"),
            "product_qa turn must stay excluded from memory extraction"
        );
    }

    /// `DREAMING_VOICE_DISABLED=1` restores the pre-flip filter.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn classify_session_excludes_voice_rows_when_flag_set(pool: sqlx::PgPool) {
        let (mock, mut state, session_id, user_id, instance_id) = seed_channel_case(&pool).await;
        state.config.dreaming_voice_disabled = true;

        let _ = classify_session(&state, session_id, user_id, Some(instance_id)).await;

        let joined = joined_request_bodies(&mock).await;
        assert!(
            joined.contains("TEXTLINE"),
            "text turn must still be extracted"
        );
        assert!(
            !joined.contains("VOICELINE"),
            "flag on ⇒ voice turn excluded again"
        );
    }

    /// Seed one session carrying a text turn, a voice turn, and a product_qa
    /// turn, plus a state whose OpenRouter points at a mock returning an empty
    /// memory list (so no embedding call is ever made).
    async fn seed_channel_case(
        pool: &sqlx::PgPool,
    ) -> (
        wiremock::MockServer,
        crate::state::AppState,
        Uuid,
        Uuid,
        Uuid,
    ) {
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "{\"memories\":[]}" } }],
                "id": "gen-x", "model": "primary"
            })))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(pool, user_id).await;
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1,$2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel) VALUES \
             ($1,'user','TEXTLINE',NULL), \
             ($1,'assistant','VOICELINE','voice'), \
             ($1,'assistant','QALINE','product_qa')",
        )
        .bind(session_id)
        .execute(pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.memory_extraction]\nmodel = \"primary\"\nfilter_prompt = \"extract\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        (mock, state, session_id, user_id, instance_id)
    }

    /// Every request body the mock saw, concatenated — enough to assert which
    /// sentinel lines made it into the extraction prompt.
    async fn joined_request_bodies(mock: &wiremock::MockServer) -> String {
        mock.received_requests()
            .await
            .expect("recording enabled by default")
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect()
    }

    /// Spec §4: bracketed TTS cues on voice assistant rows never reach the
    /// extraction prompt; user rows and non-voice rows are untouched.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn classify_session_strips_audio_tags_from_voice_assistant_rows(pool: sqlx::PgPool) {
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "{\"memories\":[]}" } }],
                "id": "gen-x", "model": "primary"
            })))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1,$2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel) VALUES \
             ($1,'user','我 [laughs] 住在上海','voice'), \
             ($1,'assistant','[giggles] 上海真好 [sighs] 我也想去','voice'), \
             ($1,'assistant','[chuckles] 文字轮次',NULL)",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.memory_extraction]\nmodel = \"primary\"\nfilter_prompt = \"extract\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let _ = classify_session(&state, session_id, user_id, Some(instance_id)).await;

        let joined = joined_request_bodies(&mock).await;
        assert!(
            joined.contains("上海真好 我也想去"),
            "voice assistant row must arrive tag-free and space-collapsed; got {joined}"
        );
        assert!(
            !joined.contains("giggles"),
            "no audio tag may survive on the voice assistant row"
        );
        assert!(
            joined.contains("我 [laughs] 住在上海"),
            "user rows are never stripped"
        );
        assert!(
            joined.contains("[chuckles] 文字轮次"),
            "non-voice assistant rows are never stripped"
        );
    }

    /// A call whose every turn is tag-only leaves nothing to extract: stamp,
    /// no LLM call — the same path a genuinely empty session takes.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn classify_session_stamps_without_llm_when_all_turns_strip_empty(pool: sqlx::PgPool) {
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "{\"memories\":[]}" } }],
                "id": "gen-x", "model": "primary"
            })))
            .mount(&mock)
            .await;

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1,$2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel) VALUES \
             ($1,'assistant','[laughs]','voice'), ($1,'assistant','[sighs] [giggles]','voice')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .unwrap();

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.memory_extraction]\nmodel = \"primary\"\nfilter_prompt = \"extract\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );

        let written = classify_session(&state, session_id, user_id, Some(instance_id))
            .await
            .expect("classify ok");
        assert_eq!(written, 0);
        assert!(
            joined_request_bodies(&mock).await.is_empty(),
            "nothing left to extract ⇒ no LLM call"
        );
        let stamped: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT classified_at FROM engine.chat_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(stamped.is_some(), "session must still be stamped");
    }

    /// 512-dim unit vector — matches the column dimension the migrations set.
    fn unit_embedding(seed: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; 512];
        v[seed % 512] = 1.0;
        v
    }

    /// An `EmbeddingRouter` whose backend is a wiremock server, built through
    /// the production `from_config_with` path (custom provider + its
    /// `<NAME>_API_KEY`). Mirrors the helper in `pipeline::voice`'s tests.
    async fn embed_router_mock() -> (
        wiremock::MockServer,
        eros_engine_llm::embedding::EmbeddingRouter,
    ) {
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(wm_path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": [ { "embedding": unit_embedding(1) } ] }),
            ))
            .mount(&server)
            .await;
        let cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(&format!(
            "[providers.mockembed]\nembeddings = \"{}/v1/embeddings\"\n\
             [tasks.embedding]\nmodel = \"mock-embed@mockembed\"\n",
            server.uri()
        ))
        .expect("embedding provider config parses");
        let router = eros_engine_llm::embedding::EmbeddingRouter::from_config_with(&cfg, |k| {
            (k == "MOCKEMBED_API_KEY").then(|| "test-key".to_string())
        })
        .expect("mock embedding router builds");
        (server, router)
    }

    /// Full path: an ended (idle) voice call becomes categorized profile-layer
    /// memories and is stamped; a call that is still warm is not swept.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn sweeper_ingests_idle_voice_session_and_skips_active_one(pool: sqlx::PgPool) {
        use wiremock::matchers::path as wm_path;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let llm = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content":
                    "{\"memories\":[{\"content\":\"用户住在上海\",\"category\":\"fact\"}]}" } }],
                "id": "gen-e2e", "model": "primary"
            })))
            .mount(&llm)
            .await;
        let (_embed_server, embed) = embed_router_mock().await;

        let mut state = crate::routes::companion::test_state(pool.clone());
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.memory_extraction]\nmodel = \"primary\"\nfilter_prompt = \"extract\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", llm.uri()),
            ),
        );
        state.embed = std::sync::Arc::new(embed);

        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, user_id).await;

        // Ended call: last_active_at two hours ago.
        let ended: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel, last_active_at) \
             VALUES ($1,$2,'voice', now() - interval '2 hours') RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Live call: active right now.
        let live: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel, last_active_at) \
             VALUES ($1,$2,'voice', now()) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        for sid in [ended, live] {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content, channel) VALUES \
                 ($1,'user','我住在上海','voice'), ($1,'assistant','[giggles] 上海不错','voice')",
            )
            .bind(sid)
            .execute(&pool)
            .await
            .unwrap();
        }

        let processed = scan_and_classify(
            &state,
            std::time::Duration::from_secs(1800),
            std::time::Duration::from_secs(600),
        )
        .await
        .expect("sweep ok");
        assert_eq!(processed, 1, "only the ended call is eligible");

        // Profile layer == `instance_id IS NULL` (there is no `layer` column;
        // see `MemoryRepo::upsert`, which forces instance_id to NULL for
        // MemoryLayer::Profile).
        let rows: Vec<(String, Option<String>, Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT content, category, session_id, instance_id \
             FROM engine.companion_memories WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "one extracted candidate ⇒ one memory row");
        assert_eq!(rows[0].0, "用户住在上海");
        assert_eq!(rows[0].1.as_deref(), Some("fact"));
        assert_eq!(rows[0].2, ended, "memory is attributed to the ended call");
        assert!(rows[0].3.is_none(), "profile layer ⇒ instance_id NULL");

        let ended_stamp: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT classified_at FROM engine.chat_sessions WHERE id = $1")
                .bind(ended)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(ended_stamp.is_some(), "ended call stamped");
        let live_stamp: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT classified_at FROM engine.chat_sessions WHERE id = $1")
                .bind(live)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(live_stamp.is_none(), "live call must not be swept mid-call");
    }
}

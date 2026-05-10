// SPDX-License-Identifier: AGPL-3.0-only
//! Dreaming-lite: session-end memory extraction sweeper.
//!
//! Background tokio task that scans `engine.chat_sessions` for idle,
//! unclassified sessions and runs the `memory_extraction` LLM task on
//! their `chat_messages`. Extracted candidates with category tags are
//! written to `engine.companion_memories` as profile-layer rows; the
//! session's `classified_at` is then stamped to suppress re-sweeps.
//!
//! Single-instance assumption: with multiple replicas the picker query
//! would need `FOR UPDATE SKIP LOCKED` to avoid double-classifying the
//! same session. OSS v1 ships single-instance.
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
const PICK_BATCH: i64 = 10;

#[derive(Debug, Deserialize)]
struct MemoryCandidate {
    content: String,
    category: String,
}

/// Run forever. Spawn this once at server startup. Returns immediately
/// (and never spawns the loop) if `state.config.dreaming_tick` is zero.
pub async fn sweeper(state: AppState) {
    let interval = state.config.dreaming_tick;
    let idle = state.config.dreaming_idle_threshold;
    if interval.is_zero() {
        tracing::info!("dreaming sweeper disabled (DREAMING_DISABLED=1 or tick=0)");
        return;
    }
    tracing::info!(?interval, ?idle, "dreaming sweeper starting");

    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        match scan_and_classify(&state, idle).await {
            Ok(0) => {} // quiet — common case on a low-traffic instance
            Ok(n) => tracing::info!(processed = n, "dreaming: sessions classified"),
            Err(e) => tracing::warn!("dreaming scan failed: {e}"),
        }
    }
}

/// One sweep tick: pick eligible sessions, classify each in turn.
async fn scan_and_classify(state: &AppState, idle: Duration) -> Result<usize, sqlx::Error> {
    let cutoff = Utc::now() - chrono::Duration::from_std(idle).unwrap_or_default();
    let sessions: Vec<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, user_id, instance_id FROM engine.chat_sessions \
         WHERE classified_at IS NULL AND last_active_at < $1 \
         ORDER BY last_active_at \
         LIMIT $2",
    )
    .bind(cutoff)
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
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM engine.chat_messages \
         WHERE session_id = $1 AND role IN ('user', 'assistant') \
         ORDER BY sent_at",
    )
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

    let turns: Vec<String> = rows
        .into_iter()
        .map(|(role, content)| {
            let label = if role == "user" { "用户" } else { "AI" };
            format!("{label}：{content}")
        })
        .collect();

    // 2. Single LLM call, structured-JSON output.
    let prompt = crate::prompt::extract_memories_prompt(&turns);
    let resolved = state.model_config.resolve(MEMORY_TASK, None);
    let req = ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages: vec![ChatMessage {
            role: "user".into(),
            content: prompt,
        }],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
    };
    let raw = state
        .openrouter
        .execute(req)
        .await
        .map_err(|e| format!("memory_extraction LLM call failed: {e}"))?;

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
        match state.voyage.embed_document(trimmed).await {
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
                    )
                    .await
                {
                    tracing::warn!(%session_id, "dreaming: insert failed: {e}");
                } else {
                    written += 1;
                }
            }
            Err(e) => {
                tracing::warn!(%session_id, "dreaming: voyage embed failed: {e}");
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

/// Walk forward from the first `{` and return the substring up to its
/// balanced `}`, ignoring braces inside string literals. Mirrors the
/// helper in `post_process.rs`; kept private to this module so the
/// extraction-vs-classification parsing stays decoupled.
fn find_json_block(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_memory_candidates(raw: &str) -> Vec<MemoryCandidate> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        return extract_memory_array(&v);
    }
    if let Some(block) = find_json_block(raw) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn find_json_block_balanced_with_string_braces() {
        let raw = r#"prefix {"a": "b}c", "d": 1} trailing"#;
        let block = find_json_block(raw).unwrap();
        let v: serde_json::Value = serde_json::from_str(block).unwrap();
        assert_eq!(v["a"], "b}c");
        assert_eq!(v["d"], 1);
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Dreaming-plus: the relationship scanner's triggers and its sweeper.
//!
//! Every 1:1 user turn counts toward its relationship's scan
//! (`engine.relationship_scan_state`): the text turn in post-process, the
//! voice turn in its route. The turn that creates the relationship's row also
//! journals the first meeting. A text turn that brings `SCAN_K` pending turns
//! scans inline; the sweeper scans relationships that went idle with at least
//! `FLUSH_MIN_TURNS` pending, after embedding the journal rows still waiting
//! for a vector. Both trigger paths claim the relationship first, so two
//! workers never scan one relationship at once; `scan::scan_instance` settles
//! the claim.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use uuid::Uuid;

use eros_engine_store::journal::{first_meet, local_date, JournalRepo};
use eros_engine_store::scan_state::{Bumped, ScanStateRepo, SCAN_K};

use crate::state::AppState;

mod parse;
mod render;
mod scan;

use scan::{scan_instance, ScanOutcome};

/// Sentinel OpenRouter `user` for system-initiated (non-user-triggered) LLM
/// calls. Dreaming runs in the background sweeper with no live request, so
/// per-user attribution is meaningless; this buckets all dreaming spend
/// under one id. All-ones (not the `0…1` nil-plus-one, which is already used
/// in our database). Not a real auth UUID (v4) and not a hashed client id,
/// so it cannot collide with a real user.
pub(crate) const SYSTEM_AUDIT_USER: &str = "11111111-1111-1111-1111-111111111111";
/// Relationships one sweeper tick claims.
const PICK_BATCH: i64 = 10;
/// Journal rows one sweeper tick embeds.
const EMBED_BATCH: i64 = 100;

#[derive(Debug, Deserialize)]
pub(crate) struct MemoryCandidate {
    pub(crate) content: String,
    pub(crate) category: String,
    /// Everything else the extraction prompt emitted per memory (domain /
    /// evidence_type / temporality / persistence / confidence, plus whatever
    /// future prompts add). Captured opaquely — the engine never validates
    /// the vocabulary; it lands verbatim in companion_memories.metadata.
    #[serde(flatten)]
    pub(crate) metadata: serde_json::Map<String, serde_json::Value>,
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

/// Count one 1:1 user turn toward its relationship's scan. Reads the user
/// row's `sent_at` (no row in a live session, no count: an archive that
/// landed since the turn is not undone) and, when the bump creates the
/// relationship's row, journals the first meeting: dated by the pair's
/// earliest chat row, else by this row, in `user_timezone` (UTC when none).
/// `None` when nothing was counted; failures are logged, never raised.
pub(crate) async fn bump_relationship(
    state: &AppState,
    instance_id: Uuid,
    user_message_id: Uuid,
    user_timezone: Option<Tz>,
) -> Option<Bumped> {
    let pool = &state.pool;
    let sent_at: Option<DateTime<Utc>> = match sqlx::query_scalar(
        "SELECT m.sent_at FROM engine.chat_messages m \
             JOIN engine.chat_sessions s ON s.id = m.session_id \
             WHERE m.id = $1 AND NOT s.archived",
    )
    .bind(user_message_id)
    .fetch_optional(pool)
    .await
    {
        Ok(at) => at,
        Err(e) => {
            tracing::warn!(%instance_id, "dreaming: user row read failed: {e}");
            return None;
        }
    };
    let sent_at = sent_at?;
    let bumped = match (ScanStateRepo { pool })
        .bump(instance_id, sent_at, user_timezone.map(|tz| tz.name()))
        .await
    {
        Ok(bumped) => bumped,
        Err(e) => {
            tracing::warn!(%instance_id, "dreaming: turn count failed: {e}");
            return None;
        }
    };
    if bumped.created {
        let journal = JournalRepo { pool };
        let on = match journal.first_meet_date(instance_id, user_timezone).await {
            Ok(on) => on,
            Err(e) => {
                tracing::warn!(%instance_id, "dreaming: first-meeting date read failed: {e}");
                None
            }
        }
        .unwrap_or_else(|| local_date(sent_at, user_timezone));
        if let Err(e) = journal.insert(&first_meet(instance_id, on)).await {
            tracing::warn!(%instance_id, "dreaming: first-meeting journal write failed: {e}");
        }
    }
    Some(bumped)
}

/// A 1:1 text turn: counted, and once `SCAN_K` turns wait, the relationship is
/// claimed and scanned inline up to the turn's newest row (`produced` are its
/// assistant rows), so a later turn in flight is never half-read. Inert past
/// the count while `[tasks.dreaming_scan]` is absent.
pub(crate) async fn count_text_turn(
    state: &AppState,
    instance_id: Uuid,
    user_message_id: Uuid,
    produced: &[Uuid],
    user_timezone: Option<Tz>,
) {
    let Some(bumped) = bump_relationship(state, instance_id, user_message_id, user_timezone).await
    else {
        return;
    };
    if bumped.pending_turns < SCAN_K || state.model_config.resolve_dreaming_scan().is_none() {
        return;
    }
    let pool = &state.pool;
    let ids: Vec<Uuid> = std::iter::once(user_message_id)
        .chain(produced.iter().copied())
        .collect();
    let bound: Option<DateTime<Utc>> = match sqlx::query_scalar(
        "SELECT max(sent_at) FROM engine.chat_messages WHERE id = ANY($1)",
    )
    .bind(&ids)
    .fetch_one(pool)
    .await
    {
        Ok(bound) => bound,
        Err(e) => {
            tracing::warn!(%instance_id, "dreaming: scan bound read failed: {e}");
            return;
        }
    };
    let Some(bound) = bound else {
        return;
    };
    match (ScanStateRepo { pool })
        .claim(instance_id, state.config.dreaming_claim_stale_threshold)
        .await
    {
        Ok(true) => {
            scan_instance(state, instance_id, bound, "scan").await;
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(%instance_id, "dreaming: scan claim failed: {e}"),
    }
}

/// Run forever. Spawn this once at server startup. Returns at once when
/// `state.config.dreaming_tick` is zero or `[tasks.dreaming_scan]` is absent.
pub async fn sweeper(state: AppState) {
    let interval = state.config.dreaming_tick;
    if interval.is_zero() {
        tracing::info!("dreaming sweeper disabled (DREAMING_DISABLED=1 or tick=0)");
        return;
    }
    // dreaming_scan is omittable: a missing [tasks.dreaming_scan] section (or
    // a blank filter_prompt, which would have boot-failed) resolves to None,
    // and the feature is off.
    if state.model_config.resolve_dreaming_scan().is_none() {
        tracing::info!("dreaming_scan not configured — dreaming sweeper inert");
        return;
    }
    tracing::info!(
        ?interval,
        idle = ?state.config.dreaming_idle_threshold,
        claim_stale = ?state.config.dreaming_claim_stale_threshold,
        "dreaming sweeper starting"
    );

    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let t = sweep_once(&state).await;
        if t != SweepTally::default() {
            tracing::info!(
                embedded = t.embedded,
                ok = t.ok,
                empty = t.empty,
                failed = t.failed,
                skipped = t.skipped,
                "dreaming: relationships swept"
            );
        }
    }
}

/// What one sweeper tick did: journal rows embedded, and the claimed
/// relationships by scan outcome (`skipped` lost their claim before the scan).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SweepTally {
    pub(crate) embedded: usize,
    pub(crate) ok: u32,
    pub(crate) empty: u32,
    pub(crate) failed: u32,
    pub(crate) skipped: u32,
}

/// One sweeper tick: embed the journal rows waiting for a vector, then claim
/// the relationships gone idle with turns pending and scan each up to now.
///
/// The whole batch is claimed at once, so each claim is refreshed just before
/// its scan; a relationship whose claim is gone (erased, or released) is
/// skipped without a model call.
pub(crate) async fn sweep_once(state: &AppState) -> SweepTally {
    let mut tally = SweepTally {
        embedded: embed_pending_journal(state).await,
        ..Default::default()
    };
    let repo = ScanStateRepo { pool: &state.pool };
    let claimed = match repo
        .claim_due(
            state.config.dreaming_idle_threshold,
            state.config.dreaming_claim_stale_threshold,
            PICK_BATCH,
        )
        .await
    {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("dreaming: claim failed: {e}");
            return tally;
        }
    };
    for instance_id in claimed {
        match repo.refresh_claim(instance_id).await {
            Ok(true) => {}
            Ok(false) => {
                tally.skipped += 1;
                continue;
            }
            Err(e) => {
                // The claim stays as it was and goes stale into a later tick.
                tracing::warn!(%instance_id, "dreaming: refreshing the claim failed: {e}");
                tally.skipped += 1;
                continue;
            }
        }
        match scan_instance(state, instance_id, Utc::now(), "scan").await {
            ScanOutcome::Ok => tally.ok += 1,
            ScanOutcome::Empty => tally.empty += 1,
            ScanOutcome::Failed => tally.failed += 1,
        }
    }
    tally
}

/// Embed up to `EMBED_BATCH` journal rows still without a vector in one
/// `embed_documents` call. Returns how many were stored; a failure is logged
/// and the rows wait for the next tick.
async fn embed_pending_journal(state: &AppState) -> usize {
    let journal = JournalRepo { pool: &state.pool };
    let pending = match journal.pending_embeddings(EMBED_BATCH).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("dreaming: pending journal read failed: {e}");
            return 0;
        }
    };
    if pending.is_empty() {
        return 0;
    }
    let texts: Vec<&str> = pending.iter().map(|(_, s)| s.as_str()).collect();
    let vectors = match state.embed.embed_documents(&texts).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(rows = pending.len(), "dreaming: journal embed failed: {e}");
            return 0;
        }
    };
    let rows: Vec<(Uuid, Vec<f32>)> = pending.iter().map(|(id, _)| *id).zip(vectors).collect();
    match journal.set_embeddings(&rows).await {
        Ok(()) => rows.len(),
        Err(e) => {
            tracing::warn!(
                rows = rows.len(),
                "dreaming: journal vectors write failed: {e}"
            );
            0
        }
    }
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
    use eros_engine_store::journal::JournalInsert;
    use serde_json::json;
    use sqlx::PgPool;
    use std::time::Duration;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn memories(raw: &str) -> Vec<MemoryCandidate> {
        extract_memory_array(&serde_json::from_str(raw).unwrap())
    }

    #[test]
    fn extract_memory_array_reads_clean_json() {
        let cands = memories(
            r#"{"memories":[{"content":"住在上海","category":"fact"},
                            {"content":"喜欢咖啡","category":"preference"}]}"#,
        );
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].content, "住在上海");
        assert_eq!(cands[0].category, "fact");
        assert_eq!(cands[1].content, "喜欢咖啡");
        assert_eq!(cands[1].category, "preference");
        assert!(memories(r#"{"facts":[]}"#).is_empty());
    }

    #[test]
    fn extract_memory_array_skips_malformed_items() {
        // Missing `category` on second item — should drop just that one.
        let cands = memories(
            r#"{"memories":[
                {"content":"a","category":"fact"},
                {"content":"b"},
                {"content":"c","category":"event"}
            ]}"#,
        );
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].content, "a");
        assert_eq!(cands[1].content, "c");
    }

    #[test]
    fn extract_memory_array_captures_extra_keys_as_metadata() {
        let cands = memories(
            r#"{"memories":[{
                "content": "用户目前正在找工作",
                "category": "fact",
                "domain": "career",
                "evidence_type": "explicit_statement",
                "temporality": "current",
                "persistence": "ongoing",
                "confidence": "high"
            }]}"#,
        );
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
        let cands = memories(r#"{"memories":[{"content":"养了一只猫","category":"fact"}]}"#);
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

    // ─── Relationship triggers and the sweeper ───────────────────────

    const SCAN_MODEL: &str = "scan/m";

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// 512-dim unit vector — matches the column dimension the migrations set.
    fn unit_embedding() -> Vec<f32> {
        let mut v = vec![0.0_f32; 512];
        v[1] = 1.0;
        v
    }

    /// One vector per input, whatever the batch size.
    struct VectorPerInput;

    impl Respond for VectorPerInput {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let n = serde_json::from_slice::<serde_json::Value>(&req.body)
                .ok()
                .and_then(|v| v["input"].as_array().map(Vec::len))
                .unwrap_or(1);
            let data: Vec<_> = (0..n)
                .map(|_| json!({ "embedding": unit_embedding() }))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "data": data }))
        }
    }

    /// A state whose chat calls go to `llm` and whose embeddings go to the
    /// returned mock. `[tasks.dreaming_scan]` is configured unless `scan` is
    /// false.
    async fn state_with(pool: PgPool, llm: &MockServer, scan: bool) -> (AppState, MockServer) {
        let embed = MockServer::start().await;
        Mock::given(wm_path("/v1/embeddings"))
            .respond_with(VectorPerInput)
            .mount(&embed)
            .await;
        let embed_cfg = eros_engine_llm::model_config::ModelConfig::from_toml_str(&format!(
            "[providers.mockembed]\nembeddings = \"{}/v1/embeddings\"\n\
             [tasks.embedding]\nmodel = \"mock-embed@mockembed\"\n",
            embed.uri()
        ))
        .unwrap();
        let router =
            eros_engine_llm::embedding::EmbeddingRouter::from_config_with(&embed_cfg, |k| {
                (k == "MOCKEMBED_API_KEY").then(|| "test-key".to_string())
            })
            .unwrap();

        let mut state = crate::routes::companion::test_state(pool);
        let toml = if scan {
            format!(
                "[tasks.dreaming_scan]\nmodel = \"{SCAN_MODEL}\"\nfilter_prompt = \"extract\"\n"
            )
        } else {
            String::new()
        };
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(&toml).unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", llm.uri()),
            ),
        );
        state.embed = std::sync::Arc::new(router);
        (state, embed)
    }

    /// Every chat call on `llm` answers `reply`.
    async fn answer(llm: &MockServer, reply: &str) {
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "gen-scan", "model": SCAN_MODEL,
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                "choices": [{ "message": { "content": reply } }],
            })))
            .mount(llm)
            .await;
    }

    async fn request_bodies(mock: &MockServer) -> Vec<String> {
        mock.received_requests()
            .await
            .expect("recording enabled by default")
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect()
    }

    async fn new_session(pool: &PgPool, owner: Uuid, instance: Uuid, channel: &str) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(owner)
        .bind(instance)
        .bind(channel)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn insert_row(
        pool: &PgPool,
        session: Uuid,
        role: &str,
        content: &str,
        channel: Option<&str>,
        sent_at: &str,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel, sent_at) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(session)
        .bind(role)
        .bind(content)
        .bind(channel)
        .bind(at(sent_at))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn idle_for_two_hours(pool: &PgPool, instance: Uuid) {
        sqlx::query(
            "UPDATE engine.relationship_scan_state \
             SET last_turn_at = now() - interval '2 hours' WHERE instance_id = $1",
        )
        .bind(instance)
        .execute(pool)
        .await
        .unwrap();
    }

    /// One relationship's voice call: three user turns, each answered, every
    /// turn counted through the voice path's bump.
    async fn voice_call(pool: &PgPool, state: &AppState) -> (Uuid, Uuid, Uuid) {
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(pool, owner).await;
        let session = new_session(pool, owner, instance, "voice").await;
        for (user_at, reply_at) in [
            ("2026-10-01T10:00:00Z", "2026-10-01T10:00:05Z"),
            ("2026-10-01T10:01:00Z", "2026-10-01T10:01:05Z"),
            ("2026-10-01T10:02:00Z", "2026-10-01T10:02:05Z"),
        ] {
            let id = insert_row(pool, session, "user", "我住在上海", Some("voice"), user_at).await;
            insert_row(
                pool,
                session,
                "assistant",
                "[giggles] 上海不错",
                Some("voice"),
                reply_at,
            )
            .await;
            bump_relationship(state, instance, id, None)
                .await
                .expect("the voice turn is counted");
        }
        (owner, instance, session)
    }

    /// A turn whose session was archived before its count ran stays
    /// uncounted: the archive cleared the relationship's scan state and
    /// journal, and the count must not grow them back.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn bump_skips_a_turn_whose_session_was_archived(pool: PgPool) {
        let llm = MockServer::start().await;
        let (state, _embed) = state_with(pool.clone(), &llm, true).await;
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, owner).await;
        let session = new_session(&pool, owner, instance, "text").await;
        let user = insert_row(&pool, session, "user", "嗨", None, "2026-10-01T10:00:00Z").await;
        sqlx::query("UPDATE engine.chat_sessions SET archived = true WHERE id = $1")
            .bind(session)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(bump_relationship(&state, instance, user, None).await, None);
        let rows: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM engine.relationship_scan_state) \
                  + (SELECT count(*) FROM engine.relationship_journal)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rows, 0);
    }

    /// Full path: a voice call that went quiet becomes profile-layer memories
    /// and an episode; a relationship still talking is not swept.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn sweeper_ingests_idle_voice_session_and_skips_active_one(pool: PgPool) {
        let llm = MockServer::start().await;
        answer(
            &llm,
            r#"{"memories":[{"content":"用户住在上海","category":"fact"}],
                "episode":"用户在通话里说起自己住在上海。"}"#,
        )
        .await;
        let (state, _embed) = state_with(pool.clone(), &llm, true).await;
        let (owner, ended, ended_session) = voice_call(&pool, &state).await;
        let (_, live, _) = voice_call(&pool, &state).await;
        idle_for_two_hours(&pool, ended).await;

        let tally = sweep_once(&state).await;
        assert_eq!(
            tally,
            SweepTally {
                embedded: 2,
                ok: 1,
                ..Default::default()
            },
            "both first meetings embedded; only the ended call is due"
        );
        assert_eq!(request_bodies(&llm).await.len(), 1);

        // Profile layer == `instance_id IS NULL`.
        let rows: Vec<(String, Option<String>, Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT content, category, session_id, instance_id \
             FROM engine.companion_memories WHERE user_id = $1",
        )
        .bind(owner)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![(
                "用户住在上海".to_string(),
                Some("fact".to_string()),
                ended_session,
                None
            )],
            "one extracted candidate ⇒ one profile row on the call's session"
        );
        let episodes: Vec<Uuid> =
            sqlx::query_scalar("SELECT instance_id FROM engine.relationship_episodes")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(episodes, vec![ended]);

        let scan = ScanStateRepo { pool: &pool };
        let s = scan.load(ended).await.unwrap().unwrap();
        assert_eq!((s.pending_turns, s.claimed_at), (0, None));
        assert_eq!(s.scanned_through_at, at("2026-10-01T10:02:05Z"));
        let s = scan.load(live).await.unwrap().unwrap();
        assert_eq!(
            (s.pending_turns, s.claimed_at, s.last_scanned_at),
            (3, None, None),
            "a live call must not be swept mid-call"
        );
    }

    /// One relationship holding a text session (a user turn and a product
    /// Q&A aside) and a voice session (a tagged user turn and a tagged
    /// reply), counted once and claimed.
    async fn channel_case(pool: &PgPool) -> Uuid {
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(pool, owner).await;
        let text = new_session(pool, owner, instance, "text").await;
        let voice = new_session(pool, owner, instance, "voice").await;
        let first = "2026-10-01T10:00:00Z";
        insert_row(pool, text, "user", "TEXTLINE", None, first).await;
        insert_row(
            pool,
            text,
            "assistant",
            "QALINE",
            Some("product_qa"),
            "2026-10-01T10:01:00Z",
        )
        .await;
        insert_row(
            pool,
            voice,
            "user",
            "我 [laughs] VOICELINE",
            Some("voice"),
            "2026-10-01T11:00:00Z",
        )
        .await;
        insert_row(
            pool,
            voice,
            "assistant",
            "[giggles] VOICEREPLY",
            Some("voice"),
            "2026-10-01T11:00:05Z",
        )
        .await;
        let scan = ScanStateRepo { pool };
        scan.bump(instance, at(first), None).await.unwrap();
        assert!(scan
            .claim(instance, Duration::from_secs(600))
            .await
            .unwrap());
        instance
    }

    /// Voice rows reach the scan transcript unless `DREAMING_VOICE_DISABLED`;
    /// product Q&A never does. Voice assistant rows lose their TTS tags, user
    /// rows are never stripped.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn segment_includes_voice_rows_unless_disabled(pool: PgPool) {
        for voice_disabled in [false, true] {
            let llm = MockServer::start().await;
            answer(&llm, r#"{"memories":[]}"#).await;
            let (mut state, _embed) = state_with(pool.clone(), &llm, true).await;
            state.config.dreaming_voice_disabled = voice_disabled;
            let instance = channel_case(&pool).await;

            scan_instance(&state, instance, at("2026-10-02T00:00:00Z"), "scan").await;

            let bodies = request_bodies(&llm).await;
            assert_eq!(bodies.len(), 1);
            let body = &bodies[0];
            assert!(body.contains("TEXTLINE"), "text turn is read");
            assert!(!body.contains("QALINE"), "product_qa stays out");
            if voice_disabled {
                assert!(
                    !body.contains("VOICELINE") && !body.contains("VOICEREPLY"),
                    "flag on ⇒ voice rows stay out: {body}"
                );
            } else {
                assert!(
                    body.contains("我 [laughs] VOICELINE"),
                    "voice user row read verbatim: {body}"
                );
                assert!(
                    body.contains("AI：VOICEREPLY") && !body.contains("giggles"),
                    "voice reply read without its audio tag: {body}"
                );
            }
        }
    }

    /// A stretch of nothing but audio tags leaves nothing to read: the cursor
    /// moves past it without a model call.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn tag_only_segment_advances_without_a_call(pool: PgPool) {
        let llm = MockServer::start().await;
        answer(&llm, r#"{"memories":[]}"#).await;
        let (state, _embed) = state_with(pool.clone(), &llm, true).await;
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, owner).await;
        let voice = new_session(&pool, owner, instance, "voice").await;
        let (first, last) = ("2026-10-01T10:00:00Z", "2026-10-01T10:00:05Z");
        insert_row(&pool, voice, "assistant", "[laughs]", Some("voice"), first).await;
        insert_row(
            &pool,
            voice,
            "assistant",
            "[sighs] [giggles]",
            Some("voice"),
            last,
        )
        .await;
        let scan = ScanStateRepo { pool: &pool };
        scan.bump(instance, at(first), None).await.unwrap();
        assert!(scan
            .claim(instance, Duration::from_secs(600))
            .await
            .unwrap());

        let outcome = scan_instance(&state, instance, at("2026-10-02T00:00:00Z"), "scan").await;
        assert_eq!(outcome, ScanOutcome::Empty);
        assert!(request_bodies(&llm).await.is_empty(), "no call to spend");
        let s = scan.load(instance).await.unwrap().unwrap();
        assert_eq!((s.scanned_through_at, s.claimed_at), (at(last), None));
    }

    /// Rows of an archived session and of a group chat never reach a scan,
    /// even when they sit inside the relationship's unread stretch.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn sweeper_never_scans_archived_or_group_rows(pool: PgPool) {
        let llm = MockServer::start().await;
        answer(&llm, r#"{"memories":[]}"#).await;
        let (state, _embed) = state_with(pool.clone(), &llm, true).await;
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, owner).await;
        let live = new_session(&pool, owner, instance, "text").await;
        let archived = new_session(&pool, owner, instance, "text").await;
        sqlx::query("UPDATE engine.chat_sessions SET archived = true WHERE id = $1")
            .bind(archived)
            .execute(&pool)
            .await
            .unwrap();
        let group: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, channel) VALUES ($1, 'group') RETURNING id",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_session_members (group_session_id, member_session_id) \
             VALUES ($1, $2)",
        )
        .bind(group)
        .bind(live)
        .execute(&pool)
        .await
        .unwrap();

        let mut user_rows = Vec::new();
        for (minute, session, content) in [
            (0, live, "TEXTLINE"),
            (1, archived, "ARCHIVEDLINE"),
            (2, group, "GROUPLINE"),
            (3, live, "TEXTLINE"),
            (4, live, "TEXTLINE"),
        ] {
            let id = insert_row(
                &pool,
                session,
                "user",
                content,
                None,
                &format!("2026-10-01T10:0{minute}:00Z"),
            )
            .await;
            if session == live {
                user_rows.push(id);
            }
        }
        for id in user_rows {
            bump_relationship(&state, instance, id, None).await.unwrap();
        }
        idle_for_two_hours(&pool, instance).await;

        let tally = sweep_once(&state).await;
        assert_eq!((tally.ok, tally.empty, tally.failed), (0, 1, 0));
        let bodies = request_bodies(&llm).await;
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].contains("TEXTLINE"));
        assert!(
            !bodies[0].contains("ARCHIVEDLINE") && !bodies[0].contains("GROUPLINE"),
            "{}",
            bodies[0]
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn sweeper_embeds_pending_journal_rows(pool: PgPool) {
        let llm = MockServer::start().await;
        let (state, embed) = state_with(pool.clone(), &llm, true).await;
        let journal = JournalRepo { pool: &pool };
        for _ in 0..2 {
            let instance = seed_persona_instance(&pool, Uuid::new_v4()).await;
            let on = chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
            journal
                .insert(&first_meet(instance, on))
                .await
                .unwrap()
                .unwrap();
            let embedded = JournalInsert {
                embedding: Some(unit_embedding()),
                dedup_key: None,
                ..first_meet(instance, on)
            };
            journal.insert(&embedded).await.unwrap().unwrap();
        }

        let tally = sweep_once(&state).await;
        assert_eq!(
            tally,
            SweepTally {
                embedded: 2,
                ..Default::default()
            }
        );
        let calls = request_bodies(&embed).await;
        assert_eq!(calls.len(), 1, "one batch call for every pending row");
        let inputs = serde_json::from_str::<serde_json::Value>(&calls[0]).unwrap()["input"]
            .as_array()
            .map(Vec::len);
        assert_eq!(
            inputs,
            Some(2),
            "rows that carry a vector are not re-embedded"
        );
        let still_pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.relationship_journal WHERE embedding IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(still_pending, 0);
        assert!(journal.pending_embeddings(10).await.unwrap().is_empty());
        assert!(request_bodies(&llm).await.is_empty(), "nothing was due");

        assert_eq!(sweep_once(&state).await, SweepTally::default());
        assert_eq!(
            request_bodies(&embed).await.len(),
            1,
            "nothing left to embed"
        );
    }

    /// Without `[tasks.dreaming_scan]` the sweeper returns at once: nothing is
    /// claimed, embedded or called.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn sweeper_inert_without_dreaming_scan_task(pool: PgPool) {
        let llm = MockServer::start().await;
        answer(&llm, r#"{"memories":[]}"#).await;
        let (mut state, embed) = state_with(pool.clone(), &llm, false).await;
        state.config.dreaming_tick = Duration::from_millis(10);
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, owner).await;
        let session = new_session(&pool, owner, instance, "text").await;
        for minute in 0..3 {
            let id = insert_row(
                &pool,
                session,
                "user",
                "TEXTLINE",
                None,
                &format!("2026-10-01T10:0{minute}:00Z"),
            )
            .await;
            bump_relationship(&state, instance, id, None).await.unwrap();
        }
        idle_for_two_hours(&pool, instance).await;

        tokio::time::timeout(Duration::from_secs(5), sweeper(state))
            .await
            .expect("an unconfigured sweeper returns instead of looping");

        let s = ScanStateRepo { pool: &pool }
            .load(instance)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((s.pending_turns, s.claimed_at), (3, None));
        let pending = JournalRepo { pool: &pool }
            .pending_embeddings(10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1, "the first meeting stays unembedded");
        assert!(request_bodies(&llm).await.is_empty());
        assert!(request_bodies(&embed).await.is_empty());
    }
}

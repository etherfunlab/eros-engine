// SPDX-License-Identifier: AGPL-3.0-only
//! One relationship scan: read the stretch of chat after the cursor, ask the
//! model for profile memories, an episode and journal moments, write them and
//! move the cursor. Every `ARC_FOLD_EVERY` episodes the same job rewrites the
//! relationship arc.

use std::sync::Once;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use uuid::Uuid;

use eros_engine_llm::model_config::{ARC_REWRITE_TASK, DREAMING_SCAN_TASK};
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
use eros_engine_store::arc::ArcRepo;
use eros_engine_store::episode::{EpisodeInsert, EpisodeRepo, ScanRow};
use eros_engine_store::journal::JournalRepo;
use eros_engine_store::memory::{MemoryLayer, MemoryRepo};
use eros_engine_store::persona::PersonaRepo;
use eros_engine_store::scan_event::{ScanEventInsert, ScanEventRepo};
use eros_engine_store::scan_state::{
    ScanState, ScanStateRepo, MAX_ARC_FAILURES, MAX_SCAN_FAILURES,
};

use super::parse::{arc_guard, parse_scan_output, validate_journal};
use super::render::render_transcript;
use super::{candidate_metadata, normalise_category, MemoryCandidate, SYSTEM_AUDIT_USER};
use crate::pipeline::{record_generation, GenerationRecord};
use crate::state::AppState;

/// Rows one scan reads at most.
pub(crate) const MAX_ROWS_PER_SCAN: i64 = 60;
/// Written episodes that trigger an arc rewrite.
pub(crate) const ARC_FOLD_EVERY: i32 = 4;

static SCAN_UNCONFIGURED: Once = Once::new();
static ARC_UNCONFIGURED: Once = Once::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanOutcome {
    Ok,
    Empty,
    Failed,
}

/// Which run a segment belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanMode {
    /// The scanner proper: settles on the live cursor and the pending count.
    Live,
    /// The operator's backfill of history before the live cursor: settles on
    /// `backfilled_through_at` only and leaves the live cursor, the pending
    /// count and the failure streak alone. Profile memories are written only
    /// with `write_memories`.
    Backfill { write_memories: bool },
}

impl ScanMode {
    /// The run's name on its event rows.
    fn kind(self) -> &'static str {
        match self {
            ScanMode::Live => "scan",
            ScanMode::Backfill { .. } => "backfill",
        }
    }
}

/// The stretch one scan reads: rows in `(after, bound]`, at most
/// `MAX_ROWS_PER_SCAN`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SegmentPlan {
    pub(crate) after: DateTime<Utc>,
    pub(crate) bound: DateTime<Utc>,
    pub(crate) mode: ScanMode,
}

/// Scan one relationship from its live cursor up to `bound`. The caller holds
/// the claim, and every path settles it except a failed state read and an
/// unconfigured `dreaming_scan`, which leave it to go stale. A scan that finds
/// the claim gone writes nothing further, records an `empty` event and reports
/// `Empty`. Every scan that reached the model leaves one event row.
pub(crate) async fn scan_instance(
    state: &AppState,
    instance_id: Uuid,
    bound: DateTime<Utc>,
) -> ScanOutcome {
    let st = match (ScanStateRepo { pool: &state.pool })
        .load(instance_id)
        .await
    {
        Ok(Some(st)) => st,
        Ok(None) => return ScanOutcome::Empty,
        Err(e) => {
            tracing::warn!(%instance_id, "dreaming scan: state read failed: {e}");
            return ScanOutcome::Failed;
        }
    };
    let seg = SegmentPlan {
        after: st.scanned_through_at,
        bound,
        mode: ScanMode::Live,
    };
    scan_segment(state, &st, seg).await
}

/// Scan one segment of the relationship `st` describes. A live segment settles
/// as `scan_instance` says; a backfill segment moves `backfilled_through_at` to
/// its last row when it succeeds or reads nothing and settles nothing when it
/// fails, so the next run reads it again, and its caller releases the claim.
/// A backfill's `bound` is already a cut that splits no timestamp, so its read
/// is never trimmed.
pub(crate) async fn scan_segment(
    state: &AppState,
    st: &ScanState,
    seg: SegmentPlan,
) -> ScanOutcome {
    let pool = &state.pool;
    let scan = ScanStateRepo { pool };
    let instance_id = st.instance_id;
    let live = seg.mode == ScanMode::Live;
    let tz = state_tz(st);
    let mut rows = match (EpisodeRepo { pool })
        .read_segment(
            instance_id,
            seg.after,
            seg.bound,
            !state.config.dreaming_voice_disabled,
            MAX_ROWS_PER_SCAN,
        )
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(%instance_id, "dreaming scan: segment read failed: {e}");
            // Released at the cursor it has, so even a third failure skips
            // nothing that was never read.
            if live {
                release_failed(&scan, st, st.scanned_through_at, 0).await;
            }
            return ScanOutcome::Failed;
        }
    };
    if live && rows.len() as i64 == MAX_ROWS_PER_SCAN {
        drop_trailing_tie(&mut rows);
    }
    // A stretch with nothing readable clears the whole count: the turns it
    // counted are unreadable (voice turns once `DREAMING_VOICE_DISABLED` is
    // set, say), and a count left standing would re-claim this relationship on
    // every tick. Rows that land later are still read: the cursor, not the
    // count, decides what a scan covers.
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        settle_empty(&scan, st, seg, seg.bound).await;
        return ScanOutcome::Empty;
    };
    let covered = rows.iter().filter(|r| r.role == "user").count() as i32;
    let lines = render_transcript(&rows, tz);
    if lines.is_empty() {
        // Nothing left to read once audio tags are stripped: no call to spend.
        settle_empty(&scan, st, seg, last.sent_at).await;
        return ScanOutcome::Empty;
    }

    let Some(resolved) = state.model_config.resolve_dreaming_scan() else {
        SCAN_UNCONFIGURED.call_once(|| {
            tracing::warn!(
                "dreaming_scan is not configured: relationship scans fail without a call"
            )
        });
        return ScanOutcome::Failed;
    };
    let req_model = resolved.model.clone();
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
                content: lines.join("\n"),
            },
        ],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        sampling: resolved.sampling,
        user: Some(SYSTEM_AUDIT_USER.into()),
        task: Some(DREAMING_SCAN_TASK.into()),
        ..Default::default()
    };
    let mut event = ScanEventInsert {
        instance_id: Some(instance_id),
        kind: seg.mode.kind(),
        status: "failed",
        rows_covered: rows.len() as i32,
        ..Default::default()
    };
    let resp = match state.openrouter.execute(req).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(%instance_id, "dreaming scan: LLM call failed: {e}");
            if live {
                release_failed(&scan, st, last.sent_at, covered).await;
            }
            record_scan_event(pool, event).await;
            return ScanOutcome::Failed;
        }
    };
    event.generation_id = record_generation(
        pool,
        GenerationRecord {
            task: DREAMING_SCAN_TASK,
            session_id: Some(last.session_id),
            generation_id: resp.generation_id.as_deref(),
            model: resp.model.as_deref(),
            usage: resp.usage.as_ref(),
        },
    )
    .await;
    event.served_fallback = resp.served_hop.as_deref().is_some_and(|h| h != req_model);
    event.model = resp.served_hop;

    let Some(out) = parse_scan_output(&resp.reply) else {
        tracing::warn!(%instance_id, "dreaming scan: reply holds no JSON object");
        if live {
            release_failed(&scan, st, last.sent_at, covered).await;
        }
        event.status = "parse_error";
        record_scan_event(pool, event).await;
        return ScanOutcome::Failed;
    };

    // Profile memories have no claim guard of their own, so the claim is
    // checked (and its clock restarted) before anything is written.
    match scan.refresh_claim(instance_id).await {
        Ok(true) => {}
        Ok(false) => return discard(pool, event, "claim gone before the writes").await,
        Err(e) => {
            // Left to go stale: the segment is read again.
            tracing::warn!(%instance_id, "dreaming scan: claim check failed: {e}");
            record_scan_event(pool, event).await;
            return ScanOutcome::Failed;
        }
    }
    // A backfill segment the retired sweeper already extracted keeps its
    // memories out: they are in the profile layer once.
    if !matches!(
        seg.mode,
        ScanMode::Backfill {
            write_memories: false
        }
    ) {
        event.memories_written =
            write_memories(state, instance_id, last.session_id, &out.memories).await;
    }
    if let Some(summary) = out.episode {
        let episode = EpisodeInsert {
            instance_id,
            summary,
            started_at: first.sent_at,
            ended_at: last.sent_at,
            first_message_id: Some(first.message_id),
            last_message_id: Some(last.message_id),
            generation_id: event.generation_id.clone(),
        };
        match (EpisodeRepo { pool }).insert_if_claimed(&episode).await {
            Ok(Some(_)) => event.episode_written = true,
            Ok(None) => return discard(pool, event, "claim gone before the episode write").await,
            Err(e) => tracing::warn!(%instance_id, "dreaming scan: episode write failed: {e}"),
        }
    }
    let journal = JournalRepo { pool };
    for mut row in validate_journal(&out.journal, &rows, tz, instance_id) {
        match state.embed.embed_document(&row.summary).await {
            Ok(v) => row.embedding = Some(v),
            // Written without a vector: the sweeper embeds such rows.
            Err(e) => tracing::warn!(%instance_id, "dreaming scan: journal embed failed: {e}"),
        }
        match journal.insert_if_claimed(&row).await {
            Ok(Some(_)) => event.journal_written += 1,
            Ok(None) => {}
            Err(e) => tracing::warn!(%instance_id, "dreaming scan: journal write failed: {e}"),
        }
    }

    let settled = if live {
        scan.finish_scan_ok(instance_id, last.sent_at, covered)
            .await
    } else {
        scan.set_backfilled_through(instance_id, last.sent_at)
            .await
            .map(|()| true)
    };
    match settled {
        Ok(true) => {}
        Ok(false) => return discard(pool, event, "claim gone before the cursor moved").await,
        Err(e) => {
            // The claim goes stale and the segment is read again.
            tracing::warn!(%instance_id, "dreaming scan: cursor write failed: {e}");
            record_scan_event(pool, event).await;
            return ScanOutcome::Failed;
        }
    }
    let wrote_any =
        event.memories_written > 0 || event.episode_written || event.journal_written > 0;
    let episode_written = event.episode_written;
    event.status = if wrote_any { "ok" } else { "empty" };
    record_scan_event(pool, event).await;

    if episode_written {
        match scan.bump_episodes(instance_id).await {
            Ok(n) if n >= ARC_FOLD_EVERY => rewrite_arc(state, instance_id).await,
            Ok(_) => {}
            Err(e) => tracing::warn!(%instance_id, "dreaming scan: episode count failed: {e}"),
        }
    }
    if wrote_any {
        ScanOutcome::Ok
    } else {
        ScanOutcome::Empty
    }
}

/// Fold the relationship's unfolded episodes into a new arc. The episode count
/// restarts before the call, so a scan landing during it cannot start a second
/// rewrite; episodes a failed rewrite leaves unfolded ride into the next one.
/// A rewrite the guards reject, or a call that fails, counts against the arc:
/// the strike-out marks the pending episodes folded and the previous arc keeps
/// serving. A relationship archived or erased during the call gets no arc.
pub(crate) async fn rewrite_arc(state: &AppState, instance_id: Uuid) {
    let Some(resolved) = state.model_config.resolve_arc_rewrite() else {
        ARC_UNCONFIGURED
            .call_once(|| tracing::warn!("arc_rewrite is not configured: episodes stay unfolded"));
        return;
    };
    let pool = &state.pool;
    let scan = ScanStateRepo { pool };
    let episodes_repo = EpisodeRepo { pool };
    let inputs = async {
        let st = scan.load(instance_id).await?;
        let arc = ArcRepo { pool }.load(instance_id).await?;
        let episodes = episodes_repo.unfolded(instance_id).await?;
        Ok::<_, sqlx::Error>((st, arc, episodes))
    }
    .await;
    let (st, arc_row, episodes) = match inputs {
        Ok(inputs) => inputs,
        Err(e) => {
            tracing::warn!(%instance_id, "arc rewrite: input read failed: {e}");
            return;
        }
    };
    // A gone state row is an erased relationship: nothing to fold.
    let (Some(st), Some(folded_through)) = (st, episodes.iter().map(|e| e.ended_at).max()) else {
        return;
    };
    if let Err(e) = scan.reserve_fold(instance_id).await {
        tracing::warn!(%instance_id, "arc rewrite: reserving the fold failed: {e}");
        return;
    }
    let tz = state_tz(&st);
    let dated: Vec<(String, String)> = episodes
        .iter()
        .map(|e| (date_range(e.started_at, e.ended_at, tz), e.summary.clone()))
        .collect();
    let previous = arc_row.as_ref().map(|a| a.arc.as_str());
    let first_rewrite = arc_row.is_none();
    let req_model = resolved.model.clone();
    let req = ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: crate::prompt::arc_rewrite_prompt().into(),
            },
            ChatMessage {
                role: "user".into(),
                content: crate::prompt::arc_rewrite_user_message(
                    arc_row.as_ref().map(|a| a.opening.as_str()),
                    previous,
                    &dated,
                ),
            },
        ],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        sampling: resolved.sampling,
        user: Some(SYSTEM_AUDIT_USER.into()),
        task: Some(ARC_REWRITE_TASK.into()),
        ..Default::default()
    };

    let mut event = ScanEventInsert {
        instance_id: Some(instance_id),
        kind: "arc",
        status: "failed",
        ..Default::default()
    };
    match state.openrouter.execute(req).await {
        Err(e) => tracing::warn!(%instance_id, "arc rewrite: LLM call failed: {e}"),
        Ok(resp) => {
            event.generation_id = record_generation(
                pool,
                GenerationRecord {
                    task: ARC_REWRITE_TASK,
                    session_id: None,
                    generation_id: resp.generation_id.as_deref(),
                    model: resp.model.as_deref(),
                    usage: resp.usage.as_ref(),
                },
            )
            .await;
            event.served_fallback = resp.served_hop.as_deref().is_some_and(|h| h != req_model);
            event.model = resp.served_hop;
            event.status = match parse_arc_reply(&resp.reply) {
                None => "parse_error",
                Some((opening, arc)) => {
                    match arc_guard(previous, &arc, opening.as_deref(), first_rewrite) {
                        Err(reject) => {
                            tracing::warn!(%instance_id, ?reject, "arc rewrite rejected");
                            "rejected"
                        }
                        Ok(()) => {
                            let opening = opening.as_deref().filter(|_| first_rewrite);
                            match (ArcRepo { pool })
                                .replace_if_live(
                                    instance_id,
                                    opening,
                                    &arc,
                                    folded_through,
                                    event.generation_id.as_deref(),
                                )
                                .await
                            {
                                Ok(true) => "ok",
                                Ok(false) => {
                                    tracing::debug!(
                                        %instance_id,
                                        "arc rewrite: relationship gone, arc not written"
                                    );
                                    "empty"
                                }
                                Err(e) => {
                                    tracing::warn!(%instance_id, "arc rewrite: arc write failed: {e}");
                                    "failed"
                                }
                            }
                        }
                    }
                }
            };
        }
    }

    let ids: Vec<Uuid> = episodes.iter().map(|e| e.id).collect();
    match event.status {
        "ok" => {
            if let Err(e) = episodes_repo.mark_folded(&ids).await {
                tracing::warn!(%instance_id, "arc rewrite: folding episodes failed: {e}");
            }
            if let Err(e) = scan.fold_episodes(instance_id).await {
                tracing::warn!(%instance_id, "arc rewrite: resetting arc counters failed: {e}");
            }
        }
        // The relationship is gone: nothing left to count or fold.
        "empty" => {}
        _ => match scan.arc_failed(instance_id).await {
            Ok(n) if n >= MAX_ARC_FAILURES => {
                tracing::warn!(
                    %instance_id,
                    episodes = ids.len(),
                    "arc rewrite failed {MAX_ARC_FAILURES} times in a row: \
                     pending episodes folded without a new arc"
                );
                if let Err(e) = episodes_repo.mark_folded(&ids).await {
                    tracing::warn!(%instance_id, "arc rewrite: folding episodes failed: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(%instance_id, "arc rewrite: failure count failed: {e}"),
        },
    }
    ScanEventRepo { pool }.record(event).await;
}

/// Record a scan's event row; a scan a fallback hop served warns, whatever
/// its status, with the journal count that hop yielded.
async fn record_scan_event(pool: &sqlx::PgPool, event: ScanEventInsert) {
    if event.served_fallback {
        tracing::warn!(
            instance_id = ?event.instance_id,
            status = event.status,
            served_hop = ?event.model,
            journal_written = event.journal_written,
            "dreaming scan served by a fallback hop"
        );
    }
    ScanEventRepo { pool }.record(event).await;
}

/// A scan whose claim went away after the call: whatever was not yet written
/// is dropped, and the paid call still leaves an `empty` row.
async fn discard(pool: &sqlx::PgPool, event: ScanEventInsert, why: &str) -> ScanOutcome {
    tracing::info!(instance_id = ?event.instance_id, "dreaming scan: {why}, scan discarded");
    record_scan_event(
        pool,
        ScanEventInsert {
            status: "empty",
            ..event
        },
    )
    .await;
    ScanOutcome::Empty
}

#[derive(Deserialize)]
struct ArcReply {
    #[serde(default)]
    opening: Option<String>,
    #[serde(default)]
    arc: Option<String>,
}

/// `(opening, arc)`, both trimmed; a blank opening is none. `None` when the
/// reply holds no object or its `arc` is missing or blank.
fn parse_arc_reply(raw: &str) -> Option<(Option<String>, String)> {
    let reply: ArcReply = crate::pipeline::parse_llm_json(raw.trim())?;
    let arc = reply
        .arc
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())?;
    let opening = reply
        .opening
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty());
    Some((opening, arc))
}

/// Profile-layer memories from one scan, filed on `session_id` under the
/// instance's owner. Returns how many were written.
async fn write_memories(
    state: &AppState,
    instance_id: Uuid,
    session_id: Uuid,
    cands: &[MemoryCandidate],
) -> i32 {
    if cands.is_empty() {
        return 0;
    }
    let pool = &state.pool;
    let owner = match (PersonaRepo { pool }).instance_owner(instance_id).await {
        Ok(Some(owner)) => owner,
        Ok(None) => return 0,
        Err(e) => {
            tracing::warn!(%instance_id, "dreaming scan: owner read failed: {e}");
            return 0;
        }
    };
    let repo = MemoryRepo { pool };
    let mut written = 0;
    for cand in cands {
        let content = cand.content.trim();
        if content.is_empty() {
            continue;
        }
        let embedding = match state.embed.embed_document(content).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(%instance_id, "dreaming scan: memory embed failed: {e}");
                continue;
            }
        };
        match repo
            .upsert(
                MemoryLayer::Profile,
                session_id,
                owner,
                None,
                content,
                &embedding,
                Some(&normalise_category(&cand.category)),
                candidate_metadata(cand).as_ref(),
            )
            .await
        {
            Ok(_) => written += 1,
            Err(e) => tracing::warn!(%instance_id, "dreaming scan: memory write failed: {e}"),
        }
    }
    written
}

/// The state's timezone, UTC when it has none or an unreadable one.
fn state_tz(st: &ScanState) -> Tz {
    st.user_timezone
        .as_deref()
        .and_then(|t| t.parse().ok())
        .unwrap_or(chrono_tz::UTC)
}

/// Settle a segment that needed no call through `through`: a live scan clears
/// the whole pending count (a lost claim leaves nothing to settle), a backfill
/// moves its own mark.
async fn settle_empty(
    scan: &ScanStateRepo<'_>,
    st: &ScanState,
    seg: SegmentPlan,
    through: DateTime<Utc>,
) {
    let instance_id = st.instance_id;
    let settled = match seg.mode {
        ScanMode::Live => scan
            .finish_scan_ok(instance_id, through, st.pending_turns)
            .await
            .map(drop),
        ScanMode::Backfill { .. } => scan.set_backfilled_through(instance_id, through).await,
    };
    if let Err(e) = settled {
        tracing::warn!(%instance_id, "dreaming scan: cursor write failed: {e}");
    }
}

/// Release a failed scan's claim with one idle period of backoff; the failure
/// that strikes out moves the cursor to `cursor_to` instead.
async fn release_failed(
    scan: &ScanStateRepo<'_>,
    st: &ScanState,
    cursor_to: DateTime<Utc>,
    covered: i32,
) {
    let instance_id = st.instance_id;
    if st.scan_failures + 1 >= MAX_SCAN_FAILURES && cursor_to > st.scanned_through_at {
        tracing::warn!(
            %instance_id,
            %cursor_to,
            "dreaming scan failed {MAX_SCAN_FAILURES} times in a row: skipping the span"
        );
    }
    if let Err(e) = scan
        .finish_scan_failed(instance_id, cursor_to, covered)
        .await
    {
        tracing::warn!(%instance_id, "dreaming scan: releasing the claim failed: {e}");
    }
}

/// A read cut by the row cap may have split rows that share a `sent_at`; the
/// cursor is exclusive on `sent_at`, so the trailing rows sharing the last one
/// wait for the next scan. A segment that is one timestamp throughout is kept.
fn drop_trailing_tie(rows: &mut Vec<ScanRow>) {
    let Some(last) = rows.last().map(|r| r.sent_at) else {
        return;
    };
    if let Some(keep) = rows.iter().rposition(|r| r.sent_at != last) {
        rows.truncate(keep + 1);
    }
}

/// `YYYY-MM-DD`, or `YYYY-MM-DD–YYYY-MM-DD` when the dates differ, in `tz`.
fn date_range(started: DateTime<Utc>, ended: DateTime<Utc>, tz: Tz) -> String {
    let from = started.with_timezone(&tz).date_naive();
    let to = ended.with_timezone(&tz).date_naive();
    if from == to {
        from.to_string()
    } else {
        format!("{from}–{to}")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::routes::companion::testutil::seed_persona_instance;
    use eros_engine_store::arc::ArcRepo;
    use eros_engine_store::episode::EpisodeRepo;
    use eros_engine_store::scan_state::ScanStateRepo;
    use serde_json::json;
    use sqlx::PgPool;
    use std::time::Duration;
    use wiremock::matchers::{body_partial_json, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub(crate) const SCAN_MODEL: &str = "scan/m";
    const ARC_MODEL: &str = "arc/m";
    const SCAN_PROMPT: &str = "scan-system-prompt";

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// One relationship's chat. Stamps in Asia/Taipei (UTC+8): #1 is still
    /// 10-01 there, #2–#4 are 10-02, while every row is 10-01 in UTC. The
    /// fifth row sits past the bound the tests scan to.
    struct Rel {
        owner: Uuid,
        instance: Uuid,
        session: Uuid,
        ids: Vec<Uuid>,
    }

    const T1: &str = "2026-10-01T15:30:00Z";
    const T4: &str = "2026-10-01T16:30:00Z";

    async fn seed_rel(pool: &PgPool) -> Rel {
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(pool, owner).await;
        let session: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(owner)
        .bind(instance)
        .fetch_one(pool)
        .await
        .unwrap();
        let mut ids = Vec::new();
        for (role, content, sent_at) in [
            ("assistant", "最近怎么样？", T1),
            ("user", "其实我一直喜欢你", "2026-10-01T16:10:00Z"),
            ("assistant", "我也喜欢你", "2026-10-01T16:20:00Z"),
            ("user", "我下个月要搬去大阪了", T4),
            ("user", "晚安啦", "2026-10-01T17:00:00Z"),
        ] {
            ids.push(
                sqlx::query_scalar(
                    "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
                     VALUES ($1, $2, $3, $4) RETURNING id",
                )
                .bind(session)
                .bind(role)
                .bind(content)
                .bind(at(sent_at))
                .fetch_one(pool)
                .await
                .unwrap(),
            );
        }
        Rel {
            owner,
            instance,
            session,
            ids,
        }
    }

    /// Three user turns counted, the cursor just before #1, the scan claimed.
    async fn open_claim(pool: &PgPool, instance: Uuid) {
        let repo = ScanStateRepo { pool };
        for _ in 0..3 {
            repo.bump(instance, at(T1), Some("Asia/Taipei"))
                .await
                .unwrap();
        }
        assert!(repo
            .claim(instance, Duration::from_secs(600))
            .await
            .unwrap());
    }

    fn initial_cursor() -> DateTime<Utc> {
        at(T1) - chrono::Duration::microseconds(1)
    }

    pub(crate) fn completion(model: &str, generation: &str, content: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": generation, "model": model,
            "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20},
            "choices": [{"message": {"content": content}}],
        }))
    }

    /// Calls to `model` on `llm` answer `reply`.
    pub(crate) async fn answer(llm: &MockServer, model: &str, generation: &str, reply: &str) {
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({ "model": model })))
            .respond_with(completion(model, generation, reply))
            .mount(llm)
            .await;
    }

    fn unit_embedding() -> Vec<f32> {
        let mut v = vec![0.0_f32; 512];
        v[0] = 1.0;
        v
    }

    /// A state whose chat calls go to `llm` and whose embeddings go to a mock.
    /// `scan_extra` lands inside the `[tasks.dreaming_scan]` block.
    pub(crate) async fn state_with(
        pool: PgPool,
        llm: &MockServer,
        scan_extra: &str,
    ) -> (AppState, MockServer) {
        let embed = MockServer::start().await;
        Mock::given(wm_path("/v1/embeddings"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "data": [ { "embedding": unit_embedding() } ] })),
            )
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
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(&format!(
                "[tasks.dreaming_scan]\nmodel = \"{SCAN_MODEL}\"\nfilter_prompt = \"{SCAN_PROMPT}\"\n\
                 {scan_extra}\n\
                 [tasks.arc_rewrite]\nmodel = \"{ARC_MODEL}\"\n"
            ))
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", llm.uri()),
            ),
        );
        state.embed = std::sync::Arc::new(router);
        (state, embed)
    }

    fn scan_reply() -> String {
        json!({
            "memories": [{"content": "用户下个月搬去大阪", "category": "event",
                          "persistence": "ongoing"}],
            "episode": "用户向 AI 表白，AI 回应了心意；用户说下个月要搬去大阪。",
            "journal": [{"kind": "confession", "realm": "real", "occurred_at": "2026-10-02",
                         "summary": "用户向 AI 表白", "source": "#2"}],
        })
        .to_string()
    }

    pub(crate) async fn count(pool: &PgPool, table: &str, instance: Uuid) -> i64 {
        sqlx::query_scalar(&format!(
            "SELECT count(*) FROM engine.{table} WHERE instance_id = $1"
        ))
        .bind(instance)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    pub(crate) type EventRow = (
        String,
        String,
        Option<String>,
        bool,
        i32,
        i32,
        bool,
        i32,
        Option<String>,
    );

    pub(crate) async fn events(pool: &PgPool, instance: Uuid) -> Vec<EventRow> {
        sqlx::query_as(
            "SELECT kind, status, model, served_fallback, rows_covered, memories_written, \
                    episode_written, journal_written, generation_id \
             FROM engine.relationship_scan_events WHERE instance_id = $1 ORDER BY id",
        )
        .bind(instance)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// Return once `llm` has seen a request; fails when `job` ends first or
    /// ten seconds pass without one.
    async fn wait_for_call<T>(llm: &MockServer, job: &tokio::task::JoinHandle<T>) {
        for waited in 0.. {
            if !llm.received_requests().await.unwrap().is_empty() {
                return;
            }
            assert!(
                waited < 2_000 && !job.is_finished(),
                "the job never called the model"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub(crate) async fn request_bodies(llm: &MockServer) -> Vec<String> {
        llm.received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn scan_writes_episode_journal_memories_and_advances_cursor(pool: PgPool) {
        let rel = seed_rel(&pool).await;
        open_claim(&pool, rel.instance).await;
        let llm = MockServer::start().await;
        answer(&llm, SCAN_MODEL, "gen-scan", &scan_reply()).await;
        let (state, _embed) = state_with(pool.clone(), &llm, "").await;

        let outcome = scan_instance(&state, rel.instance, at(T4)).await;
        assert_eq!(outcome, ScanOutcome::Ok);

        let bodies = request_bodies(&llm).await;
        assert_eq!(bodies.len(), 1);
        let body = &bodies[0];
        assert!(
            body.contains(SCAN_PROMPT),
            "system prompt from the task block"
        );
        assert!(
            body.contains("[#2 2026-10-02 00:10] 用户：其实我一直喜欢你"),
            "rows stamped in the stored timezone: {body}"
        );
        assert!(body.contains(super::super::SYSTEM_AUDIT_USER));
        assert!(
            !body.contains("晚安啦"),
            "the row past the bound is not read"
        );

        type MemoryRow = (
            String,
            Option<String>,
            Uuid,
            Option<Uuid>,
            Option<serde_json::Value>,
        );
        let memories: Vec<MemoryRow> = sqlx::query_as(
            "SELECT content, category, session_id, instance_id, metadata \
                 FROM engine.companion_memories WHERE user_id = $1",
        )
        .bind(rel.owner)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            memories,
            vec![(
                "用户下个月搬去大阪".to_string(),
                Some("event".to_string()),
                rel.session,
                None,
                Some(json!({"persistence": "ongoing"})),
            )],
            "profile layer, owned by the instance owner, on the last row's session"
        );

        let episodes = EpisodeRepo { pool: &pool }
            .unfolded(rel.instance)
            .await
            .unwrap();
        assert_eq!(episodes.len(), 1);
        let e = &episodes[0];
        assert_eq!(
            e.summary,
            "用户向 AI 表白，AI 回应了心意；用户说下个月要搬去大阪。"
        );
        assert_eq!((e.started_at, e.ended_at), (at(T1), at(T4)));
        assert_eq!(
            (e.first_message_id, e.last_message_id),
            (Some(rel.ids[0]), Some(rel.ids[3]))
        );
        assert_eq!(e.generation_id.as_deref(), Some("gen-scan"));

        let journal: Vec<(
            String,
            String,
            chrono::NaiveDate,
            String,
            Option<Uuid>,
            bool,
        )> = sqlx::query_as(
            "SELECT kind, realm, occurred_on, source, message_id, embedding IS NOT NULL \
                 FROM engine.relationship_journal WHERE instance_id = $1",
        )
        .bind(rel.instance)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            journal,
            vec![(
                "confession".to_string(),
                "real".to_string(),
                chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
                "llm".to_string(),
                Some(rel.ids[1]),
                true,
            )]
        );

        let s = ScanStateRepo { pool: &pool }
            .load(rel.instance)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.scanned_through_at, at(T4));
        assert_eq!(s.pending_turns, 1, "two of three user turns covered");
        assert_eq!((s.scan_failures, s.claimed_at), (0, None));
        assert_eq!(s.episodes_since_arc, 1);

        assert_eq!(
            events(&pool, rel.instance).await,
            vec![(
                "scan".to_string(),
                "ok".to_string(),
                Some(SCAN_MODEL.to_string()),
                false,
                4,
                1,
                true,
                1,
                Some("gen-scan".to_string()),
            )]
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn scan_discards_writes_when_state_row_is_gone(pool: PgPool) {
        let rel = seed_rel(&pool).await;
        open_claim(&pool, rel.instance).await;
        let llm = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .respond_with(
                completion(SCAN_MODEL, "gen-scan", &scan_reply())
                    .set_delay(Duration::from_millis(600)),
            )
            .expect(1)
            .mount(&llm)
            .await;
        let (state, _embed) = state_with(pool.clone(), &llm, "").await;

        let scan = tokio::spawn({
            let state = state.clone();
            let instance = rel.instance;
            async move { scan_instance(&state, instance, at(T4)).await }
        });
        // The relationship goes after the call left and before its reply lands.
        wait_for_call(&llm, &scan).await;
        sqlx::query("DELETE FROM engine.relationship_scan_state WHERE instance_id = $1")
            .bind(rel.instance)
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(scan.await.unwrap(), ScanOutcome::Empty);
        let memories: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.companion_memories WHERE user_id = $1")
                .bind(rel.owner)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(memories, 0, "profile memories are discarded with the rest");
        assert_eq!(count(&pool, "relationship_episodes", rel.instance).await, 0);
        assert_eq!(count(&pool, "relationship_journal", rel.instance).await, 0);
        assert_eq!(
            count(&pool, "relationship_scan_state", rel.instance).await,
            0,
            "the state row is not resurrected"
        );
        assert_eq!(
            events(&pool, rel.instance).await,
            vec![(
                "scan".to_string(),
                "empty".to_string(),
                Some(SCAN_MODEL.to_string()),
                false,
                4,
                0,
                false,
                0,
                Some("gen-scan".to_string()),
            )],
            "the paid call still leaves a row"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn scan_parse_error_backs_off_and_third_failure_advances(pool: PgPool) {
        let rel = seed_rel(&pool).await;
        open_claim(&pool, rel.instance).await;
        let llm = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .respond_with(completion(SCAN_MODEL, "gen-bad", "抱歉，这个我做不到。"))
            .expect(3)
            .mount(&llm)
            .await;
        let (state, _embed) = state_with(pool.clone(), &llm, "").await;
        let repo = ScanStateRepo { pool: &pool };

        for failures in 1..=2 {
            if failures > 1 {
                assert!(repo
                    .claim(rel.instance, Duration::from_secs(600))
                    .await
                    .unwrap());
            }
            assert_eq!(
                scan_instance(&state, rel.instance, at(T4)).await,
                ScanOutcome::Failed
            );
            let s = repo.load(rel.instance).await.unwrap().unwrap();
            assert_eq!(s.scanned_through_at, initial_cursor(), "the cursor stays");
            assert_eq!(s.pending_turns, 3);
            assert_eq!(s.scan_failures, failures);
            assert!(s.claimed_at.is_none(), "the claim is released");
        }

        assert!(repo
            .claim(rel.instance, Duration::from_secs(600))
            .await
            .unwrap());
        assert_eq!(
            scan_instance(&state, rel.instance, at(T4)).await,
            ScanOutcome::Failed
        );
        let s = repo.load(rel.instance).await.unwrap().unwrap();
        assert_eq!(
            s.scanned_through_at,
            at(T4),
            "the third failure skips the span"
        );
        assert_eq!(s.pending_turns, 1);
        assert_eq!((s.scan_failures, s.claimed_at), (0, None));

        assert_eq!(count(&pool, "relationship_episodes", rel.instance).await, 0);
        let parse_error = (
            "scan".to_string(),
            "parse_error".to_string(),
            Some(SCAN_MODEL.to_string()),
            false,
            4,
            0,
            false,
            0,
            Some("gen-bad".to_string()),
        );
        assert_eq!(events(&pool, rel.instance).await, vec![parse_error; 3]);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn scan_marks_fallback_served(pool: PgPool) {
        let rel = seed_rel(&pool).await;
        open_claim(&pool, rel.instance).await;
        let llm = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({ "model": SCAN_MODEL })))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&llm)
            .await;
        let reply = json!({ "episode": "两人聊了搬家的事。" }).to_string();
        answer(&llm, "scan/f", "gen-fallback", &reply).await;
        let (state, _embed) = state_with(pool.clone(), &llm, "fallback = [\"scan/f\"]").await;

        assert_eq!(
            scan_instance(&state, rel.instance, at(T4)).await,
            ScanOutcome::Ok
        );
        assert_eq!(
            events(&pool, rel.instance).await,
            vec![(
                "scan".to_string(),
                "ok".to_string(),
                Some("scan/f".to_string()),
                true,
                4,
                0,
                true,
                0,
                Some("gen-fallback".to_string()),
            )]
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn fourth_episode_triggers_arc_rewrite_and_guard_rejects_long_arc(pool: PgPool) {
        let rel = seed_rel(&pool).await;
        open_claim(&pool, rel.instance).await;
        // Three earlier episodes wait unfolded; the scan writes the fourth.
        seed_episodes(&pool, rel.instance).await;
        set_arc_counters(&pool, rel.instance, 3, 0).await;

        let llm = MockServer::start().await;
        let scan = json!({ "episode": "用户向 AI 表白，AI 接受了。" }).to_string();
        answer(&llm, SCAN_MODEL, "gen-scan", &scan).await;
        let too_long = json!({ "opening": "用户在一个雨夜第一次来找 AI 聊天。",
                               "arc": "长".repeat(1_000) })
        .to_string();
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({ "model": ARC_MODEL })))
            .respond_with(completion(ARC_MODEL, "gen-arc-1", &too_long))
            .up_to_n_times(1)
            .expect(1)
            .mount(&llm)
            .await;
        let (state, _embed) = state_with(pool.clone(), &llm, "").await;

        assert_eq!(
            scan_instance(&state, rel.instance, at(T4)).await,
            ScanOutcome::Ok
        );
        let arc_body = request_bodies(&llm)
            .await
            .into_iter()
            .find(|b| b.contains(ARC_MODEL))
            .expect("the fourth episode starts an arc rewrite");
        assert!(arc_body.contains("这是第一版"), "{arc_body}");
        assert!(arc_body.contains("2026-09-10–2026-09-11"), "{arc_body}");
        assert!(arc_body.contains("用户向 AI 表白，AI 接受了。"));

        let arcs = ArcRepo { pool: &pool };
        assert!(arcs.load(rel.instance).await.unwrap().is_none());
        let scan_repo = ScanStateRepo { pool: &pool };
        let s = scan_repo.load(rel.instance).await.unwrap().unwrap();
        assert_eq!(
            (s.episodes_since_arc, s.arc_failures),
            (0, 1),
            "the fold was reserved before the call; the rejection counts"
        );
        let episodes = EpisodeRepo { pool: &pool };
        assert_eq!(episodes.unfolded(rel.instance).await.unwrap().len(), 4);
        let evs = events(&pool, rel.instance).await;
        assert_eq!(evs.len(), 2);
        assert_eq!(
            (evs[1].0.as_str(), evs[1].1.as_str(), evs[1].2.as_deref()),
            ("arc", "rejected", Some(ARC_MODEL))
        );
        assert_eq!(evs[1].8.as_deref(), Some("gen-arc-1"));

        let opening = "用户在一个雨夜第一次来找 AI 聊天。";
        let fits = json!({ "opening": opening, "arc": "弧".repeat(500) }).to_string();
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({ "model": ARC_MODEL })))
            .respond_with(completion(ARC_MODEL, "gen-arc-2", &fits))
            .expect(1)
            .mount(&llm)
            .await;
        rewrite_arc(&state, rel.instance).await;

        let a = arcs
            .load(rel.instance)
            .await
            .unwrap()
            .expect("an accepted rewrite writes the arc");
        assert_eq!(a.opening, opening);
        assert_eq!(a.arc, "弧".repeat(500));
        assert_eq!(a.previous_arc, None);
        assert_eq!(a.folded_through, at(T4), "the newest folded episode's end");
        assert!(episodes.unfolded(rel.instance).await.unwrap().is_empty());
        let s = scan_repo.load(rel.instance).await.unwrap().unwrap();
        assert_eq!((s.episodes_since_arc, s.arc_failures), (0, 0));
        let evs = events(&pool, rel.instance).await;
        assert_eq!(
            (evs[2].0.as_str(), evs[2].1.as_str(), evs[2].8.as_deref()),
            ("arc", "ok", Some("gen-arc-2"))
        );
    }

    /// Three earlier episodes of the relationship, unfolded.
    async fn seed_episodes(pool: &PgPool, instance: Uuid) {
        for (summary, started, ended) in [
            (
                "第一次聊天，聊了喜欢的电影。",
                "2026-09-01T02:00:00Z",
                "2026-09-01T03:00:00Z",
            ),
            (
                "用户说工作压力大，AI 安慰了他。",
                "2026-09-10T02:00:00Z",
                "2026-09-11T03:00:00Z",
            ),
            (
                "在角色扮演中两人去了海边。",
                "2026-09-20T02:00:00Z",
                "2026-09-20T03:00:00Z",
            ),
        ] {
            sqlx::query(
                "INSERT INTO engine.relationship_episodes \
                     (instance_id, summary, started_at, ended_at) VALUES ($1, $2, $3, $4)",
            )
            .bind(instance)
            .bind(summary)
            .bind(at(started))
            .bind(at(ended))
            .execute(pool)
            .await
            .unwrap();
        }
    }

    async fn set_arc_counters(pool: &PgPool, instance: Uuid, episodes: i32, failures: i32) {
        sqlx::query(
            "UPDATE engine.relationship_scan_state \
             SET episodes_since_arc = $2, arc_failures = $3 WHERE instance_id = $1",
        )
        .bind(instance)
        .bind(episodes)
        .bind(failures)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A relationship with three unfolded episodes and one failed rewrite
    /// behind it, and a state whose arc call answers `reply` after a delay.
    async fn delayed_rewrite(
        pool: &PgPool,
        reply: &str,
    ) -> (Rel, AppState, MockServer, MockServer) {
        let rel = seed_rel(pool).await;
        open_claim(pool, rel.instance).await;
        seed_episodes(pool, rel.instance).await;
        set_arc_counters(pool, rel.instance, ARC_FOLD_EVERY, 1).await;
        let llm = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({ "model": ARC_MODEL })))
            .respond_with(
                completion(ARC_MODEL, "gen-arc", reply).set_delay(Duration::from_millis(600)),
            )
            .expect(1)
            .mount(&llm)
            .await;
        let (state, embed) = state_with(pool.clone(), &llm, "").await;
        (rel, state, llm, embed)
    }

    fn fitting_arc() -> String {
        json!({ "opening": "用户在一个雨夜第一次来找 AI 聊天。", "arc": "弧".repeat(500) })
            .to_string()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn rewrite_reserves_the_fold_before_the_call(pool: PgPool) {
        let (rel, state, llm, _embed) = delayed_rewrite(&pool, &fitting_arc()).await;
        let scan_repo = ScanStateRepo { pool: &pool };

        let rewrite = tokio::spawn({
            let state = state.clone();
            let instance = rel.instance;
            async move { rewrite_arc(&state, instance).await }
        });
        wait_for_call(&llm, &rewrite).await;
        let s = scan_repo.load(rel.instance).await.unwrap().unwrap();
        assert_eq!(
            (s.episodes_since_arc, s.arc_failures),
            (0, 1),
            "the count restarts before the call; the failure streak is kept"
        );

        rewrite.await.unwrap();
        assert!(ArcRepo { pool: &pool }
            .load(rel.instance)
            .await
            .unwrap()
            .is_some());
        assert!(EpisodeRepo { pool: &pool }
            .unfolded(rel.instance)
            .await
            .unwrap()
            .is_empty());
        let s = scan_repo.load(rel.instance).await.unwrap().unwrap();
        assert_eq!((s.episodes_since_arc, s.arc_failures), (0, 0));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn rewrite_writes_no_arc_once_the_relationship_is_gone(pool: PgPool) {
        let (rel, state, llm, _embed) = delayed_rewrite(&pool, &fitting_arc()).await;

        let rewrite = tokio::spawn({
            let state = state.clone();
            let instance = rel.instance;
            async move { rewrite_arc(&state, instance).await }
        });
        // The relationship is archived while the call is out.
        wait_for_call(&llm, &rewrite).await;
        sqlx::query("DELETE FROM engine.relationship_scan_state WHERE instance_id = $1")
            .bind(rel.instance)
            .execute(&pool)
            .await
            .unwrap();
        rewrite.await.unwrap();

        assert!(ArcRepo { pool: &pool }
            .load(rel.instance)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            EpisodeRepo { pool: &pool }
                .unfolded(rel.instance)
                .await
                .unwrap()
                .len(),
            3,
            "nothing is folded"
        );
        assert_eq!(
            events(&pool, rel.instance).await,
            vec![(
                "arc".to_string(),
                "empty".to_string(),
                Some(ARC_MODEL.to_string()),
                false,
                0,
                0,
                false,
                0,
                Some("gen-arc".to_string()),
            )]
        );
    }

    fn row_at(sent_at: &str) -> ScanRow {
        ScanRow {
            message_id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            role: "user".to_string(),
            content: "嗨".to_string(),
            channel: None,
            sent_at: at(sent_at),
            metadata: json!({}),
        }
    }

    #[test]
    fn capped_read_drops_the_trailing_tie() {
        let (a, b, c) = (
            "2026-10-01T10:00:00Z",
            "2026-10-01T10:01:00Z",
            "2026-10-01T10:02:00Z",
        );
        let sent = |rows: &[ScanRow]| rows.iter().map(|r| r.sent_at).collect::<Vec<_>>();

        let mut rows = vec![row_at(a), row_at(b), row_at(c), row_at(c)];
        drop_trailing_tie(&mut rows);
        assert_eq!(sent(&rows), [at(a), at(b)]);

        // The cap may cut between two rows of one timestamp, so the last row
        // waits even when no other read row shares it.
        let mut rows = vec![row_at(a), row_at(b), row_at(c)];
        drop_trailing_tie(&mut rows);
        assert_eq!(sent(&rows), [at(a), at(b)]);

        let mut rows = vec![row_at(c), row_at(c), row_at(c)];
        drop_trailing_tie(&mut rows);
        assert_eq!(rows.len(), 3, "a single-timestamp segment is kept whole");
    }

    #[test]
    fn episode_dates_read_in_the_timezone() {
        let taipei = chrono_tz::Asia::Taipei;
        // 15:30Z is still the 1st in Taipei; 16:30Z is the 2nd.
        assert_eq!(
            date_range(at("2026-10-01T02:00:00Z"), at(T1), taipei),
            "2026-10-01"
        );
        assert_eq!(date_range(at(T1), at(T4), taipei), "2026-10-01–2026-10-02");
        assert_eq!(date_range(at(T1), at(T4), chrono_tz::UTC), "2026-10-01");
    }
}

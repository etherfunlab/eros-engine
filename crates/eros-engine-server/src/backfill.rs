// SPDX-License-Identifier: AGPL-3.0-only
//! `eros-engine backfill-dreaming`: the operator's pass of the relationship
//! scanner over the history that predates live scanning.
//!
//! Live scanning starts at the cursor a relationship's scan state was created
//! with; everything before it is read here, oldest first, in segments of at
//! most `MAX_ROWS_PER_SCAN` rows, each through the normal scan with
//! `kind = 'backfill'`. Progress is `backfilled_through_at`, so a run that
//! stops resumes where it stopped. Every segment is one paid call, so
//! `--dry-run` prints the count and calls nothing. Never scheduled and never
//! started by `serve`.

use std::ops::Range;

use sqlx::PgPool;
use uuid::Uuid;

use eros_engine_llm::model_config::ModelConfig;
use eros_engine_store::scan_state::{BackfillRow, ScanState, ScanStateRepo};

use crate::pipeline::dreaming::journal_first_meet;
use crate::pipeline::dreaming::scan::{
    scan_segment, ScanMode, ScanOutcome, SegmentPlan, ARC_FOLD_EVERY, MAX_ROWS_PER_SCAN,
};
use crate::state::{AppState, ServerConfig};

pub(crate) const USAGE: &str =
    "usage: eros-engine backfill-dreaming [--dry-run] [--min-rows N] [--limit N] [--instance UUID]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackfillArgs {
    pub(crate) dry_run: bool,
    /// Relationships with fewer rows left are passed over; 20 by default, 1
    /// with `--instance`.
    pub(crate) min_rows: i64,
    /// At most this many relationships, most history first.
    pub(crate) limit: Option<i64>,
    /// This relationship only.
    pub(crate) instance: Option<Uuid>,
}

impl BackfillArgs {
    /// The flags after the subcommand; `Err` names the one refused.
    pub(crate) fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let (mut dry_run, mut min_rows, mut limit, mut instance) = (false, None, None, None);
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--dry-run" => dry_run = true,
                "--min-rows" => min_rows = Some(parse_count(&flag, &value()?)?),
                "--limit" => limit = Some(parse_count(&flag, &value()?)?),
                "--instance" => {
                    let v = value()?;
                    let id = v
                        .parse()
                        .map_err(|_| format!("--instance: not a UUID: {v}"))?;
                    instance = Some(id);
                }
                _ => return Err(format!("unknown flag: {flag}")),
            }
        }
        Ok(BackfillArgs {
            dry_run,
            // A relationship named on its own is backfilled however short.
            min_rows: min_rows.unwrap_or(if instance.is_some() { 1 } else { 20 }),
            limit,
            instance,
        })
    }
}

fn parse_count(flag: &str, v: &str) -> Result<i64, String> {
    v.parse::<u32>()
        .map(i64::from)
        .map_err(|_| format!("{flag}: not a count: {v}"))
}

/// What a run would do for one relationship.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstancePlan {
    pub(crate) instance_id: Uuid,
    /// Rows left before the live cursor.
    pub(crate) rows: usize,
    /// Scan calls those rows take.
    pub(crate) segments: usize,
    /// Arc rewrites they imply at one episode per segment, counting the
    /// episodes already waiting for one.
    pub(crate) arc_rewrites: usize,
}

/// Per-segment outcomes of a run, and the relationships it left to the next.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tally {
    pub(crate) ok: u32,
    pub(crate) empty: u32,
    pub(crate) failed: u32,
    pub(crate) skipped: u32,
}

/// The subcommand: print the plan, then, unless `--dry-run`, scan. Exits 2
/// when `[tasks.dreaming_scan]` is not configured. A dry run reads the
/// database and the model config and needs no API key.
pub(crate) async fn run_backfill(args: BackfillArgs) -> anyhow::Result<()> {
    let cfg = ServerConfig::from_env().map_err(anyhow::Error::msg)?;
    let include_voice = !cfg.dreaming_voice_disabled;
    if args.dry_run {
        require_scan_task(&crate::load_model_config()?);
        let plans = plan(&crate::connect_pool().await?, &args, include_voice).await?;
        print!("{}", render_plan(&plans));
        return Ok(());
    }
    let state = crate::build_state(&cfg, false).await?;
    require_scan_task(&state.model_config);
    let plans = plan(&state.pool, &args, include_voice).await?;
    print!("{}", render_plan(&plans));
    let t = run(&state, &plans).await;
    println!(
        "segments: {} ok, {} empty, {} failed; relationships skipped: {}",
        t.ok, t.empty, t.failed, t.skipped
    );
    Ok(())
}

fn require_scan_task(model_config: &ModelConfig) {
    if model_config.resolve_dreaming_scan().is_none() {
        eprintln!(
            "backfill-dreaming: [tasks.dreaming_scan] is not configured; nothing to scan with"
        );
        std::process::exit(2);
    }
}

/// Each candidate relationship's rows, segments and implied arc rewrites.
/// Reads only: a relationship without scan state is planned on its whole
/// history, which is the window a run opens it with.
pub(crate) async fn plan(
    pool: &PgPool,
    args: &BackfillArgs,
    include_voice: bool,
) -> Result<Vec<InstancePlan>, sqlx::Error> {
    let repo = ScanStateRepo { pool };
    let candidates = repo
        .instances_with_history(args.min_rows, args.limit, args.instance, include_voice)
        .await?;
    let mut plans = Vec::with_capacity(candidates.len());
    for (instance_id, _) in candidates {
        let rows = repo.backfill_window(instance_id, include_voice).await?;
        let waiting = repo
            .load(instance_id)
            .await?
            .map_or(0, |st| st.episodes_since_arc.max(0) as usize);
        let segments = segments(&rows).len();
        plans.push(InstancePlan {
            instance_id,
            rows: rows.len(),
            segments,
            arc_rewrites: (waiting + segments) / ARC_FOLD_EVERY as usize,
        });
    }
    Ok(plans)
}

pub(crate) fn render_plan(plans: &[InstancePlan]) -> String {
    let mut out = format!("{:<36}  {:>8}  {:>8}\n", "instance_id", "rows", "segments");
    for p in plans {
        out += &format!("{:<36}  {:>8}  {:>8}\n", p.instance_id, p.rows, p.segments);
    }
    let rows: usize = plans.iter().map(|p| p.rows).sum();
    let calls: usize = plans.iter().map(|p| p.segments).sum();
    let arcs: usize = plans.iter().map(|p| p.arc_rewrites).sum();
    out += &format!(
        "{} relationships, {rows} rows: {calls} scan calls, about {arcs} arc rewrites\n",
        plans.len()
    );
    out
}

/// Cut a window into scan segments, oldest first. Each takes at most
/// `MAX_ROWS_PER_SCAN` rows; when more follow, it ends after its last reply
/// that a later row follows, so a segment closes on the companion's answer and
/// never splits rows that share a `sent_at`. Without such a reply it ends
/// after its last row that a later row follows, the live tie rule. One
/// timestamp longer than the cap is taken whole, as a live scan takes it, and
/// the rest of that timestamp is passed over.
pub(crate) fn segments(rows: &[BackfillRow]) -> Vec<Range<usize>> {
    let cap = MAX_ROWS_PER_SCAN as usize;
    let mut out = Vec::new();
    let mut start = 0;
    while start < rows.len() {
        if rows.len() - start <= cap {
            out.push(start..rows.len());
            break;
        }
        // A cut after row `i` splits nothing when row `i + 1` is later.
        let clean = |i: &usize| rows[i + 1].sent_at != rows[*i].sent_at;
        let span = start..start + cap;
        let end = span
            .clone()
            .rev()
            .filter(clean)
            .find(|&i| rows[i].assistant)
            .or_else(|| span.rev().find(clean))
            .map_or(start + cap, |i| i + 1);
        out.push(start..end);
        let through = rows[end - 1].sent_at;
        start = end
            + rows[end..]
                .iter()
                .take_while(|r| r.sent_at == through)
                .count();
    }
    out
}

/// Run the planned relationships.
pub(crate) async fn run(state: &AppState, plans: &[InstancePlan]) -> Tally {
    let mut tally = Tally::default();
    for p in plans {
        backfill_instance(state, p.instance_id, &mut tally).await;
    }
    tally
}

/// Walk one relationship's window segment by segment, each under a claim of
/// its own, so a live scan waits one segment at most. A claim the backfill
/// cannot take leaves the relationship to the next run; a failed segment
/// stops it there, and the next run starts with that segment.
async fn backfill_instance(state: &AppState, instance_id: Uuid, tally: &mut Tally) {
    let repo = ScanStateRepo { pool: &state.pool };
    let opened = async {
        let Some(st) = open_state(state, instance_id).await? else {
            return Ok(None);
        };
        let rows = repo
            .backfill_window(instance_id, !state.config.dreaming_voice_disabled)
            .await?;
        Ok::<_, sqlx::Error>(Some((st, rows)))
    }
    .await;
    let (st, rows) = match opened {
        Ok(Some(opened)) => opened,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(%instance_id, "backfill: reading the relationship failed: {e}");
            tally.skipped += 1;
            return;
        }
    };
    let Some(first) = rows.first() else {
        return;
    };
    let mut after = st
        .backfilled_through_at
        .unwrap_or(first.sent_at - chrono::Duration::microseconds(1));
    for seg in segments(&rows) {
        let bound = rows[seg.end - 1].sent_at;
        let write_memories = rows[seg].iter().any(|r| r.unextracted);
        match repo
            .claim_for_backfill(instance_id, state.config.dreaming_claim_stale_threshold)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(%instance_id, "backfill: claimed by a scan or gone; left to the next run");
                tally.skipped += 1;
                return;
            }
            Err(e) => {
                tracing::warn!(%instance_id, "backfill: claim failed: {e}");
                tally.skipped += 1;
                return;
            }
        }
        let seg_plan = SegmentPlan {
            after,
            bound,
            mode: ScanMode::Backfill { write_memories },
        };
        let outcome = scan_segment(state, &st, seg_plan).await;
        if let Err(e) = repo.release_claim(instance_id).await {
            tracing::warn!(%instance_id, "backfill: releasing the claim failed: {e}");
        }
        match outcome {
            ScanOutcome::Ok => tally.ok += 1,
            ScanOutcome::Empty => tally.empty += 1,
            ScanOutcome::Failed => {
                tally.failed += 1;
                return;
            }
        }
        after = bound;
    }
}

/// The relationship's scan state. One that has none yet gets it here, the
/// cursor at its newest row and its first meeting journaled, so a
/// relationship that never chats again still has one. `None` when it has no
/// rows to open on.
async fn open_state(state: &AppState, instance_id: Uuid) -> Result<Option<ScanState>, sqlx::Error> {
    let repo = ScanStateRepo { pool: &state.pool };
    let created = repo.ensure_for_backfill(instance_id).await?;
    let st = repo.load(instance_id).await?;
    if let (true, Some(st)) = (created, &st) {
        journal_first_meet(&state.pool, instance_id, st.scanned_through_at, None).await;
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::dreaming::scan::scan_instance;
    use crate::pipeline::dreaming::scan::tests::{
        answer, completion, count, events, request_bodies, state_with, wait_for_call, SCAN_MODEL,
    };
    use crate::routes::companion::testutil::seed_persona_instance;
    use chrono::{DateTime, Utc};
    use eros_engine_store::scan_state::{BackfillRow, ScanStateRepo};
    use serde_json::json;
    use sqlx::PgPool;
    use std::time::Duration;
    use uuid::Uuid;
    use wiremock::matchers::{any, body_partial_json, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const BASE: &str = "2026-09-01T00:00:00Z";

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// Row `i` of a seeded history: one minute apart from `BASE`.
    fn minute(i: i64) -> DateTime<Utc> {
        at(BASE) + chrono::Duration::minutes(i)
    }

    fn keys(n: usize, assistant: impl Fn(usize) -> bool) -> Vec<BackfillRow> {
        (0..n)
            .map(|i| BackfillRow {
                sent_at: minute(i as i64),
                assistant: assistant(i),
                unextracted: true,
            })
            .collect()
    }

    #[test]
    fn segments_cut_after_assistant_rows_and_respect_cap() {
        let odd = |i: usize| i % 2 == 1;
        assert!(segments(&[]).is_empty());
        assert_eq!(
            segments(&keys(60, odd)),
            vec![0..60_usize],
            "60 rows fit one scan"
        );
        assert_eq!(
            segments(&keys(61, odd)),
            [0..60, 60..61],
            "the 60th row ends the scan when the row after it is later"
        );
        assert_eq!(
            segments(&keys(70, |i| i % 2 == 0)),
            [0..59, 59..70],
            "a capped scan ends on its last reply"
        );

        // The reply at the cap shares its timestamp with the next row: the
        // cut moves back to the reply before it.
        let mut tied = keys(70, odd);
        tied[60].sent_at = tied[59].sent_at;
        assert_eq!(segments(&tied), [0..58, 58..70]);

        assert_eq!(
            segments(&keys(70, |_| false)),
            [0..60, 60..70],
            "no reply: the tie rule alone decides"
        );
        let mut tie_tail = keys(70, |_| false);
        tie_tail[60].sent_at = tie_tail[59].sent_at;
        assert_eq!(segments(&tie_tail), [0..59, 59..70]);

        // One timestamp longer than the cap is read whole, as a live scan
        // does; the rest of it lies behind the cursor.
        let mut burst = keys(66, |_| false);
        for row in &mut burst[..65] {
            row.sent_at = minute(0);
        }
        assert_eq!(segments(&burst), [0..60, 65..66]);

        let long = keys(500, |i| i % 4 == 3);
        let cut = segments(&long);
        assert_eq!(cut.first().map(|r| r.start), Some(0));
        assert_eq!(cut.last().map(|r| r.end), Some(500));
        for pair in cut.windows(2) {
            assert_eq!(pair[0].end, pair[1].start, "segments are contiguous");
        }
        for seg in &cut {
            assert!(seg.len() as i64 <= MAX_ROWS_PER_SCAN, "{seg:?}");
        }
        for seg in &cut[..cut.len() - 1] {
            assert!(long[seg.end - 1].assistant, "{seg:?} ends on a reply");
        }
    }

    /// A relationship with one live text session; returns `(owner, instance,
    /// session)`.
    async fn seed_rel(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(pool, owner).await;
        let session = new_session(pool, owner, instance).await;
        (owner, instance, session)
    }

    async fn new_session(pool: &PgPool, owner: Uuid, instance: Uuid) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(owner)
        .bind(instance)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Rows `from..to` of a history, a minute apart, user first and then
    /// alternating, each saying `<i>`.
    async fn seed_rows(pool: &PgPool, session: Uuid, from: i64, to: i64) {
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             SELECT $1, CASE WHEN g % 2 = 0 THEN 'user' ELSE 'assistant' END, \
                    '<' || g || '>', $2::timestamptz + g * interval '1 minute' \
             FROM generate_series($3::bigint, $4::bigint - 1) g",
        )
        .bind(session)
        .bind(at(BASE))
        .bind(from)
        .bind(to)
        .execute(pool)
        .await
        .unwrap();
    }

    fn args(dry_run: bool) -> BackfillArgs {
        BackfillArgs {
            dry_run,
            min_rows: 20,
            limit: None,
            instance: None,
        }
    }

    fn scan_reply() -> String {
        json!({
            "memories": [{"content": "用户喜欢猫", "category": "preference"}],
            "episode": "两人聊了很久。",
        })
        .to_string()
    }

    async fn backfilled_through(pool: &PgPool, instance: Uuid) -> Option<DateTime<Utc>> {
        ScanStateRepo { pool }
            .load(instance)
            .await
            .unwrap()
            .unwrap()
            .backfilled_through_at
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn dry_run_counts_without_calls(pool: PgPool) {
        let (_, instance, session) = seed_rel(&pool).await;
        seed_rows(&pool, session, 0, 130).await;
        let (_, short, short_session) = seed_rel(&pool).await;
        seed_rows(&pool, short_session, 0, 5).await;
        let llm = MockServer::start().await;
        let (state, _embed) = state_with(pool.clone(), &llm, "").await;

        let no_calls = Mock::given(any())
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount_as_scoped(&llm)
            .await;
        let plans = plan(&state.pool, &args(true), true).await.unwrap();
        assert_eq!(
            plans,
            vec![InstancePlan {
                instance_id: instance,
                rows: 130,
                segments: 3,
                arc_rewrites: 0,
            }],
            "the five-row relationship is under the floor"
        );
        let table = render_plan(&plans);
        assert!(table.contains(&instance.to_string()), "{table}");
        assert!(table.contains("3 scan calls"), "{table}");
        drop(no_calls);
        for table in ["relationship_scan_state", "relationship_journal"] {
            assert_eq!(count(&pool, table, instance).await, 0, "{table} untouched");
        }
        assert!(events(&pool, instance).await.is_empty());
        assert_eq!(count(&pool, "relationship_scan_state", short).await, 0);

        answer(&llm, SCAN_MODEL, "gen-scan", &scan_reply()).await;
        let tally = run(&state, &plans).await;
        assert_eq!(
            tally,
            Tally {
                ok: 3,
                ..Default::default()
            }
        );
        assert_eq!(
            request_bodies(&llm).await.len(),
            3,
            "a real run makes the calls the dry run counted"
        );
        assert_eq!(backfilled_through(&pool, instance).await, Some(minute(129)));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn memories_skipped_for_segments_before_classified_at(pool: PgPool) {
        let (owner, instance, swept) = seed_rel(&pool).await;
        let later = new_session(&pool, owner, instance).await;
        seed_rows(&pool, swept, 0, 60).await;
        seed_rows(&pool, later, 60, 64).await;
        // The retired sweeper extracted the first session after its last row,
        // and stamped the second before any of its rows.
        for (session, stamp) in [(swept, minute(100)), (later, minute(-1))] {
            sqlx::query("UPDATE engine.chat_sessions SET classified_at = $2 WHERE id = $1")
                .bind(session)
                .bind(stamp)
                .execute(&pool)
                .await
                .unwrap();
        }
        let llm = MockServer::start().await;
        answer(&llm, SCAN_MODEL, "gen-scan", &scan_reply()).await;
        let (state, _embed) = state_with(pool.clone(), &llm, "").await;

        let plans = plan(&state.pool, &args(false), true).await.unwrap();
        assert_eq!(plans[0].segments, 2);
        run(&state, &plans).await;

        let memories: Vec<Uuid> = sqlx::query_scalar(
            "SELECT session_id FROM engine.companion_memories WHERE user_id = $1",
        )
        .bind(owner)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(memories, vec![later], "only the unextracted segment");
        assert_eq!(
            count(&pool, "relationship_episodes", instance).await,
            2,
            "episodes come from every segment"
        );
        let evs = events(&pool, instance).await;
        assert_eq!(
            evs.iter()
                .map(|e| (e.0.as_str(), e.1.as_str(), e.5, e.6))
                .collect::<Vec<_>>(),
            [("backfill", "ok", 0, true), ("backfill", "ok", 1, true)]
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn resumes_from_backfilled_through(pool: PgPool) {
        let (_, instance, session) = seed_rel(&pool).await;
        seed_rows(&pool, session, 0, 70).await;
        let llm = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .and(body_partial_json(json!({ "model": SCAN_MODEL })))
            .respond_with(completion(SCAN_MODEL, "gen-1", &scan_reply()))
            .up_to_n_times(1)
            .mount(&llm)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&llm)
            .await;
        let (state, _embed) = state_with(pool.clone(), &llm, "").await;

        let plans = plan(&state.pool, &args(false), true).await.unwrap();
        assert_eq!(plans[0].segments, 2);
        let tally = run(&state, &plans).await;
        assert_eq!(
            tally,
            Tally {
                ok: 1,
                failed: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            backfilled_through(&pool, instance).await,
            Some(minute(59)),
            "a failed segment leaves the mark where the last good one put it"
        );
        let s = ScanStateRepo { pool: &pool }
            .load(instance)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((s.scan_failures, s.claimed_at), (0, None));
        let evs = events(&pool, instance).await;
        assert_eq!(
            evs.iter()
                .map(|e| (e.0.as_str(), e.1.as_str()))
                .collect::<Vec<_>>(),
            [("backfill", "ok"), ("backfill", "failed")]
        );

        llm.reset().await;
        answer(&llm, SCAN_MODEL, "gen-2", &scan_reply()).await;
        let plans = plan(&state.pool, &args(false), true).await;
        assert!(
            plans.unwrap().is_empty(),
            "ten rows left are under the floor"
        );
        let resume = BackfillArgs {
            min_rows: 1,
            ..args(false)
        };
        let plans = plan(&state.pool, &resume, true).await.unwrap();
        assert_eq!((plans[0].rows, plans[0].segments), (10, 1));
        run(&state, &plans).await;
        let bodies = request_bodies(&llm).await;
        assert_eq!(bodies.len(), 1);
        assert!(
            bodies[0].contains("<60>") && bodies[0].contains("<69>"),
            "{}",
            bodies[0]
        );
        assert!(
            !bodies[0].contains("<59>"),
            "covered rows are not read again"
        );
        assert_eq!(backfilled_through(&pool, instance).await, Some(minute(69)));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn backfill_leaves_live_cursor_and_pending_alone(pool: PgPool) {
        let (_, instance, session) = seed_rel(&pool).await;
        // Thirty rows of history, then the first turn after the upgrade.
        seed_rows(&pool, session, 0, 31).await;
        let repo = ScanStateRepo { pool: &pool };
        for _ in 0..3 {
            repo.bump(instance, minute(30), Some("Asia/Taipei"))
                .await
                .unwrap();
        }
        sqlx::query(
            "UPDATE engine.relationship_scan_state SET scan_failures = 1 WHERE instance_id = $1",
        )
        .bind(instance)
        .execute(&pool)
        .await
        .unwrap();
        let before = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(before.scanned_through_at, minute(29));

        let llm = MockServer::start().await;
        answer(&llm, SCAN_MODEL, "gen-scan", &scan_reply()).await;
        let (state, _embed) = state_with(pool.clone(), &llm, "").await;
        let plans = plan(&state.pool, &args(false), true).await.unwrap();
        assert_eq!((plans[0].rows, plans[0].segments), (30, 1));

        assert!(repo
            .claim(instance, std::time::Duration::from_secs(600))
            .await
            .unwrap());
        assert_eq!(
            run(&state, &plans).await,
            Tally {
                skipped: 1,
                ..Default::default()
            },
            "a live scan holds the relationship: left to the next run"
        );
        assert!(request_bodies(&llm).await.is_empty());
        repo.release_claim(instance).await.unwrap();

        assert_eq!(
            run(&state, &plans).await,
            Tally {
                ok: 1,
                ..Default::default()
            }
        );

        let bodies = request_bodies(&llm).await;
        assert!(!bodies[0].contains("<30>"), "the live turn is not read");
        let after = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(
            (
                after.scanned_through_at,
                after.pending_turns,
                after.last_turn_at,
                after.last_scanned_at,
                after.scan_failures,
                after.claimed_at,
            ),
            (
                before.scanned_through_at,
                before.pending_turns,
                before.last_turn_at,
                before.last_scanned_at,
                before.scan_failures,
                None,
            )
        );
        assert_eq!(after.backfilled_through_at, Some(minute(29)));
        assert_eq!(after.episodes_since_arc, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn backfill_window_ignores_live_progress(pool: PgPool) {
        let (_, instance, session) = seed_rel(&pool).await;
        // Thirty rows of history, then two turns after the upgrade that a
        // live scan reads before the backfill runs.
        seed_rows(&pool, session, 0, 34).await;
        let repo = ScanStateRepo { pool: &pool };
        repo.bump(instance, minute(30), None).await.unwrap();
        repo.bump(instance, minute(32), None).await.unwrap();
        let llm = MockServer::start().await;
        answer(&llm, SCAN_MODEL, "gen-scan", &scan_reply()).await;
        let (state, _embed) = state_with(pool.clone(), &llm, "").await;
        assert!(repo
            .claim(instance, Duration::from_secs(600))
            .await
            .unwrap());
        assert_eq!(
            scan_instance(&state, instance, minute(33)).await,
            ScanOutcome::Ok
        );
        let s = repo.load(instance).await.unwrap().unwrap();
        assert_eq!(
            (s.scanned_through_at, s.live_from_at),
            (minute(33), minute(29))
        );

        let plans = plan(&state.pool, &args(false), true).await.unwrap();
        assert_eq!(
            (plans[0].rows, plans[0].segments),
            (30, 1),
            "the live-scanned turns are not counted"
        );
        assert_eq!(
            run(&state, &plans).await,
            Tally {
                ok: 1,
                ..Default::default()
            }
        );
        let bodies = request_bodies(&llm).await;
        assert_eq!(bodies.len(), 2, "one live scan, one backfill segment");
        assert!(
            bodies[1].contains("<29>") && !bodies[1].contains("<30>"),
            "{}",
            bodies[1]
        );
        let ended: Vec<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT ended_at FROM engine.relationship_episodes \
             WHERE instance_id = $1 ORDER BY ended_at",
        )
        .bind(instance)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            ended,
            [minute(29), minute(33)],
            "no live turn gets a second episode"
        );
        assert_eq!(backfilled_through(&pool, instance).await, Some(minute(29)));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn backfill_segment_fails_when_its_claim_goes(pool: PgPool) {
        let (_, instance, session) = seed_rel(&pool).await;
        seed_rows(&pool, session, 0, 70).await;
        let llm = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .respond_with(
                completion(SCAN_MODEL, "gen-scan", &scan_reply())
                    .set_delay(Duration::from_millis(600)),
            )
            .mount(&llm)
            .await;
        let (state, _embed) = state_with(pool.clone(), &llm, "").await;
        let plans = plan(&state.pool, &args(false), true).await.unwrap();
        assert_eq!(plans[0].segments, 2);

        let job = tokio::spawn({
            let state = state.clone();
            async move { run(&state, &plans).await }
        });
        // The claim goes while the first segment's call is out.
        wait_for_call(&llm, &job).await;
        ScanStateRepo { pool: &pool }
            .release_claim(instance)
            .await
            .unwrap();

        assert_eq!(
            job.await.unwrap(),
            Tally {
                failed: 1,
                ..Default::default()
            },
            "the run stops at the segment it lost"
        );
        assert_eq!(
            request_bodies(&llm).await.len(),
            1,
            "the rest waits for the next run"
        );
        assert_eq!(backfilled_through(&pool, instance).await, None);
        assert_eq!(count(&pool, "relationship_episodes", instance).await, 0);
        let evs = events(&pool, instance).await;
        assert_eq!(
            evs.iter()
                .map(|e| (e.0.as_str(), e.1.as_str()))
                .collect::<Vec<_>>(),
            [("backfill", "empty")],
            "the paid call still leaves a row"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn ensure_for_backfill_writes_first_meet_once(pool: PgPool) {
        let (_, instance, session) = seed_rel(&pool).await;
        seed_rows(&pool, session, 0, 30).await;
        let llm = MockServer::start().await;
        let (state, _embed) = state_with(pool.clone(), &llm, "").await;

        for _ in 0..2 {
            let st = open_state(&state, instance).await.unwrap().unwrap();
            assert_eq!((st.scanned_through_at, st.pending_turns), (minute(29), 0));
        }
        let first_meet: Vec<chrono::NaiveDate> = sqlx::query_scalar(
            "SELECT occurred_on FROM engine.relationship_journal \
             WHERE instance_id = $1 AND kind = 'first_meet'",
        )
        .bind(instance)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            first_meet,
            vec![at(BASE).date_naive()],
            "dated by the first row, written once"
        );
    }

    #[test]
    fn backfill_args_parse_flags_and_refuse_unknown_ones() {
        let parse = |a: &[&str]| BackfillArgs::parse(a.iter().map(|s| s.to_string()));
        assert_eq!(parse(&[]), Ok(args(false)));
        let id = Uuid::new_v4();
        assert_eq!(
            parse(&["--instance", &id.to_string()]),
            Ok(BackfillArgs {
                min_rows: 1,
                instance: Some(id),
                ..args(false)
            }),
            "one named relationship needs no floor"
        );
        assert_eq!(
            parse(&[
                "--dry-run",
                "--min-rows",
                "5",
                "--limit",
                "10",
                "--instance",
                &id.to_string()
            ]),
            Ok(BackfillArgs {
                dry_run: true,
                min_rows: 5,
                limit: Some(10),
                instance: Some(id),
            })
        );
        assert!(parse(&["--force"]).is_err());
        assert!(parse(&["--limit"]).is_err());
        assert!(parse(&["--limit", "-1"]).is_err());
        assert!(parse(&["--instance", "nope"]).is_err());
    }
}

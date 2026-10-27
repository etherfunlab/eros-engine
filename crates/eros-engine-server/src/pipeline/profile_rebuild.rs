// SPDX-License-Identifier: AGPL-3.0-only
//! Profile rebuild: the derived `human_insights` row is regenerated from the
//! user's whole fact ledger, never patched from its previous value.
//!
//! A rebuild runs for one user at a time and always ends by releasing that
//! user's `human_profile_state` claim, so the idle-flush sweeper below and any
//! other caller can hand it a user without tracking the claim themselves.
//! `Ok` and `Empty` settle the pending counter; `Failed` keeps it for the
//! bounded retry in `ProfileStateRepo`.

use uuid::Uuid;

use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
use eros_engine_store::human_fact::{HumanFactRepo, HumanFactRow};
use eros_engine_store::human_insight::{project_profile, HumanInsightRepo};
use eros_engine_store::insight::{InsightEventInsert, InsightEventRepo};
use eros_engine_store::parse_error_payload;
use eros_engine_store::profile_state::ProfileStateRepo;

use crate::state::AppState;

/// Cap on the rendered ledger, in characters. Past it the lowest-ranked facts
/// are left out so the prompt stays inside the model's context.
pub const LEDGER_BUDGET_CHARS: usize = 16_000;
pub const PROFILE_REBUILD_TASK: &str = "profile_rebuild";
/// Sentinel OpenRouter `user` for rebuild calls, so their spend is attributable
/// apart from the other background subsystems (dreaming `…111`, world `…112`,
/// stories `…113`). Not a real auth UUID; cannot collide with a real user.
pub(crate) const PROFILE_REBUILD_AUDIT_USER: &str = "11111111-1111-1111-1111-111111111114";

/// What a rebuild did to the profile row.
#[derive(Debug, PartialEq)]
pub enum RebuildOutcome {
    /// The profile row was replaced.
    Ok,
    /// Nothing to write: no ledger facts, or the model supported no field.
    Empty,
    /// No usable reply or the write failed; the profile is untouched.
    Failed,
}

/// A rendered line's characters plus the newline that joins it to the next.
fn line_cost(line: &str) -> usize {
    line.chars().count() + 1
}

fn ledger_line(r: &HumanFactRow) -> String {
    // `unknown` says nothing the model can use, so the segment is dropped.
    let persistence = if r.persistence == "unknown" {
        String::new()
    } else {
        format!("持续性: {}；", r.persistence)
    };
    let frequency = if r.occurrences > 1 {
        format!("，×{}，{} 个会话", r.occurrences, r.session_count)
    } else {
        String::new()
    };
    // The labels are printed as their literal codes: the prompt's rules name
    // `transient`, `repeated_across_sessions` and `roleplay_expression`.
    format!(
        "- {}（证据: {}；{persistence}复现: {}{frequency}；最近 {}）",
        r.content,
        r.evidence_type,
        r.recurrence().as_str(),
        r.last_seen_at.format("%Y-%m-%d"),
    )
}

/// Render the ledger as `## domain` groups of one line per fact.
///
/// Facts are ranked by occurrences, then recency, and admitted in that order
/// while the output (headers included) stays within `budget` characters. The
/// first fact that does not fit ends the walk, so what is kept is always the
/// top-ranked prefix of the ledger. Groups appear in the order their first fact
/// was admitted. An empty ledger renders as the empty string.
pub(crate) fn render_ledger(rows: &[HumanFactRow], budget: usize) -> String {
    let mut ranked: Vec<&HumanFactRow> = rows.iter().collect();
    ranked.sort_by(|a, b| {
        b.occurrences
            .cmp(&a.occurrences)
            .then(b.last_seen_at.cmp(&a.last_seen_at))
    });

    let mut groups: Vec<(&str, Vec<String>)> = Vec::new();
    let mut used = 0;
    for row in ranked {
        let line = ledger_line(row);
        let slot = groups.iter().position(|(d, _)| *d == row.domain);
        let header_cost = if slot.is_none() {
            line_cost(&format!("## {}", row.domain))
        } else {
            0
        };
        let cost = header_cost + line_cost(&line);
        // The joined output has no newline after its last line.
        if (used + cost).saturating_sub(1) > budget {
            break;
        }
        used += cost;
        match slot {
            Some(i) => groups[i].1.push(line),
            None => groups.push((&row.domain, vec![line])),
        }
    }

    groups
        .into_iter()
        .flat_map(|(domain, lines)| std::iter::once(format!("## {domain}")).chain(lines))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rebuild one user's profile from their ledger and release their claim.
///
/// The caller already holds the claim (`ProfileStateRepo::claim` /
/// `claim_due`): claiming is what keeps two workers off the same user, and
/// this function only settles the state row once it is done. A user erased
/// while the rebuild ran has no claim left to settle, so nothing is written
/// and the outcome is `Empty`.
pub(crate) async fn rebuild_user(state: &AppState, user_id: Uuid) -> RebuildOutcome {
    let Some(outcome) = rebuild(state, user_id).await else {
        return RebuildOutcome::Empty;
    };
    let repo = ProfileStateRepo { pool: &state.pool };
    let released = match outcome {
        RebuildOutcome::Failed => repo.finish_failed(user_id).await,
        RebuildOutcome::Ok | RebuildOutcome::Empty => repo.finish_ok(user_id).await,
    };
    // An unreleased claim is not lost work: it goes stale and is re-claimed.
    if let Err(e) = released {
        tracing::warn!(%user_id, "profile rebuild: releasing the claim failed: {e}");
    }
    outcome
}

/// `None` when the claim was gone before the profile write (see
/// `HumanInsightRepo::replace_profile_if_claimed`): the state row is then no
/// longer this rebuild's to settle.
async fn rebuild(state: &AppState, user_id: Uuid) -> Option<RebuildOutcome> {
    let rows = match (HumanFactRepo { pool: &state.pool })
        .list_for_user(user_id)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(%user_id, "profile rebuild: ledger read failed: {e}");
            return Some(RebuildOutcome::Failed);
        }
    };
    // Without facts there is nothing to ask the model, and the counter that
    // brought the user here would otherwise trigger the same empty rebuild again.
    if rows.is_empty() {
        return Some(RebuildOutcome::Empty);
    }

    let resolved = state.model_config.resolve_profile_rebuild();
    let req = ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages: vec![ChatMessage {
            role: "user".into(),
            content: crate::prompt::profile_rebuild_prompt(&render_ledger(
                &rows,
                LEDGER_BUDGET_CHARS,
            )),
        }],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        sampling: resolved.sampling,
        user: Some(PROFILE_REBUILD_AUDIT_USER.into()),
        task: Some(PROFILE_REBUILD_TASK.into()),
        ..Default::default()
    };
    // No reply, no audit row: the table records what a model said, and a
    // transport error says nothing.
    let resp = match state.openrouter.execute(req).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(%user_id, "profile rebuild: LLM call failed: {e}");
            return Some(RebuildOutcome::Failed);
        }
    };
    let generation_id = super::record_generation(
        &state.pool,
        super::GenerationRecord {
            task: PROFILE_REBUILD_TASK,
            session_id: None,
            generation_id: resp.generation_id.as_deref(),
            model: resp.model.as_deref(),
            usage: resp.usage.as_ref(),
        },
    )
    .await;

    let raw = resp.reply.trim();
    // A bare scalar or array parses as JSON but is no profile: that is a reply
    // the model got wrong, to be retried, not a profile with nothing in it.
    let Some(profile) =
        super::parse_llm_json::<serde_json::Value>(raw).filter(serde_json::Value::is_object)
    else {
        write_profile_event(
            &state.pool,
            user_id,
            "parse_error",
            Some(parse_error_payload(raw)),
            generation_id,
        )
        .await;
        return Some(RebuildOutcome::Failed);
    };

    let columns = project_profile(&profile);
    // An empty projection settles the counter too. The same ledger would yield
    // the same answer, so retrying cannot help; the next new fact starts a
    // fresh count.
    if columns.is_empty() {
        write_profile_event(&state.pool, user_id, "empty", Some(profile), generation_id).await;
        return Some(RebuildOutcome::Empty);
    }
    match (HumanInsightRepo { pool: &state.pool })
        .replace_profile_if_claimed(user_id, &columns)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(
                %user_id,
                "profile rebuild: claim gone before the write, profile not written"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(%user_id, "profile rebuild: profile write failed: {e}");
            return Some(RebuildOutcome::Failed);
        }
    }
    write_profile_event(&state.pool, user_id, "ok", Some(profile), generation_id).await;
    Some(RebuildOutcome::Ok)
}

/// Append one `profile`-stage audit row. The rebuild belongs to the user, not
/// to a session or message, so both pointers stay empty. The payload is the
/// model's object as returned, not the projected columns, so a value the
/// projection dropped stays visible to an operator. Fail-open: a lost audit
/// row must not undo a rebuild.
async fn write_profile_event(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    status: &str,
    payload: Option<serde_json::Value>,
    generation_id: Option<String>,
) {
    let ev = InsightEventInsert {
        run_id: Uuid::new_v4(),
        user_id,
        session_id: None,
        message_id: None,
        stage: "profile",
        status,
        payload,
        generation_id,
    };
    if let Err(e) = (InsightEventRepo { pool }).record(ev).await {
        tracing::warn!("insight event (profile) persist failed: {e}");
    }
}

/// Run forever, rebuilding users whose new facts have sat idle. Spawn once at
/// server startup; returns at once when the configured tick is zero.
pub async fn sweeper(state: AppState) {
    let cfg = state.config.profile_rebuild.clone();
    if cfg.tick.is_zero() {
        tracing::info!("profile rebuild sweeper disabled (PROFILE_REBUILD_DISABLED=1 or tick=0)");
        return;
    }
    tracing::info!(
        tick = ?cfg.tick,
        idle = ?cfg.idle,
        claim_stale = ?cfg.claim_stale,
        batch = cfg.batch,
        "profile rebuild sweeper starting"
    );

    let mut tick = tokio::time::interval(cfg.tick);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let claimed = match (ProfileStateRepo { pool: &state.pool })
            .claim_due(cfg.idle, cfg.claim_stale, cfg.batch)
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!("profile rebuild claim failed: {e}");
                continue;
            }
        };
        if claimed.is_empty() {
            continue;
        }
        let (ok, empty, failed, skipped) = rebuild_claimed(&state, &claimed).await;
        tracing::info!(ok, empty, failed, skipped, "profile rebuild: users swept");
    }
}

/// Rebuild a claimed batch one user at a time and tally the outcomes as
/// `(ok, empty, failed, skipped)`.
///
/// The whole batch was claimed at once, so a user late in the line would
/// otherwise reach its turn with a claim older than the stale window and be
/// rebuilt a second time by another instance. Each claim is therefore
/// refreshed just before its rebuild. A user whose claim cannot be refreshed
/// is skipped: the state row is gone (erased) or the claim was released, and
/// either way no model call should be spent on it.
async fn rebuild_claimed(state: &AppState, claimed: &[Uuid]) -> (u32, u32, u32, u32) {
    let repo = ProfileStateRepo { pool: &state.pool };
    let (mut ok, mut empty, mut failed, mut skipped) = (0, 0, 0, 0);
    for user_id in claimed {
        match repo.refresh_claim(*user_id).await {
            Ok(true) => {}
            Ok(false) => {
                skipped += 1;
                continue;
            }
            Err(e) => {
                // The claim stays as it was and goes stale into a later sweep.
                tracing::warn!(%user_id, "profile rebuild: refreshing the claim failed: {e}");
                skipped += 1;
                continue;
            }
        }
        match rebuild_user(state, *user_id).await {
            RebuildOutcome::Ok => ok += 1,
            RebuildOutcome::Empty => empty += 1,
            RebuildOutcome::Failed => failed += 1,
        }
    }
    (ok, empty, failed, skipped)
}

#[cfg(test)]
mod tests {
    use super::super::dreaming::SYSTEM_AUDIT_USER;
    use super::super::story::STORY_AUDIT_USER;
    use super::super::world::WORLD_AUDIT_USER;
    use super::*;
    use chrono::{TimeZone, Utc};
    use eros_engine_store::erasure::ErasureRepo;
    use eros_engine_store::human_fact::{FactInput, FactSource, HumanFactRepo};
    use eros_engine_store::profile_state::ProfileStateRepo;
    use sqlx::PgPool;
    use std::time::Duration;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn row(
        content: &str,
        domain: &str,
        occurrences: i32,
        last_seen_day: u32,
        persistence: &str,
    ) -> HumanFactRow {
        let seen = Utc
            .with_ymd_and_hms(2026, 10, last_seen_day, 8, 30, 0)
            .unwrap();
        HumanFactRow {
            id: Uuid::new_v4(),
            user_id: Uuid::nil(),
            content: content.into(),
            category: "other".into(),
            domain: domain.into(),
            evidence_type: "explicit_statement".into(),
            temporality: "current".into(),
            persistence: persistence.into(),
            confidence: "high".into(),
            occurrences,
            session_count: if occurrences > 1 { 2 } else { 1 },
            first_seen_at: seen,
            last_seen_at: seen,
        }
    }

    #[test]
    fn render_groups_by_domain_and_ranks_by_occurrences_then_recency() {
        // Deliberately shuffled: the renderer, not the caller, owns the order.
        let rows = vec![
            row("A", "career", 1, 20, "unknown"),
            row("B", "hobby", 3, 5, "unknown"),
            row("C", "career", 3, 10, "unknown"),
        ];
        let out = render_ledger(&rows, usize::MAX);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 5, "two headers + three rows: {out}");
        // C and B tie on occurrences; C is more recent, so `career` appears first.
        assert_eq!(lines[0], "## career");
        assert!(lines[1].starts_with("- C（"), "{out}");
        assert!(lines[2].starts_with("- A（"), "{out}");
        assert_eq!(lines[3], "## hobby");
        assert!(lines[4].starts_with("- B（"), "{out}");
    }

    #[test]
    fn render_line_carries_evidence_persistence_recurrence_and_date() {
        let once = row("在做后端", "career", 1, 20, "unknown");
        assert_eq!(
            render_ledger(&[once], usize::MAX),
            "## career\n- 在做后端（证据: explicit_statement；复现: once；最近 2026-10-20）",
            "persistence is omitted when unknown; no frequency tail for a single occurrence"
        );

        let repeated = row("爱爬山", "hobby", 3, 21, "stable");
        assert_eq!(
            render_ledger(&[repeated], usize::MAX),
            "## hobby\n- 爱爬山（证据: explicit_statement；持续性: stable；\
             复现: repeated_across_sessions，×3，2 个会话；最近 2026-10-21）"
        );

        let mut in_session = row("在煮面", "lifestyle", 2, 22, "transient");
        in_session.session_count = 1;
        assert!(render_ledger(&[in_session], usize::MAX)
            .contains("持续性: transient；复现: repeated_in_session，×2，1 个会话；"),);
    }

    #[test]
    fn render_respects_budget_counting_headers() {
        let rows = vec![
            row("one", "career", 3, 20, "unknown"),
            row("two", "hobby", 2, 20, "unknown"),
            row("three", "career", 1, 20, "unknown"),
        ];
        let chars = |s: &str| s.chars().count();

        // Exactly enough for rows 1-2 and both headers: row 3 is cut.
        let two = render_ledger(&rows[..2], usize::MAX);
        let out = render_ledger(&rows, chars(&two));
        assert_eq!(out, two);
        assert!(!out.contains("three"));

        // One char short: row 2's header is part of its cost, so it is cut with
        // its header rather than leaving an empty `## hobby` behind.
        let tight = chars(&two) - 1;
        let out = render_ledger(&rows, tight);
        assert!(chars(&out) <= tight);
        assert!(out.contains("one") && !out.contains("two"));
        assert!(!out.contains("## hobby"));
    }

    #[test]
    fn render_empty_ledger_is_empty_string() {
        assert_eq!(render_ledger(&[], LEDGER_BUDGET_CHARS), "");
    }

    async fn seed_fact(pool: &PgPool, user_id: Uuid, content: &str, domain: &str) {
        let fact = FactInput {
            content: content.into(),
            category: "other".into(),
            domain: domain.into(),
            evidence_type: "explicit_statement".into(),
            temporality: "current".into(),
            persistence: "stable".into(),
            confidence: "high".into(),
        };
        let src = FactSource {
            session_id: None,
            message_id: None,
            run_id: None,
        };
        HumanFactRepo { pool }
            .upsert_run(user_id, &[fact], &src)
            .await
            .unwrap();
    }

    /// A state whose single LLM call is answered with `reply` verbatim.
    async fn state_replying(pool: PgPool, reply: &str) -> (AppState, MockServer) {
        state_replying_after(pool, reply, Duration::ZERO).await
    }

    /// Like `state_replying`, with the reply held back for `delay`.
    async fn state_replying_after(
        pool: PgPool,
        reply: &str,
        delay: Duration,
    ) -> (AppState, MockServer) {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "id": "gen-profile", "model": "p/m",
                        "choices": [{"message": {"content": reply}}],
                    }))
                    .set_delay(delay),
            )
            .expect(1)
            .mount(&mock)
            .await;
        let mut state = crate::routes::companion::test_state(pool);
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.insight_extraction]\nmodel=\"i/m\"\nfilter_prompt=\"p\"\n\
                 [tasks.profile_rebuild]\nmodel=\"p/m\"\n",
            )
            .unwrap(),
        );
        state.openrouter = std::sync::Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "k".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        (state, mock)
    }

    /// The state a rebuild starts from: one pending fact, claimed.
    async fn seed_claim(pool: &PgPool, user_id: Uuid) {
        let state_repo = ProfileStateRepo { pool };
        state_repo.bump(user_id, 1).await.unwrap();
        assert!(state_repo
            .claim(user_id, Duration::from_secs(600))
            .await
            .unwrap());
    }

    async fn count(pool: &PgPool, sql: &str, user_id: Uuid) -> i64 {
        sqlx::query_scalar(sql)
            .bind(user_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test]
    fn audit_users_are_pairwise_distinct() {
        let all = [
            SYSTEM_AUDIT_USER,
            WORLD_AUDIT_USER,
            STORY_AUDIT_USER,
            PROFILE_REBUILD_AUDIT_USER,
        ];
        let distinct: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(distinct.len(), all.len(), "{all:?}");
        assert_eq!(
            PROFILE_REBUILD_AUDIT_USER,
            "11111111-1111-1111-1111-111111111114"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn rebuild_user_with_empty_ledger_is_empty_and_zeroes_counter(pool: PgPool) {
        let user = Uuid::new_v4();
        let state_repo = ProfileStateRepo { pool: &pool };
        state_repo.bump(user, 20).await.unwrap();

        let state = crate::routes::companion::test_state(pool.clone());
        assert_eq!(rebuild_user(&state, user).await, RebuildOutcome::Empty);

        let s = state_repo.load(user).await.unwrap().unwrap();
        assert_eq!((s.pending_facts, s.claimed_at), (0, None));
        assert!(s.last_rebuilt_at.is_some());
        let profiles: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.human_insights WHERE user_id = $1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        let events: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.human_insights_events WHERE user_id = $1",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (profiles, events),
            (0, 0),
            "no ledger, no profile, no audit"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn profile_audit_row_uses_stage_profile_without_session(pool: PgPool) {
        let user = Uuid::new_v4();
        let payload = serde_json::json!({"occupation": "工程师"});
        write_profile_event(&pool, user, "ok", Some(payload.clone()), None).await;

        let (stage, status, session, message, stored): (
            String,
            String,
            Option<Uuid>,
            Option<Uuid>,
            Option<serde_json::Value>,
        ) = sqlx::query_as(
            "SELECT stage, status, session_id, message_id, payload \
             FROM engine.human_insights_events WHERE user_id = $1",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((stage.as_str(), status.as_str()), ("profile", "ok"));
        assert_eq!((session, message), (None, None));
        assert_eq!(stored, Some(payload));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn rebuild_user_writes_profile_and_audits_ok(pool: PgPool) {
        let user = Uuid::new_v4();
        seed_fact(&pool, user, "是后端工程师", "career").await;
        seed_claim(&pool, user).await;

        let reply = serde_json::json!({"occupation": "后端工程师"});
        let (state, mock) = state_replying(pool.clone(), &reply.to_string()).await;
        assert_eq!(rebuild_user(&state, user).await, RebuildOutcome::Ok);

        let sent = mock.received_requests().await.unwrap();
        let body: serde_json::Value = sent[0].body_json().unwrap();
        assert_eq!(body["user"], PROFILE_REBUILD_AUDIT_USER);

        let occupation: Option<String> =
            sqlx::query_scalar("SELECT occupation FROM engine.human_insights WHERE user_id = $1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(occupation.as_deref(), Some("后端工程师"));
        let (status, payload): (String, Option<serde_json::Value>) = sqlx::query_as(
            "SELECT status, payload FROM engine.human_insights_events \
             WHERE user_id = $1 AND stage = 'profile'",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "ok");
        assert_eq!(payload, Some(reply), "audit payload is the model's JSON");
        let s = ProfileStateRepo { pool: &pool }
            .load(user)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((s.pending_facts, s.rebuild_failures), (0, 0));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn rebuild_user_unparseable_reply_fails_and_audits_parse_error(pool: PgPool) {
        let user = Uuid::new_v4();
        seed_fact(&pool, user, "是后端工程师", "career").await;
        ProfileStateRepo { pool: &pool }
            .bump(user, 1)
            .await
            .unwrap();

        let (state, _mock) = state_replying(pool.clone(), "抱歉，我无法完成这个请求。").await;
        assert_eq!(rebuild_user(&state, user).await, RebuildOutcome::Failed);

        let status: String = sqlx::query_scalar(
            "SELECT status FROM engine.human_insights_events \
             WHERE user_id = $1 AND stage = 'profile'",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "parse_error");
        let s = ProfileStateRepo { pool: &pool }
            .load(user)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (s.pending_facts, s.rebuild_failures, s.claimed_at),
            (1, 1, None),
            "a failed rebuild keeps the pending count and releases the claim"
        );
        let profiles: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.human_insights WHERE user_id = $1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(profiles, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn rebuild_user_reply_with_no_supported_field_is_empty_and_zeroes_counter(pool: PgPool) {
        let user = Uuid::new_v4();
        seed_fact(&pool, user, "今天心情一般", "lifestyle").await;
        ProfileStateRepo { pool: &pool }
            .bump(user, 1)
            .await
            .unwrap();

        let (state, _mock) = state_replying(pool.clone(), "{}").await;
        assert_eq!(rebuild_user(&state, user).await, RebuildOutcome::Empty);

        let status: String = sqlx::query_scalar(
            "SELECT status FROM engine.human_insights_events \
             WHERE user_id = $1 AND stage = 'profile'",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "empty");
        let s = ProfileStateRepo { pool: &pool }
            .load(user)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((s.pending_facts, s.rebuild_failures), (0, 0));
        let profiles: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.human_insights WHERE user_id = $1")
                .bind(user)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(profiles, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn rebuild_of_a_user_erased_mid_flight_writes_nothing(pool: PgPool) {
        let user = Uuid::new_v4();
        seed_fact(&pool, user, "是后端工程师", "career").await;
        seed_claim(&pool, user).await;

        let reply = serde_json::json!({"occupation": "后端工程师"});
        let (state, mock) =
            state_replying_after(pool.clone(), &reply.to_string(), Duration::from_millis(600))
                .await;
        let rebuild = tokio::spawn({
            let state = state.clone();
            async move { rebuild_user(&state, user).await }
        });
        // The erase lands after the model call left and before its reply does.
        while mock.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        ErasureRepo { pool: &pool }.erase_user(user).await.unwrap();

        assert_eq!(rebuild.await.unwrap(), RebuildOutcome::Empty);
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM engine.human_insights WHERE user_id = $1",
                user
            )
            .await,
            0,
            "the erased profile stays erased"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM engine.human_insights_events \
                 WHERE user_id = $1 AND stage = 'profile'",
                user
            )
            .await,
            0,
            "no audit row for a profile that was never written"
        );
        assert!(ProfileStateRepo { pool: &pool }
            .load(user)
            .await
            .unwrap()
            .is_none());
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn rebuild_claimed_skips_users_whose_claim_is_gone(pool: PgPool) {
        let (kept, erased) = (Uuid::new_v4(), Uuid::new_v4());
        for user in [kept, erased] {
            seed_fact(&pool, user, "是后端工程师", "career").await;
            seed_claim(&pool, user).await;
        }
        ErasureRepo { pool: &pool }
            .erase_user(erased)
            .await
            .unwrap();

        // The mock allows exactly one model call: the erased user costs none.
        let reply = serde_json::json!({"occupation": "后端工程师"});
        let (state, _mock) = state_replying(pool.clone(), &reply.to_string()).await;
        assert_eq!(
            rebuild_claimed(&state, &[erased, kept]).await,
            (1, 0, 0, 1),
            "(ok, empty, failed, skipped)"
        );
        let profiles = "SELECT count(*) FROM engine.human_insights WHERE user_id = $1";
        assert_eq!(count(&pool, profiles, kept).await, 1);
        assert_eq!(count(&pool, profiles, erased).await, 0);
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Settling a date (spec 2026-11-12-activity-sub-session-design.md §6–7): one
//! call to the activity judge over the sub-session, then, in one transaction,
//! its verdict folded into the parent's affinity, the date's episode, and its
//! journal rows. The activity's claim keeps a second caller out.

use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use chrono_tz::Tz;
use uuid::Uuid;

use eros_engine_core::affinity::{Affinity, AffinityDeltas, AxisGrades, EndpointLevelReads};
use eros_engine_llm::failure::{AttemptFailure, GatewayError, GatewayKind};
use eros_engine_llm::model_config::ACTIVITY_JUDGE_TASK;
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest};
use eros_engine_store::activity::{ActivityRecord, ActivityRepo, SettleClaim};
use eros_engine_store::affinity::{ActivityVerdict, AffinityRepo};
use eros_engine_store::chat::{self, ChatRepo};
use eros_engine_store::episode::{one_line, EpisodeInsert, EpisodeRepo, EPISODE_MAX_CHARS};
use eros_engine_store::journal::{self, JOURNAL_SUMMARY_MAX_CHARS};
use eros_engine_store::persona::PersonaRepo;
use eros_engine_store::scan_state::ScanStateRepo;
use eros_engine_store::session_archive::lock_live_session;

use super::dreaming::parse::LLM_KINDS;
use crate::error::{AppError, StreamPreError};
use crate::prompt::{action_from_metadata, action_subject, user_text_with_action};
use crate::state::AppState;

/// A settlement claim older than this belongs to a caller presumed gone, and
/// the next caller takes it over. Well above the judge's timeout.
pub(crate) const SETTLE_CLAIM_STALE_SECS: i64 = 120;
/// Upper bound on the judge call; on elapse the date settles as a failed call.
pub(crate) const ACTIVITY_JUDGE_TIMEOUT_SECS: u64 = 45;
/// The newest rows of the sub-session the judge reads.
pub(crate) const ACTIVITY_TRANSCRIPT_ROWS: i64 = 120;

/// One milestone the judge read in a date. Its realm and its date are the
/// engine's: `real`, on the day the date started.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DateMoment {
    pub kind: String,
    pub summary: String,
    pub due_on: Option<NaiveDate>,
}

/// The judge's reply, its three parts read independently. The default is
/// what a failed call leaves: no movement, no episode, no moments.
#[derive(Debug, Clone, Default)]
pub(crate) struct DateVerdict {
    pub grades: AxisGrades,
    pub levels: EndpointLevelReads,
    pub reason: String,
    pub episode: Option<String>,
    pub moments: Vec<DateMoment>,
}

/// Read the judge's reply. The verdict goes through the per-turn evaluator's
/// parser, so a malformed axis or level rejects it whole (zero grades, no
/// levels, no reason). `episode` and `moments` are read from the same JSON
/// object on their own, so neither is lost to the other or to the verdict:
/// an episode that is missing, blank or over `EPISODE_MAX_CHARS` is `None`,
/// and each moment stands or falls alone (see [`date_moment`]).
pub(crate) fn parse_date_verdict(raw: &str) -> DateVerdict {
    let (grades, levels, reason) = super::post_process::parse_affinity_eval(raw);
    let reply: Option<serde_json::Value> = super::parse_llm_json(raw);
    let field = |key: &str| reply.as_ref().and_then(|v| v.get(key));
    let episode = field("episode")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|e| !e.is_empty() && e.chars().count() <= EPISODE_MAX_CHARS)
        .map(str::to_string);
    let moments = field("moments")
        .and_then(serde_json::Value::as_array)
        .map(|items| items.iter().filter_map(date_moment).collect())
        .unwrap_or_default();
    DateVerdict {
        grades,
        levels,
        reason,
        episode,
        moments,
    }
}

/// One moment, or `None` to drop it: its kind must be one the scan may write
/// other than `gift` (an accepted give is journaled at its turn), and its
/// summary, on one line, 1 to `JOURNAL_SUMMARY_MAX_CHARS` characters. Only a
/// `promise` or a `shared_plan` keeps a `due_on`, and one that is not a
/// `YYYY-MM-DD` date is dropped, not the moment.
fn date_moment(v: &serde_json::Value) -> Option<DateMoment> {
    let kind = v.get("kind")?.as_str()?.trim();
    if kind == "gift" || !LLM_KINDS.contains(&kind) {
        return None;
    }
    let summary = one_line(v.get("summary")?.as_str()?);
    let len = summary.chars().count();
    if len == 0 || len > JOURNAL_SUMMARY_MAX_CHARS {
        return None;
    }
    let due_on = if matches!(kind, "promise" | "shared_plan") {
        v.get("due_on")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok())
    } else {
        None
    };
    Some(DateMoment {
        kind: kind.to_string(),
        summary,
        due_on,
    })
}

/// A settled date: the activity as stored, the parent's affinity after it
/// (`None` when the parent has no row) and the deltas its event committed
/// (`None` when it wrote none).
#[derive(Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "no reader outside tests"))]
pub(crate) struct Settlement {
    pub activity: ActivityRecord,
    pub affinity: Option<Affinity>,
    pub effective_deltas: Option<AffinityDeltas>,
}

/// Settle the date `activity_id` once, at `now` (spec §6.1–6.2, §7).
///
/// Behind the activity's claim: while another caller holds it this is
/// `409 activity_settling`, and a settled date answers with its stored result
/// (the affinity as it stands and the deltas its event wrote). A date with no
/// substantive turn, or whose parent is archived, is marked settled and
/// nothing moves. Otherwise the judge reads the sub-session and one
/// transaction writes the parent's `activity` event, the episode when one
/// parsed, the `date` journal row and one row per moment, and the settled
/// stamp; a failed call writes the event with no movement and the `date` row
/// alone. An archive that lands while the judge runs is found under the
/// parent's row lock, and the date is then marked settled with nothing else.
/// The write locks the activity's row first: a caller whose stale claim was
/// taken over and settled meanwhile finds the stamp there, writes nothing,
/// and returns the stored result.
#[cfg_attr(not(test), expect(dead_code, reason = "no caller outside tests"))]
pub(crate) async fn settle(
    state: &AppState,
    activity_id: Uuid,
    now: DateTime<Utc>,
) -> Result<Settlement, AppError> {
    let pool = &state.pool;
    let activities = ActivityRepo { pool };
    let mut activity = match activities
        .claim_settlement(activity_id, now, Duration::seconds(SETTLE_CLAIM_STALE_SECS))
        .await?
    {
        SettleClaim::Claimed(a) => a,
        SettleClaim::Held => return Err(settling()),
        SettleClaim::Settled(activity) => return stored_result(state, activity).await,
    };

    let turns = activities
        .substantive_turns(activity.sub_session_id)
        .await?;
    if turns == 0 {
        return settle_quietly(state, activity, now).await;
    }
    // The parent while it is live: whose relationship, and whether a demo.
    let Some((instance_id, is_demo)) = ChatRepo { pool }
        .get_session(activity.session_id)
        .await?
        .and_then(|p| {
            let demo = p.metadata.get("is_demo").and_then(|v| v.as_bool());
            Some((p.instance_id?, demo.unwrap_or(false)))
        })
    else {
        return settle_quietly(state, activity, now).await;
    };

    let (rows, omitted) = activities
        .transcript(activity.sub_session_id, ACTIVITY_TRANSCRIPT_ROWS)
        .await?;
    let (invitation, accept_reply) = invitation_and_reply(state, &activity).await?;
    let persona_name = PersonaRepo { pool }
        .load_companion(instance_id)
        .await?
        .map(|p| p.genome.name)
        .unwrap_or_default();
    let reading = AffinityRepo { pool }
        .load(activity.session_id)
        .await?
        .unwrap_or_else(Affinity::unrecorded);
    let payload = crate::prompt::activity_judge_user_payload(
        &persona_name,
        &reading,
        &invitation,
        &accept_reply,
        &render_transcript(&rows, &persona_name),
        omitted,
    );
    let (verdict, generation_id, failures) = judge(state, activity.session_id, payload).await;

    let episode = match &verdict.episode {
        Some(summary) => {
            episode_row(
                state,
                instance_id,
                activity.sub_session_id,
                &rows,
                omitted,
                summary,
                generation_id.clone(),
            )
            .await?
        }
        None => None,
    };
    let tz: Option<Tz> = ScanStateRepo { pool }
        .load(instance_id)
        .await?
        .and_then(|s| s.user_timezone?.parse().ok());
    let on = journal::local_date(activity.started_at.unwrap_or(activity.created_at), tz);
    let tuning = &state.config.affinity_tuning;
    let (llm_attempts, gateway_errors) = super::stream::split_failures(&failures);
    let growth_scale = super::life_choice::growth_scale_for(state, activity.session_id, now).await;
    let settled = ActivityVerdict {
        activity_id: activity.id,
        grades: verdict.grades,
        levels: verdict.levels,
        weight: tuning.activity_weight(
            u32::try_from(turns).unwrap_or(u32::MAX),
            activity.duration(),
        ),
        turns,
        reason: &verdict.reason,
        boost: if is_demo { tuning.demo_boost } else { 1.0 },
        growth_scale,
        generation_id: generation_id.as_deref(),
        llm_attempts,
        gateway_errors,
        journal_tz: tz,
    };

    let mut tx = pool.begin().await?;
    // A caller that took over a claim this one held past the stale window may
    // have settled the date meanwhile. The row lock orders the two writes, and
    // the later one writes nothing.
    let stamped: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT settled_at FROM engine.activities WHERE id = $1 FOR UPDATE")
            .bind(activity.id)
            .fetch_one(&mut *tx)
            .await?;
    if stamped.is_some() {
        tx.rollback().await?;
        let stored = activities
            .get(activity.id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        return stored_result(state, stored).await;
    }
    let moved = if lock_live_session(&mut tx, activity.session_id).await? {
        let moved =
            AffinityRepo::persist_activity_in_tx(&mut tx, activity.session_id, &settled, tuning)
                .await?;
        if let Some(e) = &episode {
            EpisodeRepo::insert_in_tx(&mut tx, e).await?;
        }
        journal::insert_journal(
            &mut *tx,
            &journal::date(
                instance_id,
                activity.id,
                activity.message_id,
                on,
                &activity.place,
                &activity.content,
            ),
        )
        .await?;
        for m in &verdict.moments {
            journal::insert_journal(
                &mut *tx,
                &journal::date_moment(instance_id, on, &m.kind, &m.summary, m.due_on),
            )
            .await?;
        }
        moved
    } else {
        None
    };
    ActivityRepo::mark_settled_in_tx(&mut tx, activity.id, now).await?;
    tx.commit().await?;

    activity.settled_at = Some(now);
    let (affinity, effective_deltas) = moved.unzip();
    Ok(Settlement {
        activity,
        affinity,
        effective_deltas,
    })
}

/// `409 activity_settling`: another caller is settling the date.
fn settling() -> AppError {
    AppError::StreamPre(StreamPreError {
        status: StatusCode::CONFLICT,
        code: "activity_settling",
        message: "another request is settling this date".into(),
        user_message: "约会正在收尾，请稍后再试".into(),
        original_user_message_id: None,
    })
}

/// A settled date's stored result: the activity as stored, the affinity as it
/// stands, and the deltas its event wrote.
async fn stored_result(state: &AppState, activity: ActivityRecord) -> Result<Settlement, AppError> {
    let effective_deltas = AffinityRepo { pool: &state.pool }
        .activity_event_deltas(activity.session_id, activity.id)
        .await?;
    Ok(Settlement {
        affinity: current_affinity(state, activity.session_id).await?,
        activity,
        effective_deltas,
    })
}

/// Mark the date settled with nothing else written.
async fn settle_quietly(
    state: &AppState,
    mut activity: ActivityRecord,
    now: DateTime<Utc>,
) -> Result<Settlement, AppError> {
    let mut tx = state.pool.begin().await?;
    ActivityRepo::mark_settled_in_tx(&mut tx, activity.id, now).await?;
    tx.commit().await?;
    activity.settled_at = Some(now);
    Ok(Settlement {
        affinity: current_affinity(state, activity.session_id).await?,
        activity,
        effective_deltas: None,
    })
}

/// The parent's affinity as it stands, decayed and refreshed the way every
/// snapshot is read.
async fn current_affinity(
    state: &AppState,
    session_id: Uuid,
) -> Result<Option<Affinity>, AppError> {
    Ok(AffinityRepo { pool: &state.pool }
        .load(session_id)
        .await?
        .map(|mut a| {
            a.apply_time_decay();
            a.refresh_endpoints(&state.config.affinity_tuning);
            a
        }))
}

/// The invitation as her action line states it, and her answer: the
/// assistant rows replying to it, in order, one per line. Both are empty once
/// the invitation row is gone.
async fn invitation_and_reply(
    state: &AppState,
    activity: &ActivityRecord,
) -> Result<(String, String), AppError> {
    let Some(id) = activity.message_id else {
        return Ok((String::new(), String::new()));
    };
    let chat = ChatRepo { pool: &state.pool };
    let invitation = chat
        .message_by_id_in_session(activity.session_id, id)
        .await?
        .and_then(|m| action_from_metadata(m.metadata.as_ref()))
        .map(|a| action_subject(&a))
        .unwrap_or_default();
    let reply = chat
        .replay_chain(id)
        .await?
        .into_iter()
        .map(|r| r.msg.content)
        .filter(|c| !c.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Ok((invitation, reply))
}

/// The date's rows as the judge reads them, one line each in the per-turn
/// evaluator's style: 「对方：」 with any action marker for the user, her name
/// for her. A row with nothing to read is left out.
fn render_transcript(rows: &[chat::ChatMessage], persona_name: &str) -> String {
    rows.iter()
        .filter_map(|m| {
            let (speaker, text) = match m.role.as_str() {
                "user" => (
                    "对方",
                    user_text_with_action(
                        action_from_metadata(m.metadata.as_ref()).as_ref(),
                        &m.content,
                    ),
                ),
                "assistant" => (persona_name, m.content.clone()),
                _ => return None,
            };
            let text = one_line(&text);
            (!text.is_empty()).then(|| format!("{speaker}：{text}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The date's episode, from the sub-session's first row to its last. The
/// transcript holds the last; the first is read on its own when the
/// transcript left rows out. `None` with no rows.
async fn episode_row(
    state: &AppState,
    instance_id: Uuid,
    sub_session_id: Uuid,
    rows: &[chat::ChatMessage],
    omitted: i64,
    summary: &str,
    generation_id: Option<String>,
) -> Result<Option<EpisodeInsert>, AppError> {
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return Ok(None);
    };
    let mut start = (first.id, first.sent_at);
    if omitted > 0 {
        let oldest = ChatRepo { pool: &state.pool }
            .history_slim_before(sub_session_id, first.id, omitted)
            .await?
            .and_then(|older| older.into_iter().next());
        if let Some(m) = oldest {
            start = (m.id, m.sent_at);
        }
    }
    Ok(Some(EpisodeInsert {
        instance_id,
        summary: summary.to_string(),
        started_at: start.1,
        ended_at: last.sent_at,
        first_message_id: Some(start.0),
        last_message_id: Some(last.id),
        generation_id,
    }))
}

/// One call to the activity judge: the parsed verdict, the generation id
/// `record_generation` returned, and every failed attempt. The call hands
/// `execute` the whole `[model] + fallback` chain, as the per-turn evaluator
/// does, so the attempts a fallback recovered from are kept on success too.
/// A failed or timed-out call is the default verdict: nothing moves.
async fn judge(
    state: &AppState,
    session_id: Uuid,
    payload: String,
) -> (DateVerdict, Option<String>, Vec<AttemptFailure>) {
    let resolved = state.model_config.resolve_activity_judge();
    // Kept out of the request so a local timeout still has a slug to name.
    let model_for_audit = resolved.model.clone();
    let req = ChatRequest {
        model: resolved.model,
        fallback_model: resolved.fallback_model,
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: crate::prompt::activity_judge_system_prompt().to_string(),
            },
            ChatMessage {
                role: "user".into(),
                content: payload,
            },
        ],
        temperature: resolved.temperature as f32,
        max_tokens: resolved.max_tokens,
        sampling: resolved.sampling,
        task: Some(ACTIVITY_JUDGE_TASK.into()),
        ..Default::default()
    };
    let timeout = std::time::Duration::from_secs(ACTIVITY_JUDGE_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, state.openrouter.execute(req)).await {
        Ok(Ok(resp)) => {
            let generation_id = super::record_generation(
                &state.pool,
                super::GenerationRecord {
                    task: ACTIVITY_JUDGE_TASK,
                    session_id: Some(session_id),
                    generation_id: resp.generation_id.as_deref(),
                    model: resp.model.as_deref(),
                    usage: resp.usage.as_ref(),
                },
            )
            .await;
            (
                parse_date_verdict(&resp.reply),
                generation_id,
                resp.failures,
            )
        }
        Ok(Err(e)) => {
            tracing::warn!(%session_id, "activity judge call failed: {e}");
            let failures = match &e {
                eros_engine_llm::LlmError::Chain { failures } if !failures.is_empty() => {
                    failures.clone()
                }
                other => vec![AttemptFailure::from_llm_error(
                    ACTIVITY_JUDGE_TASK,
                    &model_for_audit,
                    other,
                )],
            };
            (DateVerdict::default(), None, failures)
        }
        Err(_elapsed) => {
            tracing::warn!(%session_id, "activity judge timed out after {timeout:?}");
            let failure = AttemptFailure::Gateway(GatewayError {
                task: ACTIVITY_JUDGE_TASK.into(),
                model: Some(model_for_audit),
                kind: GatewayKind::TotalTimeout,
                message: format!("activity judge timeout after {ACTIVITY_JUDGE_TIMEOUT_SECS}s"),
            });
            (DateVerdict::default(), None, vec![failure])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::http::StatusCode;
    use chrono::{DateTime, Duration, NaiveDate, Utc};
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;
    use wiremock::matchers::path as wm_path;
    use wiremock::{Mock, MockServer, ResponseTemplate, Times};

    use eros_engine_core::affinity::{AffinityDeltas, AxisGrades, EndpointLevelReads};
    use eros_engine_core::types::DateDuration;
    use eros_engine_store::activity::{ActivityRecord, ActivityRepo, NewActivity};
    use eros_engine_store::affinity::AffinityRepo;
    use eros_engine_store::episode::EPISODE_MAX_CHARS;
    use eros_engine_store::journal::JOURNAL_SUMMARY_MAX_CHARS;
    use eros_engine_store::scan_state::ScanStateRepo;
    use eros_engine_store::session_archive::SessionArchiveRepo;

    use crate::error::AppError;
    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{seed_genome, seed_instance, seed_session};
    use crate::state::AppState;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    // ─── parse_date_verdict ────────────────────────────────────────

    const GRADES: &str = r#""warmth": 3, "trust": {"grade": 2, "direction": "up"}, "intrigue": {"grade": 1, "direction": "up"}, "intimacy": {"grade": 2, "direction": "up"}, "tension": {"grade": 1, "direction": "down"}, "patience": 3, "reason": "他今天很用心""#;

    /// The per-turn verdict plus `extra` keys, as one JSON object.
    fn reply(extra: &str) -> String {
        format!("{{{GRADES}, {extra}}}")
    }

    #[test]
    fn a_full_reply_parses_the_verdict_the_episode_and_the_moments() {
        let raw = format!(
            "好的\n```json\n{}\n```",
            reply(
                r#""episode": "  用户和AI去海边散步，聊到了小时候。 ",
                   "moments": [
                     {"kind": "promise", "summary": "用户答应下次带AI去看日出", "due_on": "2026-11-20"},
                     {"kind": "confession", "summary": "用户说喜欢AI"}
                   ]"#
            )
        );
        let v = parse_date_verdict(&raw);
        assert_eq!(
            v.grades,
            AxisGrades {
                trust: 2,
                intrigue: 1,
                intimacy: 2,
                tension: -1
            }
        );
        assert_eq!(
            v.levels,
            EndpointLevelReads {
                warmth: Some(3),
                patience: Some(3)
            }
        );
        assert_eq!(v.reason, "他今天很用心");
        assert_eq!(
            v.episode.as_deref(),
            Some("用户和AI去海边散步，聊到了小时候。")
        );
        assert_eq!(
            v.moments,
            vec![
                DateMoment {
                    kind: "promise".into(),
                    summary: "用户答应下次带AI去看日出".into(),
                    due_on: Some(date("2026-11-20")),
                },
                DateMoment {
                    kind: "confession".into(),
                    summary: "用户说喜欢AI".into(),
                    due_on: None,
                },
            ]
        );
    }

    #[test]
    fn a_malformed_axis_rejects_the_verdict_but_keeps_the_episode_and_moments() {
        let raw = r#"{"warmth": 2, "trust": {"grade": 9, "direction": "up"}, "patience": 2,
                      "reason": "嗯", "episode": "两人看了一场电影。",
                      "moments": [{"kind": "conflict", "summary": "两人为选片拌了嘴"}]}"#;
        let v = parse_date_verdict(raw);
        assert_eq!(v.grades, AxisGrades::default());
        assert_eq!(v.levels, EndpointLevelReads::default());
        assert_eq!(v.reason, "");
        assert_eq!(v.episode.as_deref(), Some("两人看了一场电影。"));
        assert_eq!(
            v.moments,
            vec![DateMoment {
                kind: "conflict".into(),
                summary: "两人为选片拌了嘴".into(),
                due_on: None,
            }]
        );
    }

    #[test]
    fn moments_drop_gift_unknown_kinds_and_bad_summaries() {
        let at_cap = "约".repeat(JOURNAL_SUMMARY_MAX_CHARS);
        let over = "约".repeat(JOURNAL_SUMMARY_MAX_CHARS + 1);
        let raw = json!({
            "moments": [
                {"kind": "gift", "summary": "用户送了AI一束花"},
                {"kind": "first_meet", "summary": "两人第一次见面"},
                {"kind": "birthday", "summary": "给AI过了生日"},
                {"kind": "promise", "summary": "  \n "},
                {"kind": "promise", "summary": over},
                {"kind": "reconciliation"},
                "not a moment",
                {"kind": "shared_plan", "summary": at_cap, "due_on": "下周末"},
                {"kind": "promise", "summary": "用户答应陪AI跨年", "due_on": 20261231},
                {"kind": "first_intimacy", "summary": "两人第一次接吻", "due_on": "2026-11-20"},
            ]
        })
        .to_string();
        let got: Vec<(String, usize, Option<NaiveDate>)> = parse_date_verdict(&raw)
            .moments
            .into_iter()
            .map(|m| (m.kind, m.summary.chars().count(), m.due_on))
            .collect();
        assert_eq!(
            got,
            vec![
                // A bad due_on keeps the moment, without a date.
                ("shared_plan".into(), JOURNAL_SUMMARY_MAX_CHARS, None),
                ("promise".into(), 9, None),
                // Only a promise or a shared plan comes back on a day.
                ("first_intimacy".into(), 7, None),
            ]
        );
    }

    #[test]
    fn a_missing_or_oversized_episode_is_none() {
        assert_eq!(parse_date_verdict(&reply(r#""moments": []"#)).episode, None);
        let at_cap = "海".repeat(EPISODE_MAX_CHARS);
        assert_eq!(
            parse_date_verdict(&json!({ "episode": at_cap }).to_string()).episode,
            Some(at_cap)
        );
        let over = "海".repeat(EPISODE_MAX_CHARS + 1);
        assert_eq!(
            parse_date_verdict(&json!({ "episode": over }).to_string()).episode,
            None
        );
        assert_eq!(parse_date_verdict(r#"{"episode": "   "}"#).episode, None);
        assert_eq!(parse_date_verdict(r#"{"episode": 3}"#).episode, None);

        let garbage = parse_date_verdict("sorry, I can't");
        assert!(garbage.episode.is_none() && garbage.moments.is_empty());
        assert_eq!(garbage.grades, AxisGrades::default());
    }

    // ─── settle ────────────────────────────────────────────────────

    /// Her accepting reply, which starts the one-hour date: 01:30 on the 13th
    /// in Taipei, still the 12th in UTC.
    const REPLY_AT: &str = "2026-11-12T17:30:00Z";
    /// After the date ended.
    const NOW: &str = "2026-11-12T18:45:00Z";

    const JUDGE_REPLY: &str = r#"{"warmth": 3,
        "trust": {"grade": 2, "direction": "up"}, "intrigue": {"grade": 1, "direction": "up"},
        "intimacy": {"grade": 2, "direction": "up"}, "tension": {"grade": 0, "direction": "up"},
        "patience": 3, "reason": "他今天很用心",
        "episode": "用户和AI去海边散步，聊到了小时候。",
        "moments": [
          {"kind": "promise", "summary": "用户答应下次带AI去看日出", "due_on": "2026-11-20"},
          {"kind": "confession", "summary": "用户说喜欢AI"}
        ]}"#;

    /// The judge on `mock`: `[tasks.affinity_evaluation]` only, so the date is
    /// judged by the evaluator's model.
    fn with_judge(mut state: AppState, mock: &MockServer) -> AppState {
        state.model_config = Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.affinity_evaluation]\nmodel = \"judge\"\n",
            )
            .unwrap(),
        );
        state.openrouter = Arc::new(
            eros_engine_llm::openrouter::OpenRouterClient::with_base_url(
                "test-key".into(),
                format!("{}/api/v1/chat/completions", mock.uri()),
            ),
        );
        state
    }

    async fn judge_mock(template: ResponseTemplate, calls: impl Into<Times>) -> MockServer {
        let mock = MockServer::start().await;
        Mock::given(wm_path("/api/v1/chat/completions"))
            .respond_with(template)
            .expect(calls)
            .mount(&mock)
            .await;
        mock
    }

    fn judged(reply: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "gen-date",
            "model": "served/judge",
            "choices": [{"message": {"content": reply}}],
        }))
    }

    async fn put(
        pool: &PgPool,
        session: Uuid,
        role: &str,
        content: &str,
        at: &str,
        answers: Option<Uuid>,
        metadata: Option<serde_json::Value>,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages \
               (session_id, role, content, sent_at, user_message_id, metadata) \
             VALUES ($1, $2, $3, $4::timestamptz, $5, $6) RETURNING id",
        )
        .bind(session)
        .bind(role)
        .bind(content)
        .bind(at)
        .bind(answers)
        .bind(metadata)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    struct Scene {
        state: AppState,
        pool: PgPool,
        user_id: Uuid,
        instance_id: Uuid,
        session_id: Uuid,
        invite: Uuid,
        activity: ActivityRecord,
        /// The sub-session's rows, oldest first, with their stamps.
        rows: Vec<(Uuid, DateTime<Utc>)>,
    }

    /// A text session with an affinity row and the user's timezone (Taipei),
    /// the invitation and her two-row answer, the short date it opened, and
    /// `turns` answered turns in the sub-session.
    async fn scene(state: AppState, turns: u32) -> Scene {
        let pool = state.pool.clone();
        let user_id = Uuid::new_v4();
        let genome_id = seed_genome(&pool, "Aria").await;
        let instance_id = seed_instance(&pool, genome_id, user_id).await;
        let session_id = seed_session(&pool, user_id, instance_id).await;
        AffinityRepo { pool: &pool }
            .load_or_create(session_id, user_id, instance_id)
            .await
            .unwrap();
        ScanStateRepo { pool: &pool }
            .bump(instance_id, t("2026-11-12T17:29:50Z"), Some("Asia/Taipei"))
            .await
            .unwrap();
        let invite = put(
            &pool,
            session_id,
            "user",
            "去海边走走吧",
            "2026-11-12T17:29:50Z",
            None,
            Some(json!({
                "action": {"type": "date", "place": "海边", "content": "散步", "duration": "short"}
            })),
        )
        .await;
        put(
            &pool,
            session_id,
            "assistant",
            "好呀",
            "2026-11-12T17:29:55Z",
            Some(invite),
            None,
        )
        .await;
        put(
            &pool,
            session_id,
            "assistant",
            "等我换件衣服",
            REPLY_AT,
            Some(invite),
            None,
        )
        .await;
        let activity = ActivityRepo { pool: &pool }
            .accept(
                &NewActivity {
                    session_id,
                    user_id,
                    instance_id,
                    message_id: invite,
                    duration: DateDuration::Short,
                    place: "海边",
                    content: "散步",
                },
                t(REPLY_AT),
            )
            .await
            .unwrap()
            .expect("the date opened");
        let mut rows = Vec::new();
        for n in 0..turns {
            let asked = format!("2026-11-12T17:{:02}:00Z", 31 + 2 * n);
            let answered = format!("2026-11-12T17:{:02}:30Z", 31 + 2 * n);
            let u = put(
                &pool,
                activity.sub_session_id,
                "user",
                &format!("第{n}句"),
                &asked,
                None,
                None,
            )
            .await;
            let a = put(
                &pool,
                activity.sub_session_id,
                "assistant",
                &format!("回你第{n}句"),
                &answered,
                Some(u),
                None,
            )
            .await;
            rows.push((u, t(&asked)));
            rows.push((a, t(&answered)));
        }
        Scene {
            state,
            pool,
            user_id,
            instance_id,
            session_id,
            invite,
            activity,
            rows,
        }
    }

    impl Scene {
        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sql)
                .bind(self.instance_id)
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        /// The parent's `activity` events.
        async fn activity_events(&self) -> i64 {
            self.count(
                "SELECT count(*) FROM engine.companion_affinity_events e \
                   JOIN engine.companion_affinity a ON a.id = e.affinity_id \
                  WHERE a.instance_id = $1 AND e.event_type = 'activity'",
            )
            .await
        }

        async fn episodes(&self) -> i64 {
            self.count("SELECT count(*) FROM engine.relationship_episodes WHERE instance_id = $1")
                .await
        }

        async fn journal_rows(&self) -> i64 {
            self.count("SELECT count(*) FROM engine.relationship_journal WHERE instance_id = $1")
                .await
        }

        async fn settled_at(&self) -> Option<DateTime<Utc>> {
            ActivityRepo { pool: &self.pool }
                .get(self.activity.id)
                .await
                .unwrap()
                .expect("the activity")
                .settled_at
        }
    }

    fn assert_no_movement(d: &AffinityDeltas) {
        for v in [
            d.warmth, d.trust, d.intrigue, d.intimacy, d.patience, d.tension,
        ] {
            assert!(v.abs() < 1e-9, "{d:?}");
        }
    }

    /// An episode row: summary, span, first and last message, generation.
    type EpisodeCols = (
        String,
        DateTime<Utc>,
        DateTime<Utc>,
        Option<Uuid>,
        Option<Uuid>,
        Option<String>,
    );

    /// A journal row: kind, realm, date, summary, due date, message.
    type JournalCols = (
        String,
        String,
        NaiveDate,
        String,
        Option<NaiveDate>,
        Option<Uuid>,
    );

    fn same_deltas(a: &AffinityDeltas, b: &AffinityDeltas) -> bool {
        [
            (a.warmth, b.warmth),
            (a.trust, b.trust),
            (a.intrigue, b.intrigue),
            (a.intimacy, b.intimacy),
            (a.patience, b.patience),
            (a.tension, b.tension),
        ]
        .iter()
        .all(|(x, y)| (x - y).abs() < 1e-12)
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn settle_writes_affinity_episode_and_journal(pool: PgPool) {
        let mock = judge_mock(judged(JUDGE_REPLY), 1).await;
        let s = scene(with_judge(test_state(pool), &mock), 3).await;

        let out = settle(&s.state, s.activity.id, t(NOW)).await.unwrap();
        assert_eq!(out.activity.id, s.activity.id);
        assert_eq!(out.activity.settled_at, Some(t(NOW)));
        assert_eq!(s.settled_at().await, Some(t(NOW)));
        let deltas = out.effective_deltas.expect("the date's deltas");
        assert!(deltas.trust > 0.0 && deltas.intimacy > 0.0, "{deltas:?}");
        let after = out.affinity.expect("the parent's affinity");
        let stored = AffinityRepo { pool: &s.pool }
            .load(s.session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.trust, stored.trust);

        // One `activity` event, worth a quarter of each of the three turns.
        assert_eq!(s.activity_events().await, 1);
        let (context, generation_id, user_message_id): (
            serde_json::Value,
            Option<String>,
            Option<Uuid>,
        ) = sqlx::query_as(
            "SELECT e.context, e.generation_id, e.user_message_id \
               FROM engine.companion_affinity_events e \
               JOIN engine.companion_affinity a ON a.id = e.affinity_id \
              WHERE a.session_id = $1 AND e.event_type = 'activity'",
        )
        .bind(s.session_id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
        assert_eq!(context["activity_id"], s.activity.id.to_string());
        assert_eq!(context["turns"], 3);
        assert_eq!(context["weight"], 0.75);
        assert_eq!(context["reason"], "他今天很用心");
        assert_eq!(generation_id.as_deref(), Some("gen-date"));
        assert_eq!(user_message_id, None);

        // One episode, spanning the sub-session's first and last rows.
        let episodes: Vec<EpisodeCols> = sqlx::query_as(
            "SELECT summary, started_at, ended_at, first_message_id, last_message_id, \
                        generation_id \
                   FROM engine.relationship_episodes WHERE instance_id = $1",
        )
        .bind(s.instance_id)
        .fetch_all(&s.pool)
        .await
        .unwrap();
        let (first, last) = (s.rows[0], s.rows[5]);
        assert_eq!(
            episodes,
            vec![(
                "用户和AI去海边散步，聊到了小时候。".to_string(),
                first.1,
                last.1,
                Some(first.0),
                Some(last.0),
                Some("gen-date".to_string()),
            )]
        );

        // The date and its moments, all real, dated the local day it started.
        // A label first reached writes its own `tier_reached` row beside them.
        let journal: Vec<JournalCols> = sqlx::query_as(
            "SELECT kind, realm, occurred_on, summary, due_on, message_id \
                   FROM engine.relationship_journal \
                  WHERE instance_id = $1 AND kind <> 'tier_reached' ORDER BY kind",
        )
        .bind(s.instance_id)
        .fetch_all(&s.pool)
        .await
        .unwrap();
        let on = date("2026-11-13");
        assert_eq!(
            journal,
            vec![
                (
                    "confession".into(),
                    "real".into(),
                    on,
                    "用户说喜欢AI".into(),
                    None,
                    None
                ),
                (
                    "date".into(),
                    "real".into(),
                    on,
                    "约会：在海边，散步".into(),
                    None,
                    Some(s.invite)
                ),
                (
                    "promise".into(),
                    "real".into(),
                    on,
                    "用户答应下次带AI去看日出".into(),
                    Some(date("2026-11-20")),
                    None
                ),
            ]
        );

        // The evaluator's model read the invitation, her answer and the date.
        let requests = mock.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "judge");
        assert_eq!(
            body["messages"][0]["content"],
            crate::prompt::activity_judge_system_prompt()
        );
        let payload = body["messages"][1]["content"].as_str().unwrap();
        assert!(payload.starts_with("角色名：Aria\n"), "{payload}");
        assert!(
            payload.contains(
                "对方邀请你去约会：地点「海边」，安排「散步」，大约 1 小时\n\
                 你的答复：好呀\n等我换件衣服\n"
            ),
            "{payload}"
        );
        assert!(
            payload.ends_with(
                "约会时的对话：\n对方：第0句\nAria：回你第0句\n对方：第1句\nAria：回你第1句\n\
                 对方：第2句\nAria：回你第2句"
            ),
            "{payload}"
        );
    }

    /// A date longer than the transcript the judge reads: the note of the rows
    /// left out leads it, and the episode still starts at the first row.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_long_date_is_judged_on_its_newest_rows(pool: PgPool) {
        let mock = judge_mock(judged(JUDGE_REPLY), 1).await;
        let s = scene(with_judge(test_state(pool), &mock), 3).await;
        // 121 unanswered rows before the date's three turns.
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             SELECT $1, 'user', '嗯' || n, \
                    '2026-11-12T17:30:01Z'::timestamptz + make_interval(secs => n * 0.25) \
               FROM generate_series(0, 120) n",
        )
        .bind(s.activity.sub_session_id)
        .execute(&s.pool)
        .await
        .unwrap();
        let (first_id, first_at): (Uuid, DateTime<Utc>) = sqlx::query_as(
            "SELECT id, sent_at FROM engine.chat_messages WHERE session_id = $1 \
             ORDER BY sent_at, id LIMIT 1",
        )
        .bind(s.activity.sub_session_id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
        assert_eq!(first_at, t("2026-11-12T17:30:01Z"));

        settle(&s.state, s.activity.id, t(NOW)).await.unwrap();

        // 127 rows, of which the newest 120 are read.
        let requests = mock.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let payload = body["messages"][1]["content"].as_str().unwrap();
        assert!(
            payload.contains("约会时的对话：\n（更早的 7 行省略）\n对方：嗯7\n"),
            "{payload}"
        );
        let (started_at, first_message_id): (DateTime<Utc>, Option<Uuid>) = sqlx::query_as(
            "SELECT started_at, first_message_id FROM engine.relationship_episodes \
              WHERE instance_id = $1",
        )
        .bind(s.instance_id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
        assert_eq!((started_at, first_message_id), (first_at, Some(first_id)));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn settle_without_a_substantive_turn_moves_nothing(pool: PgPool) {
        let mock = judge_mock(judged(JUDGE_REPLY), 0).await;
        let s = scene(with_judge(test_state(pool), &mock), 0).await;
        // Said, but never answered.
        put(
            &s.pool,
            s.activity.sub_session_id,
            "user",
            "到了吗",
            "2026-11-12T17:40:00Z",
            None,
            None,
        )
        .await;

        let out = settle(&s.state, s.activity.id, t(NOW)).await.unwrap();
        assert_eq!(out.activity.settled_at, Some(t(NOW)));
        assert!(out.affinity.is_some(), "the parent's row, as it stands");
        assert!(out.effective_deltas.is_none());
        assert_eq!(s.settled_at().await, Some(t(NOW)));
        assert_eq!(s.activity_events().await, 0);
        assert_eq!(s.episodes().await, 0);
        assert_eq!(s.journal_rows().await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_failed_judge_call_settles_with_no_movement(pool: PgPool) {
        let mock = judge_mock(
            ResponseTemplate::new(500).set_body_string("provider down"),
            1..,
        )
        .await;
        let s = scene(with_judge(test_state(pool), &mock), 3).await;

        let out = settle(&s.state, s.activity.id, t(NOW)).await.unwrap();
        assert_eq!(s.settled_at().await, Some(t(NOW)));
        assert_no_movement(&out.effective_deltas.expect("the event's deltas"));

        assert_eq!(s.activity_events().await, 1);
        let (effective, llm_attempts, generation_id): (
            serde_json::Value,
            Option<serde_json::Value>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT e.effective_deltas, e.llm_attempts, e.generation_id \
               FROM engine.companion_affinity_events e \
               JOIN engine.companion_affinity a ON a.id = e.affinity_id \
              WHERE a.session_id = $1 AND e.event_type = 'activity'",
        )
        .bind(s.session_id)
        .fetch_one(&s.pool)
        .await
        .unwrap();
        assert_no_movement(&serde_json::from_value(effective).unwrap());
        let attempts = llm_attempts.expect("the failed attempt is recorded");
        assert_eq!(attempts[0]["http_status"], 500, "{attempts}");
        assert_eq!(generation_id, None);

        assert_eq!(s.episodes().await, 0);
        let kinds: Vec<String> = sqlx::query_scalar(
            "SELECT kind FROM engine.relationship_journal WHERE instance_id = $1",
        )
        .bind(s.instance_id)
        .fetch_all(&s.pool)
        .await
        .unwrap();
        assert_eq!(kinds, vec!["date".to_string()], "that it took place");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn concurrent_settlements_write_one_event(pool: PgPool) {
        let mock = judge_mock(
            judged(JUDGE_REPLY).set_delay(std::time::Duration::from_millis(300)),
            1,
        )
        .await;
        let s = scene(with_judge(test_state(pool), &mock), 3).await;

        let (a, b) = tokio::join!(
            settle(&s.state, s.activity.id, t(NOW)),
            settle(&s.state, s.activity.id, t(NOW)),
        );
        let (won, held) = match (a, b) {
            (Ok(won), Err(held)) | (Err(held), Ok(won)) => (won, held),
            other => panic!("one settles, the other is held: {other:?}"),
        };
        assert!(
            matches!(
                &held,
                AppError::StreamPre(e)
                    if e.status == StatusCode::CONFLICT && e.code == "activity_settling"
            ),
            "{held:?}"
        );
        let won_deltas = won.effective_deltas.expect("the winner's deltas");

        let again = settle(&s.state, s.activity.id, t(NOW) + Duration::minutes(5))
            .await
            .unwrap();
        assert_eq!(again.activity.settled_at, Some(t(NOW)));
        assert!(again.affinity.is_some());
        let stored = again.effective_deltas.expect("the stored deltas");
        assert!(
            same_deltas(&stored, &won_deltas),
            "{stored:?} vs {won_deltas:?}"
        );
        assert_eq!(s.activity_events().await, 1);
        assert_eq!(s.episodes().await, 1);
    }

    /// A caller that holds its claim past the stale window is taken over, and
    /// both then reach their write: whichever commits second finds the date
    /// settled under the activity's row lock, writes nothing, and returns the
    /// stored result.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_takeover_of_a_stale_claim_writes_one_settlement(pool: PgPool) {
        let mock = judge_mock(
            judged(JUDGE_REPLY).set_delay(std::time::Duration::from_millis(500)),
            2,
        )
        .await;
        let s = scene(with_judge(test_state(pool), &mock), 3).await;
        let past_stale = t(NOW) + Duration::seconds(SETTLE_CLAIM_STALE_SECS + 1);

        let (first, second) = tokio::join!(settle(&s.state, s.activity.id, t(NOW)), async {
            // The first caller holds the claim once it is waiting on the judge.
            while mock.received_requests().await.unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            settle(&s.state, s.activity.id, past_stale).await
        });
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(
            mock.received_requests().await.unwrap().len(),
            2,
            "a takeover"
        );

        let a = first.effective_deltas.expect("the first caller's deltas");
        let b = second.effective_deltas.expect("the second caller's deltas");
        assert!(same_deltas(&a, &b), "{a:?} vs {b:?}");
        assert!(first.activity.settled_at.is_some());
        assert_eq!(first.activity.settled_at, second.activity.settled_at);

        assert_eq!(s.activity_events().await, 1);
        assert_eq!(s.episodes().await, 1);
        assert_eq!(
            s.count(
                "SELECT count(*) FROM engine.relationship_journal \
                  WHERE instance_id = $1 AND kind IN ('promise', 'confession')"
            )
            .await,
            2,
            "one set of moments"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn settle_after_archive_writes_nothing(pool: PgPool) {
        let mock = judge_mock(judged(JUDGE_REPLY), 0).await;
        let s = scene(with_judge(test_state(pool), &mock), 3).await;
        SessionArchiveRepo { pool: &s.pool }
            .archive_relationship(s.user_id, s.instance_id)
            .await
            .unwrap();

        let out = settle(&s.state, s.activity.id, t(NOW)).await.unwrap();
        assert!(out.affinity.is_none() && out.effective_deltas.is_none());
        assert_eq!(s.settled_at().await, Some(t(NOW)));
        assert_eq!(
            s.count("SELECT count(*) FROM engine.companion_affinity WHERE instance_id = $1")
                .await,
            0
        );
        assert_eq!(s.activity_events().await, 0);
        assert_eq!(s.episodes().await, 0);
        assert_eq!(s.journal_rows().await, 0);
    }

    /// An archive that lands while the judge runs: the write finds the parent
    /// archived and settles without writing.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_archive_during_the_judge_call_writes_nothing(pool: PgPool) {
        let mock = judge_mock(
            judged(JUDGE_REPLY).set_delay(std::time::Duration::from_millis(500)),
            0..=1,
        )
        .await;
        let s = scene(with_judge(test_state(pool), &mock), 3).await;

        let (out, _) = tokio::join!(settle(&s.state, s.activity.id, t(NOW)), async {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            SessionArchiveRepo { pool: &s.pool }
                .archive_relationship(s.user_id, s.instance_id)
                .await
                .unwrap();
        });

        let out = out.unwrap();
        assert!(out.affinity.is_none() && out.effective_deltas.is_none());
        assert_eq!(s.settled_at().await, Some(t(NOW)));
        assert_eq!(s.activity_events().await, 0);
        assert_eq!(s.episodes().await, 0);
        assert_eq!(s.journal_rows().await, 0);
    }
}

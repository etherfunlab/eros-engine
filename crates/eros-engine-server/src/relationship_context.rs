// SPDX-License-Identifier: AGPL-3.0-only
//! The relationship's own memory in the chat prompt (spec 2026-10-28
//! memory-2-dreaming-plus §6): the arc, the latest episodes and the journal
//! moments of one persona instance, loaded per 1:1 turn and rendered into the
//! relationship part of `[shared_memories]`.

use chrono_tz::Tz;
use eros_engine_store::arc::{ArcRepo, ArcRow};
use eros_engine_store::episode::{EpisodeRepo, EpisodeRow};
use eros_engine_store::journal::{JournalRepo, JournalRow};
use uuid::Uuid;

use crate::state::AppState;

/// Characters the relationship part of `[shared_memories]` may take (§6.2).
pub(crate) const SHARED_MEMORIES_BUDGET_CHARS: usize = 1_200;
/// Episodes rendered per turn, the newest by `ended_at`.
const RECENT_EPISODES: i64 = 3;
/// Journal rows recalled per turn by cosine over the turn's query embedding.
const JOURNAL_RECALL_K: i32 = 2;

/// Everything the relationship layer adds to a 1:1 turn beyond raw recall.
#[derive(Debug, Clone, Default)]
pub(crate) struct RelationshipContext {
    pub arc: Option<ArcRow>,
    /// Oldest first.
    pub episodes: Vec<EpisodeRow>,
    /// Nearest first.
    pub journal_hits: Vec<JournalRow>,
    /// The real-world rows that come back around, for the anniversary line.
    pub recurring: Vec<JournalRow>,
}

/// Load the relationship context of `instance_id`. `None` when the
/// relationship layer is off. Each read degrades on its own: a store error
/// leaves that part empty with a warn, since relationship memory must never
/// block a reply. The journal search reuses the turn's query embedding and is
/// skipped without one.
pub(crate) async fn load_relationship_context(
    state: &AppState,
    instance_id: Uuid,
    query_embedding: Option<&[f32]>,
    relationship_layer_on: bool,
) -> Option<RelationshipContext> {
    if !relationship_layer_on {
        return None;
    }
    let pool = &state.pool;
    let arc = ArcRepo { pool }
        .load(instance_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, instance_id = %instance_id, "relationship arc load failed; arc omitted");
            None
        });
    let episodes = EpisodeRepo { pool }
        .recent(instance_id, RECENT_EPISODES)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, instance_id = %instance_id, "relationship episodes load failed; episodes omitted");
            Vec::new()
        });
    let journal = JournalRepo { pool };
    let recurring = journal.recurring(instance_id).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, instance_id = %instance_id, "recurring journal load failed; anniversaries omitted");
        Vec::new()
    });
    let journal_hits = match query_embedding {
        Some(query) => journal
            .search(instance_id, query, JOURNAL_RECALL_K)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, instance_id = %instance_id, "journal search failed; journal recall omitted");
                Vec::new()
            }),
        None => Vec::new(),
    };
    let ctx = RelationshipContext {
        arc,
        episodes,
        journal_hits,
        recurring,
    };
    tracing::debug!(
        instance_id = %instance_id,
        arc = ctx.arc.is_some(),
        episodes = ctx.episodes.len(),
        journal_hits = ctx.journal_hits.len(),
        recurring = ctx.recurring.len(),
        "relationship context loaded"
    );
    Some(ctx)
}

/// Render the relationship part of `[shared_memories]` (§6.2): the arc
/// paragraph (`{opening}\n{arc}`), then one `- {date}：{summary}` line per
/// episode (oldest first, dated by `started_at` in `date_tz`), then the
/// journal hits (`（扮演）` marks a roleplay row), then the raw recalled rows
/// as `- {raw}`, all joined by `\n`.
///
/// Over `budget` (characters of the joined text) lines go in this order:
/// episodes oldest first, then raw rows last first, then journal rows last
/// first. The arc paragraph always stays. `None` when nothing renders.
pub(crate) fn render_shared_memories(
    ctx: Option<&RelationshipContext>,
    raw: &[String],
    date_tz: Tz,
    budget: usize,
) -> Option<String> {
    let arc: Option<String> = ctx.and_then(|c| c.arc.as_ref()).and_then(|a| {
        let parts: Vec<&str> = [a.opening.as_str(), a.arc.as_str()]
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    });
    let mut episodes: Vec<String> = ctx
        .map(|c| {
            c.episodes
                .iter()
                .map(|e| {
                    let day = e.started_at.with_timezone(&date_tz).format("%Y-%m-%d");
                    format!("- {day}：{}", e.summary)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut journal: Vec<String> = ctx
        .map(|c| {
            c.journal_hits
                .iter()
                .map(|j| {
                    let mark = if j.realm == "roleplay" {
                        "（扮演）"
                    } else {
                        ""
                    };
                    format!("- {}{mark}：{}", j.occurred_on, j.summary)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut raw: Vec<String> = raw.iter().map(|f| format!("- {f}")).collect();

    loop {
        let text = arc
            .iter()
            .chain(&episodes)
            .chain(&journal)
            .chain(&raw)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        if text.chars().count() <= budget {
            return (!text.is_empty()).then_some(text);
        }
        if !episodes.is_empty() {
            episodes.remove(0);
        } else if raw.pop().is_none() && journal.pop().is_none() {
            // Only the arc is left.
            return Some(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDate, TimeZone, Utc};

    fn arc_row(opening: &str, arc: &str) -> ArcRow {
        ArcRow {
            instance_id: Uuid::nil(),
            opening: opening.into(),
            arc: arc.into(),
            previous_arc: None,
            folded_through: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn episode(started_at: DateTime<Utc>, summary: &str) -> EpisodeRow {
        EpisodeRow {
            id: Uuid::new_v4(),
            instance_id: Uuid::nil(),
            summary: summary.into(),
            started_at,
            ended_at: started_at,
            first_message_id: None,
            last_message_id: None,
            folded: false,
            generation_id: None,
            created_at: started_at,
        }
    }

    fn journal(occurred_on: NaiveDate, realm: &str, summary: &str) -> JournalRow {
        JournalRow {
            id: Uuid::new_v4(),
            instance_id: Uuid::nil(),
            occurred_on,
            kind: "moment".into(),
            realm: realm.into(),
            summary: summary.into(),
            source: "llm".into(),
            message_id: None,
            affinity_event_id: None,
            dedup_key: None,
            due_on: None,
            recurrence: "none".into(),
            created_at: Utc::now(),
        }
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn render_orders_arc_episodes_journal_raw() {
        let ctx = RelationshipContext {
            arc: Some(arc_row("在咖啡店认识", "后来常常聊到深夜")),
            episodes: vec![
                // 20:00 UTC is already the next day in Taipei.
                episode(
                    Utc.with_ymd_and_hms(2026, 10, 11, 20, 0, 0).unwrap(),
                    "一起挑了猫粮",
                ),
                episode(
                    Utc.with_ymd_and_hms(2026, 10, 20, 3, 0, 0).unwrap(),
                    "吵了一架又和好",
                ),
            ],
            journal_hits: vec![journal(day(2026, 8, 15), "real", "第一次送花")],
            recurring: vec![],
        };
        let raw = vec!["用户：今天好累".to_string(), "一起看过电影".to_string()];
        let text = render_shared_memories(
            Some(&ctx),
            &raw,
            chrono_tz::Asia::Taipei,
            SHARED_MEMORIES_BUDGET_CHARS,
        );
        assert_eq!(
            text.as_deref(),
            Some(
                "在咖啡店认识\n后来常常聊到深夜\n\
                 - 2026-10-12：一起挑了猫粮\n\
                 - 2026-10-20：吵了一架又和好\n\
                 - 2026-08-15：第一次送花\n\
                 - 用户：今天好累\n\
                 - 一起看过电影"
            )
        );

        // A blank opening leaves the arc alone on its paragraph.
        let ctx = RelationshipContext {
            arc: Some(arc_row("  ", "后来常常聊到深夜")),
            ..Default::default()
        };
        assert_eq!(
            render_shared_memories(Some(&ctx), &[], chrono_tz::UTC, 1_200).as_deref(),
            Some("后来常常聊到深夜")
        );

        // Raw rows alone render exactly as the plain recall section does.
        let (_, plain) = crate::prompt::render_recall_sections(&[], &raw);
        assert_eq!(
            render_shared_memories(None, &raw, chrono_tz::UTC, 1_200),
            plain
        );
    }

    #[test]
    fn render_marks_roleplay_journal() {
        let ctx = RelationshipContext {
            journal_hits: vec![
                journal(day(2026, 8, 15), "roleplay", "在城堡里求婚"),
                journal(day(2026, 9, 1), "real", "说好一起去海边"),
            ],
            ..Default::default()
        };
        assert_eq!(
            render_shared_memories(Some(&ctx), &[], chrono_tz::UTC, 1_200).as_deref(),
            Some("- 2026-08-15（扮演）：在城堡里求婚\n- 2026-09-01：说好一起去海边")
        );
    }

    #[test]
    fn render_budget_drops_episodes_then_raw_then_journal_never_arc() {
        // Paragraph 99 + 1 + 800 = 900 chars; episode lines 213, journal
        // lines 73, raw lines 102.
        let opening = "o".repeat(99);
        let arc = "a".repeat(800);
        let ctx = RelationshipContext {
            arc: Some(arc_row(&opening, &arc)),
            episodes: (1..=3)
                .map(|d| {
                    episode(
                        Utc.with_ymd_and_hms(2026, 10, d, 12, 0, 0).unwrap(),
                        &d.to_string().repeat(200),
                    )
                })
                .collect(),
            journal_hits: vec![
                journal(day(2026, 8, 1), "real", &"j".repeat(60)),
                journal(day(2026, 8, 2), "real", &"k".repeat(60)),
            ],
            recurring: vec![],
        };
        let raw: Vec<String> = ["x", "y", "z"].iter().map(|c| c.repeat(100)).collect();
        let paragraph = format!("{opening}\n{arc}");
        let j1 = format!("- 2026-08-01：{}", "j".repeat(60));
        let j2 = format!("- 2026-08-02：{}", "k".repeat(60));
        let r1 = format!("- {}", "x".repeat(100));
        let render = |budget| render_shared_memories(Some(&ctx), &raw, chrono_tz::UTC, budget);

        // 1,999 chars in all: every episode goes, then the last two raw rows.
        let survivors = [paragraph.as_str(), &j1, &j2, &r1].join("\n");
        assert_eq!(survivors.chars().count(), 1_151);
        assert_eq!(
            render(SHARED_MEMORIES_BUDGET_CHARS).as_deref(),
            Some(survivors.as_str())
        );

        // Tighter: the raw rows run out, then the journal goes last first.
        assert_eq!(
            render(1_100).as_deref(),
            Some([paragraph.as_str(), &j1, &j2].join("\n").as_str())
        );
        assert_eq!(
            render(1_000).as_deref(),
            Some([paragraph.as_str(), &j1].join("\n").as_str())
        );

        // The arc stays even over budget.
        assert_eq!(render(10).as_deref(), Some(paragraph.as_str()));
    }

    #[test]
    fn render_none_when_empty() {
        assert_eq!(
            render_shared_memories(None, &[], chrono_tz::UTC, 1_200),
            None
        );
        let empty = RelationshipContext::default();
        assert_eq!(
            render_shared_memories(Some(&empty), &[], chrono_tz::UTC, 1_200),
            None
        );
    }
}

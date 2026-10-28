// SPDX-License-Identifier: AGPL-3.0-only
//! Episodes of the relationship scanner (`engine.relationship_episodes`): one
//! short summary per scanned stretch of chat, and the reader that hands the
//! scanner its next stretch of rows.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// Characters an episode summary keeps; the rest is cut on write.
pub const EPISODE_MAX_CHARS: usize = 400;

/// One chat row as the scanner reads it.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ScanRow {
    pub message_id: Uuid,
    pub session_id: Uuid,
    pub role: String,
    pub content: String,
    pub channel: Option<String>,
    pub sent_at: DateTime<Utc>,
    /// `{}` when the row carries none.
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct EpisodeRow {
    pub id: Uuid,
    pub instance_id: Uuid,
    pub summary: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub first_message_id: Option<Uuid>,
    pub last_message_id: Option<Uuid>,
    /// `true` once the arc has absorbed the episode.
    pub folded: bool,
    pub generation_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, instance_id, summary, started_at, ended_at, first_message_id, \
                       last_message_id, folded, generation_id, created_at";

#[derive(Debug, Clone)]
pub struct EpisodeInsert {
    pub instance_id: Uuid,
    pub summary: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub first_message_id: Option<Uuid>,
    pub last_message_id: Option<Uuid>,
    pub generation_id: Option<String>,
}

pub struct EpisodeRepo<'a> {
    pub pool: &'a PgPool,
}

impl EpisodeRepo<'_> {
    /// The relationship's chat rows in `(after, bound]`, oldest first, at most
    /// `limit`, across its live text sessions and, with `include_voice`, its
    /// voice ones. Only what was said counts: user and assistant rows, never
    /// product Q&A.
    pub async fn read_segment(
        &self,
        instance_id: Uuid,
        after: DateTime<Utc>,
        bound: DateTime<Utc>,
        include_voice: bool,
        limit: i64,
    ) -> Result<Vec<ScanRow>, sqlx::Error> {
        sqlx::query_as::<_, ScanRow>(
            "SELECT m.id AS message_id, m.session_id, m.role, m.content, m.channel, \
                    m.sent_at, COALESCE(m.metadata, '{}'::jsonb) AS metadata \
             FROM engine.chat_messages m \
             JOIN engine.chat_sessions s ON s.id = m.session_id \
             WHERE s.instance_id = $1 AND NOT s.archived \
               AND s.channel IN ('text', 'voice') AND ($4 OR s.channel = 'text') \
               AND m.role IN ('user', 'assistant') \
               AND (m.channel IS NULL OR m.channel = 'voice') \
               AND m.sent_at > $2 AND m.sent_at <= $3 \
             ORDER BY m.sent_at, m.id \
             LIMIT $5",
        )
        .bind(instance_id)
        .bind(after)
        .bind(bound)
        .bind(include_voice)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// Store an episode, cutting the summary to `EPISODE_MAX_CHARS`. Only the
    /// scan's claim holder may write: the insert is one statement guarded by
    /// the relationship's live claim, so a worker that lost it writes nothing.
    /// `None` when nothing was inserted.
    pub async fn insert_if_claimed(&self, e: &EpisodeInsert) -> Result<Option<Uuid>, sqlx::Error> {
        let summary: String = e.summary.chars().take(EPISODE_MAX_CHARS).collect();
        sqlx::query_scalar(
            "INSERT INTO engine.relationship_episodes \
                 (instance_id, summary, started_at, ended_at, \
                  first_message_id, last_message_id, generation_id) \
             SELECT $1::uuid, $2::text, $3::timestamptz, $4::timestamptz, \
                    $5::uuid, $6::uuid, $7::text \
             WHERE EXISTS (SELECT 1 FROM engine.relationship_scan_state \
                           WHERE instance_id = $1 AND claimed_at IS NOT NULL) \
             RETURNING id",
        )
        .bind(e.instance_id)
        .bind(summary)
        .bind(e.started_at)
        .bind(e.ended_at)
        .bind(e.first_message_id)
        .bind(e.last_message_id)
        .bind(e.generation_id.as_deref())
        .fetch_optional(self.pool)
        .await
    }

    /// The newest `n` episodes by `ended_at`, returned oldest first.
    pub async fn recent(&self, instance_id: Uuid, n: i64) -> Result<Vec<EpisodeRow>, sqlx::Error> {
        let mut rows = sqlx::query_as::<_, EpisodeRow>(&format!(
            "SELECT {COLUMNS} FROM engine.relationship_episodes \
             WHERE instance_id = $1 \
             ORDER BY ended_at DESC, id DESC \
             LIMIT $2"
        ))
        .bind(instance_id)
        .bind(n)
        .fetch_all(self.pool)
        .await?;
        rows.reverse();
        Ok(rows)
    }

    /// Episodes the arc has not absorbed yet, oldest first.
    pub async fn unfolded(&self, instance_id: Uuid) -> Result<Vec<EpisodeRow>, sqlx::Error> {
        sqlx::query_as::<_, EpisodeRow>(&format!(
            "SELECT {COLUMNS} FROM engine.relationship_episodes \
             WHERE instance_id = $1 AND NOT folded \
             ORDER BY ended_at, id"
        ))
        .bind(instance_id)
        .fetch_all(self.pool)
        .await
    }

    /// The arc has absorbed these episodes.
    pub async fn mark_folded(&self, ids: &[Uuid]) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE engine.relationship_episodes SET folded = true WHERE id = ANY($1)")
            .bind(ids)
            .execute(self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan_state::ScanStateRepo;
    use crate::testutil::{seed_persona_instance, seed_session};
    use std::time::Duration;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    /// A fresh owner with one persona instance.
    async fn seed_relationship(pool: &PgPool) -> (Uuid, Uuid) {
        let owner = Uuid::new_v4();
        (owner, seed_persona_instance(pool, owner).await)
    }

    /// A session on `channel`, archived or not.
    async fn seed_channel_session(
        pool: &PgPool,
        owner: Uuid,
        instance: Uuid,
        channel: &str,
        archived: bool,
    ) -> Uuid {
        let session = seed_session(pool, owner, instance, archived).await;
        sqlx::query("UPDATE engine.chat_sessions SET channel = $2 WHERE id = $1")
            .bind(session)
            .bind(channel)
            .execute(pool)
            .await
            .unwrap();
        session
    }

    async fn seed_msg(
        pool: &PgPool,
        session: Uuid,
        role: &str,
        channel: Option<&str>,
        sent_at: DateTime<Utc>,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel, sent_at) \
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
        )
        .bind(session)
        .bind(role)
        .bind(format!("{role}@{}", sent_at.timestamp()))
        .bind(channel)
        .bind(sent_at)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Open the relationship's scan state and take its claim.
    async fn hold_claim(pool: &PgPool, instance: Uuid) {
        let scan = ScanStateRepo { pool };
        scan.bump(instance, at(1_000), None).await.unwrap();
        assert!(scan
            .claim(instance, Duration::from_secs(600))
            .await
            .unwrap());
    }

    fn episode(instance: Uuid, summary: &str, ended: i64) -> EpisodeInsert {
        EpisodeInsert {
            instance_id: instance,
            summary: summary.to_string(),
            started_at: at(ended - 100),
            ended_at: at(ended),
            first_message_id: None,
            last_message_id: None,
            generation_id: None,
        }
    }

    fn sent(rows: &[ScanRow]) -> Vec<i64> {
        rows.iter().map(|r| r.sent_at.timestamp()).collect()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn segment_orders_across_sessions(pool: PgPool) {
        let repo = EpisodeRepo { pool: &pool };
        let (owner, instance) = seed_relationship(&pool).await;
        let text = seed_channel_session(&pool, owner, instance, "text", false).await;
        let voice = seed_channel_session(&pool, owner, instance, "voice", false).await;
        let t1 = seed_msg(&pool, text, "user", None, at(1_000)).await;
        seed_msg(&pool, voice, "assistant", Some("voice"), at(2_000)).await;
        seed_msg(&pool, text, "assistant", None, at(3_000)).await;
        sqlx::query("UPDATE engine.chat_messages SET metadata = '{\"k\":1}' WHERE id = $1")
            .bind(t1)
            .execute(&pool)
            .await
            .unwrap();

        let all = repo
            .read_segment(instance, at(0), at(9_000), true, 100)
            .await
            .unwrap();
        assert_eq!(sent(&all), [1_000, 2_000, 3_000]);
        assert_eq!(all[0].message_id, t1);
        assert_eq!(all[0].session_id, text);
        assert_eq!(all[0].role, "user");
        assert_eq!(all[0].content, "user@1000");
        assert_eq!(all[0].channel, None);
        assert_eq!(all[0].metadata, serde_json::json!({ "k": 1 }));
        assert_eq!(all[1].session_id, voice);
        assert_eq!(all[1].channel.as_deref(), Some("voice"));
        assert_eq!(
            all[2].metadata,
            serde_json::json!({}),
            "a NULL metadata column reads as an empty object"
        );

        let text_only = repo
            .read_segment(instance, at(0), at(9_000), false, 100)
            .await
            .unwrap();
        assert_eq!(sent(&text_only), [1_000, 3_000]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn segment_respects_cursor_bound_and_limit(pool: PgPool) {
        let repo = EpisodeRepo { pool: &pool };
        let (owner, instance) = seed_relationship(&pool).await;
        let (other_owner, other) = seed_relationship(&pool).await;
        let session = seed_channel_session(&pool, owner, instance, "text", false).await;
        let other_session = seed_channel_session(&pool, other_owner, other, "text", false).await;
        for secs in [1_000, 2_000, 3_000, 4_000, 5_000] {
            seed_msg(&pool, session, "user", None, at(secs)).await;
        }
        seed_msg(&pool, other_session, "user", None, at(3_500)).await;

        let rows = repo
            .read_segment(instance, at(2_000), at(4_000), true, 100)
            .await
            .unwrap();
        assert_eq!(
            sent(&rows),
            [3_000, 4_000],
            "the cursor is exclusive, the bound inclusive, another relationship's rows absent"
        );

        let rows = repo
            .read_segment(instance, at(2_000), at(4_000), true, 1)
            .await
            .unwrap();
        assert_eq!(sent(&rows), [3_000], "the limit keeps the earliest rows");

        let rows = repo
            .read_segment(instance, at(5_000), at(9_000), true, 100)
            .await
            .unwrap();
        assert!(rows.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn segment_skips_archived_sessions_and_product_qa_rows(pool: PgPool) {
        let repo = EpisodeRepo { pool: &pool };
        let (owner, instance) = seed_relationship(&pool).await;
        let live = seed_channel_session(&pool, owner, instance, "text", false).await;
        let archived = seed_channel_session(&pool, owner, instance, "text", true).await;
        seed_msg(&pool, live, "user", None, at(1_000)).await;
        seed_msg(&pool, archived, "user", None, at(2_000)).await;
        seed_msg(&pool, live, "assistant", Some("product_qa"), at(3_000)).await;
        seed_msg(&pool, live, "system_error", None, at(4_000)).await;
        seed_msg(&pool, live, "assistant", None, at(5_000)).await;

        let rows = repo
            .read_segment(instance, at(0), at(9_000), true, 100)
            .await
            .unwrap();
        assert_eq!(sent(&rows), [1_000, 5_000]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn episode_insert_requires_a_claim(pool: PgPool) {
        let repo = EpisodeRepo { pool: &pool };
        let scan = ScanStateRepo { pool: &pool };
        let (owner, instance) = seed_relationship(&pool).await;
        let session = seed_channel_session(&pool, owner, instance, "text", false).await;
        let first = seed_msg(&pool, session, "user", None, at(1_000)).await;
        let last = seed_msg(&pool, session, "assistant", None, at(2_000)).await;
        let count = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM engine.relationship_episodes WHERE instance_id = $1",
            )
            .bind(instance)
            .fetch_one(&pool)
            .await
            .unwrap()
        };

        let e = EpisodeInsert {
            first_message_id: Some(first),
            last_message_id: Some(last),
            generation_id: Some("gen-1".to_string()),
            ..episode(instance, "talked about the move", 2_000)
        };
        assert_eq!(
            repo.insert_if_claimed(&e).await.unwrap(),
            None,
            "no scan state, no episode"
        );

        scan.bump(instance, at(1_000), None).await.unwrap();
        assert_eq!(
            repo.insert_if_claimed(&e).await.unwrap(),
            None,
            "an unclaimed relationship takes no episode"
        );
        assert_eq!(count().await, 0);

        assert!(scan
            .claim(instance, Duration::from_secs(600))
            .await
            .unwrap());
        let id = repo
            .insert_if_claimed(&e)
            .await
            .unwrap()
            .expect("the claim holder writes the episode");
        assert_eq!(count().await, 1);
        let rows = repo.unfolded(instance).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.id, id);
        assert_eq!(row.instance_id, instance);
        assert_eq!(row.summary, "talked about the move");
        assert_eq!((row.started_at, row.ended_at), (at(1_900), at(2_000)));
        assert_eq!(row.first_message_id, Some(first));
        assert_eq!(row.last_message_id, Some(last));
        assert!(!row.folded);
        assert_eq!(row.generation_id.as_deref(), Some("gen-1"));

        assert!(scan.finish_scan_ok(instance, at(2_000), 1).await.unwrap());
        assert_eq!(
            repo.insert_if_claimed(&e).await.unwrap(),
            None,
            "a released claim writes nothing"
        );
        assert_eq!(count().await, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn episode_summary_truncated_to_400_chars(pool: PgPool) {
        let repo = EpisodeRepo { pool: &pool };
        let (_, instance) = seed_relationship(&pool).await;
        hold_claim(&pool, instance).await;

        // Multi-byte on purpose: the cut counts characters, not bytes.
        let long = "汉".repeat(450);
        assert!(long.len() > 450);
        repo.insert_if_claimed(&episode(instance, &long, 2_000))
            .await
            .unwrap()
            .expect("inserted");
        repo.insert_if_claimed(&episode(instance, "short", 3_000))
            .await
            .unwrap()
            .expect("inserted");

        let rows = repo.unfolded(instance).await.unwrap();
        assert_eq!(rows[0].summary.chars().count(), EPISODE_MAX_CHARS);
        assert_eq!(rows[0].summary, "汉".repeat(400));
        assert_eq!(rows[1].summary, "short", "a short summary is kept whole");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_returns_oldest_first_of_last_n(pool: PgPool) {
        let repo = EpisodeRepo { pool: &pool };
        let (_, instance) = seed_relationship(&pool).await;
        let (_, other) = seed_relationship(&pool).await;
        hold_claim(&pool, instance).await;
        hold_claim(&pool, other).await;
        // Inserted out of order: recency is ended_at, not insertion order.
        for (summary, ended) in [("e3", 3_000), ("e1", 1_000), ("e4", 4_000), ("e2", 2_000)] {
            repo.insert_if_claimed(&episode(instance, summary, ended))
                .await
                .unwrap()
                .expect("inserted");
        }
        repo.insert_if_claimed(&episode(other, "foreign", 5_000))
            .await
            .unwrap()
            .expect("inserted");

        let last_two = repo.recent(instance, 2).await.unwrap();
        let summaries: Vec<&str> = last_two.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(summaries, ["e3", "e4"]);

        let all = repo.recent(instance, 10).await.unwrap();
        let summaries: Vec<&str> = all.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(summaries, ["e1", "e2", "e3", "e4"]);

        assert!(repo.recent(instance, 0).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unfolded_then_mark_folded(pool: PgPool) {
        let repo = EpisodeRepo { pool: &pool };
        let (_, instance) = seed_relationship(&pool).await;
        let (_, other) = seed_relationship(&pool).await;
        hold_claim(&pool, instance).await;
        hold_claim(&pool, other).await;
        let mut ids = Vec::new();
        for (summary, ended) in [("e2", 2_000), ("e1", 1_000), ("e3", 3_000)] {
            ids.push(
                repo.insert_if_claimed(&episode(instance, summary, ended))
                    .await
                    .unwrap()
                    .expect("inserted"),
            );
        }
        repo.insert_if_claimed(&episode(other, "foreign", 500))
            .await
            .unwrap()
            .expect("inserted");

        let summaries = |rows: Vec<EpisodeRow>| -> Vec<String> {
            rows.into_iter().map(|e| e.summary).collect()
        };
        assert_eq!(
            summaries(repo.unfolded(instance).await.unwrap()),
            ["e1", "e2", "e3"],
            "oldest first, this relationship only"
        );

        repo.mark_folded(&[]).await.unwrap();
        // ids[0] = e2, ids[1] = e1
        repo.mark_folded(&ids[..2]).await.unwrap();
        assert_eq!(summaries(repo.unfolded(instance).await.unwrap()), ["e3"]);
        assert_eq!(
            summaries(repo.unfolded(other).await.unwrap()),
            ["foreign"],
            "marking one relationship's episodes leaves the other's alone"
        );

        let all = repo.recent(instance, 10).await.unwrap();
        assert_eq!(
            all.iter().map(|e| e.folded).collect::<Vec<_>>(),
            [true, true, false],
            "a folded episode stays readable through recent"
        );
    }
}

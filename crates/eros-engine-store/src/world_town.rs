// SPDX-License-Identifier: AGPL-3.0-only
//! World Town persistence: post publishing, feed reads, comment threads,
//! and the comment-round / reply-responder scheduling primitives.
//!
//! All feed-visible reads and user writes JOIN `world_enrollments` on
//! `town_enabled` — flipping the flag off makes the feed empty immediately
//! while keeping rows (spec town §6).

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeedPost {
    pub post_id: Uuid,
    pub instance_id: Uuid,
    pub author_name: String,
    pub content: String,
    pub published_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeedComment {
    pub comment_id: Uuid,
    pub post_id: Uuid,
    pub author_instance_id: Option<Uuid>,
    pub author_name: Option<String>,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

pub struct WorldTownRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> WorldTownRepo<'a> {
    /// Flip every due post of a town-enabled owner to published (spec §3.1).
    /// Pure SQL, zero LLM. Returns rows published.
    pub async fn publish_due(&self) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            "UPDATE engine.world_posts p SET published_at = now() \
             FROM engine.world_enrollments we \
             WHERE we.owner_uid = p.owner_uid AND we.town_enabled \
               AND p.published_at IS NULL AND p.scheduled_at <= now()",
        )
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// One keyset page of the owner's published feed, newest first
    /// (spec §4). The town_enabled JOIN makes disabled/unenrolled owners an
    /// empty feed, not an error.
    pub async fn feed_page(
        &self,
        owner_uid: Uuid,
        limit: i64,
        cursor: Option<(DateTime<Utc>, Uuid)>,
    ) -> Result<Vec<FeedPost>, sqlx::Error> {
        let (cur_ts, cur_id) = match cursor {
            Some((ts, id)) => (Some(ts), Some(id)),
            None => (None, None),
        };
        sqlx::query_as(
            "SELECT p.id AS post_id, p.instance_id, pg.name AS author_name, \
                    p.content, p.published_at \
             FROM engine.world_posts p \
             JOIN engine.world_enrollments we \
               ON we.owner_uid = p.owner_uid AND we.town_enabled \
             JOIN engine.persona_instances pi ON pi.id = p.instance_id \
             JOIN engine.persona_genomes pg ON pg.id = pi.genome_id \
             WHERE p.owner_uid = $1 AND p.published_at IS NOT NULL \
               AND ($2::timestamptz IS NULL OR (p.published_at, p.id) < ($2, $3)) \
             ORDER BY p.published_at DESC, p.id DESC \
             LIMIT $4",
        )
        .bind(owner_uid)
        .bind(cur_ts)
        .bind(cur_id)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// All comments for a page of posts, thread order (spec §4: threads are
    /// small by construction; no comment pagination in v1).
    pub async fn list_comments_for_posts(
        &self,
        post_ids: &[Uuid],
    ) -> Result<Vec<FeedComment>, sqlx::Error> {
        sqlx::query_as(
            "SELECT c.id AS comment_id, c.post_id, c.author_instance_id, \
                    pg.name AS author_name, c.content, c.created_at \
             FROM engine.world_post_comments c \
             LEFT JOIN engine.persona_instances pi ON pi.id = c.author_instance_id \
             LEFT JOIN engine.persona_genomes pg ON pg.id = pi.genome_id \
             WHERE c.post_id = ANY($1) \
             ORDER BY c.post_id, c.created_at",
        )
        .bind(post_ids)
        .fetch_all(self.pool)
        .await
    }

    /// Insert a user comment (author NULL, source NULL) after validating the
    /// post belongs to this owner's town-enabled world and is published.
    /// `None` = not visible ⇒ caller 404s (spec §4).
    pub async fn insert_user_comment(
        &self,
        owner_uid: Uuid,
        post_id: Uuid,
        content: &str,
    ) -> Result<Option<FeedComment>, sqlx::Error> {
        sqlx::query_as(
            "INSERT INTO engine.world_post_comments (post_id, author_instance_id, source, content) \
             SELECT p.id, NULL, NULL, $3 \
             FROM engine.world_posts p \
             JOIN engine.world_enrollments we \
               ON we.owner_uid = p.owner_uid AND we.town_enabled \
             WHERE p.id = $1 AND p.owner_uid = $2 AND p.published_at IS NOT NULL \
             RETURNING id AS comment_id, post_id, author_instance_id, \
                       NULL::text AS author_name, content, created_at",
        )
        .bind(post_id)
        .bind(owner_uid)
        .bind(content)
        .fetch_optional(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// genome + instance + world enrollment (town on) + world_states backfill.
    pub(super) async fn seed_town_owner(pool: &PgPool) -> (Uuid, Uuid) {
        let owner = Uuid::new_v4();
        let genome: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('Aria','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let inst: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1,$2) RETURNING id",
        )
        .bind(genome)
        .bind(owner)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.world_enrollments (owner_uid, town_enabled) VALUES ($1, true)",
        )
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests) \
             VALUES ($1, '{}'::jsonb, '{}'::jsonb)",
        )
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
        (owner, inst)
    }

    pub(super) async fn seed_post(
        pool: &PgPool,
        owner: Uuid,
        inst: Uuid,
        content: &str,
        published: bool,
    ) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.world_posts \
                 (owner_uid, instance_id, content, scheduled_at, published_at) \
             VALUES ($1, $2, $3, now() - interval '1 hour', \
                     CASE WHEN $4 THEN now() ELSE NULL END) \
             RETURNING id",
        )
        .bind(owner)
        .bind(inst)
        .bind(content)
        .bind(published)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn publish_due_flips_only_town_enabled_due_posts(pool: PgPool) {
        let (owner, inst) = seed_town_owner(&pool).await;
        let due = seed_post(&pool, owner, inst, "due", false).await;
        // Future post: not due.
        let future: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.world_posts (owner_uid, instance_id, content, scheduled_at) \
             VALUES ($1, $2, 'future', now() + interval '1 hour') RETURNING id",
        )
        .bind(owner)
        .bind(inst)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Town-disabled owner: due but must not publish.
        let (owner2, inst2) = seed_town_owner(&pool).await;
        sqlx::query(
            "UPDATE engine.world_enrollments SET town_enabled = false WHERE owner_uid = $1",
        )
        .bind(owner2)
        .execute(&pool)
        .await
        .unwrap();
        let frozen = seed_post(&pool, owner2, inst2, "frozen", false).await;

        let repo = WorldTownRepo { pool: &pool };
        let n = repo.publish_due().await.unwrap();
        assert_eq!(n, 1);
        for (id, expect_published) in [(due, true), (future, false), (frozen, false)] {
            let published: Option<DateTime<Utc>> =
                sqlx::query_scalar("SELECT published_at FROM engine.world_posts WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(published.is_some(), expect_published, "post {id}");
        }
    }

    #[sqlx::test]
    async fn feed_page_keyset_paginates_and_hides_disabled(pool: PgPool) {
        let (owner, inst) = seed_town_owner(&pool).await;
        for i in 0..5 {
            let id = seed_post(&pool, owner, inst, &format!("post {i}"), false).await;
            // Distinct published_at per row for a deterministic keyset order.
            sqlx::query(
                "UPDATE engine.world_posts SET published_at = now() - ($2 || ' minutes')::interval \
                 WHERE id = $1",
            )
            .bind(id)
            .bind((5 - i).to_string())
            .execute(&pool)
            .await
            .unwrap();
        }
        let repo = WorldTownRepo { pool: &pool };
        let page1 = repo.feed_page(owner, 2, None).await.unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page1[0].content, "post 4", "newest first");
        assert_eq!(page1[0].author_name, "Aria");
        let cursor = Some((page1[1].published_at, page1[1].post_id));
        let page2 = repo.feed_page(owner, 2, cursor).await.unwrap();
        assert_eq!(page2.len(), 2);
        assert_eq!(page2[0].content, "post 2", "no overlap, no gap");

        // Unpublished posts are invisible.
        seed_post(&pool, owner, inst, "draft", false).await;
        let all = repo.feed_page(owner, 50, None).await.unwrap();
        assert_eq!(all.len(), 5);

        // Flipping town_enabled off empties the feed but keeps rows.
        sqlx::query(
            "UPDATE engine.world_enrollments SET town_enabled = false WHERE owner_uid = $1",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        assert!(repo.feed_page(owner, 50, None).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn user_comment_validates_visibility_and_round_trips(pool: PgPool) {
        let (owner, inst) = seed_town_owner(&pool).await;
        let post = seed_post(&pool, owner, inst, "hello", true).await;
        let repo = WorldTownRepo { pool: &pool };

        let c = repo
            .insert_user_comment(owner, post, "第一次评论")
            .await
            .unwrap()
            .expect("visible post accepts comment");
        assert_eq!(c.post_id, post);
        assert!(c.author_instance_id.is_none());
        assert!(c.author_name.is_none());

        // Wrong owner ⇒ None.
        assert!(repo
            .insert_user_comment(Uuid::new_v4(), post, "x")
            .await
            .unwrap()
            .is_none());
        // Unpublished ⇒ None.
        let draft = seed_post(&pool, owner, inst, "draft", false).await;
        assert!(repo
            .insert_user_comment(owner, draft, "x")
            .await
            .unwrap()
            .is_none());

        // Thread read joins the author name for persona rows, NULL for user.
        sqlx::query(
            "INSERT INTO engine.world_post_comments (post_id, author_instance_id, source, content) \
             VALUES ($1, $2, 'round', '路过点赞')",
        )
        .bind(post)
        .bind(inst)
        .execute(&pool)
        .await
        .unwrap();
        let thread = repo.list_comments_for_posts(&[post]).await.unwrap();
        assert_eq!(thread.len(), 2);
        assert_eq!(thread[0].content, "第一次评论");
        assert_eq!(thread[1].author_name.as_deref(), Some("Aria"));
    }
}

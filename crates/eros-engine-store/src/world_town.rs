// SPDX-License-Identifier: AGPL-3.0-only
//! World Town persistence: post publishing, feed reads, comment threads,
//! and the comment-round / reply-responder scheduling primitives.
//!
//! All feed-visible reads and user writes JOIN `world_enrollments` on
//! `town_enabled` — flipping the flag off makes the feed empty immediately
//! while keeping rows (spec town §6).

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::time::Duration;
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReplyCandidate {
    pub post_id: Uuid,
    pub owner_uid: Uuid,
    pub author_instance_id: Uuid,
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
            "WITH ins AS ( \
                 INSERT INTO engine.world_post_comments (post_id, author_instance_id, source, content) \
                 SELECT p.id, NULL, NULL, $3 \
                 FROM engine.world_posts p \
                 JOIN engine.world_enrollments we \
                   ON we.owner_uid = p.owner_uid AND we.town_enabled \
                 WHERE p.id = $1 AND p.owner_uid = $2 AND p.published_at IS NOT NULL \
                 RETURNING id, post_id, author_instance_id, content, created_at \
             ), upd AS ( \
                 UPDATE engine.world_posts SET last_user_comment_at = now() \
                 WHERE id = (SELECT post_id FROM ins) \
             ) \
             SELECT id AS comment_id, post_id, author_instance_id, \
                    NULL::text AS author_name, content, created_at FROM ins",
        )
        .bind(post_id)
        .bind(owner_uid)
        .bind(content)
        .fetch_optional(self.pool)
        .await
    }

    /// Owners due for a comment round: town-enabled AND stamp NULL/older than
    /// the round cadence. The per-owner CAS (`claim_comment_round`) is the
    /// authoritative claim; this list is just the scan.
    pub async fn list_round_candidates(&self, round: Duration) -> Result<Vec<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT ws.owner_uid FROM engine.world_states ws \
             JOIN engine.world_enrollments we USING (owner_uid) \
             WHERE we.town_enabled \
               AND (ws.last_comment_round_at IS NULL \
                    OR ws.last_comment_round_at < now() - make_interval(secs => $1))",
        )
        .bind(round.as_secs_f64())
        .fetch_all(self.pool)
        .await
    }

    /// CAS-claim one owner's comment round (spec §3.2): stamp
    /// last_comment_round_at = now() iff still due, returning the PREVIOUS
    /// stamp (the activity-window floor). Outer `None` ⇒ another instance
    /// took it or it is no longer due.
    pub async fn claim_comment_round(
        &self,
        owner_uid: Uuid,
        round: Duration,
    ) -> Result<Option<Option<DateTime<Utc>>>, sqlx::Error> {
        sqlx::query_scalar(
            "UPDATE engine.world_states ws SET last_comment_round_at = now() \
             FROM (SELECT owner_uid, last_comment_round_at AS prev \
                   FROM engine.world_states WHERE owner_uid = $1 FOR UPDATE) old \
             WHERE ws.owner_uid = old.owner_uid \
               AND (old.prev IS NULL OR old.prev < now() - make_interval(secs => $2)) \
             RETURNING old.prev",
        )
        .bind(owner_uid)
        .bind(round.as_secs_f64())
        .fetch_optional(self.pool)
        .await
    }

    /// Anything worth a comment round since `since`? Published posts or user
    /// comments (spec §3.2). `since = None` ⇒ everything counts.
    pub async fn has_town_activity_since(
        &self,
        owner_uid: Uuid,
        since: Option<DateTime<Utc>>,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS( \
                 SELECT 1 FROM engine.world_posts \
                 WHERE owner_uid = $1 AND published_at IS NOT NULL \
                   AND ($2::timestamptz IS NULL OR published_at > $2)) \
             OR EXISTS( \
                 SELECT 1 FROM engine.world_post_comments c \
                 JOIN engine.world_posts p ON p.id = c.post_id \
                 WHERE p.owner_uid = $1 AND c.author_instance_id IS NULL \
                   AND ($2::timestamptz IS NULL OR c.created_at > $2))",
        )
        .bind(owner_uid)
        .bind(since)
        .fetch_one(self.pool)
        .await
    }

    /// Insert one hourly-round persona comment with validation folded into
    /// the INSERT (spec §3.2): post belongs to the owner and is published,
    /// author is one of the owner's ACTIVE instances, and the round path
    /// never self-replies. `false` = rejected (caller warns + drops).
    pub async fn insert_round_comment(
        &self,
        owner_uid: Uuid,
        post_id: Uuid,
        author_instance_id: Uuid,
        content: &str,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            "INSERT INTO engine.world_post_comments \
                 (post_id, author_instance_id, source, content) \
             SELECT p.id, pi.id, 'round', $4 \
             FROM engine.world_posts p \
             JOIN engine.persona_instances pi \
               ON pi.id = $3 AND pi.owner_uid = p.owner_uid AND pi.status = 'active' \
             WHERE p.id = $1 AND p.owner_uid = $2 \
               AND p.published_at IS NOT NULL AND p.instance_id <> $3",
        )
        .bind(post_id)
        .bind(owner_uid)
        .bind(author_instance_id)
        .bind(content)
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Posts whose `last_user_comment_at` stamp has settled past the
    /// debounce, within the activity window, with no persona comment after
    /// it (spec §3.3, issue #176). The stamp IS the latest-user-comment time
    /// (kept current by `insert_user_comment`), so no LATERAL probe is
    /// needed here; `window` bounds the scan to recently active threads —
    /// index-driven via `idx_world_posts_reply` — instead of walking every
    /// published post ever. Author must still be active (§6). Cooldown is
    /// filtered here (excludes posts still inside `last_reply_at`'s cooldown
    /// window) and the scan returns AT MOST ONE candidate per owner via
    /// `DISTINCT ON`, oldest-first across owners — fairness under cap
    /// pressure, so a capped owner with many pending threads occupies at
    /// most one batch slot per tick instead of starving every other owner.
    /// The daily cap itself stays in the pipeline (single source of truth in
    /// `count_replies_today`).
    pub async fn list_reply_candidates(
        &self,
        window: Duration,
        debounce: Duration,
        cooldown: Duration,
        limit: i64,
    ) -> Result<Vec<ReplyCandidate>, sqlx::Error> {
        sqlx::query_as(
            "SELECT post_id, owner_uid, author_instance_id FROM ( \
                 SELECT DISTINCT ON (p.owner_uid) \
                        p.id AS post_id, p.owner_uid, p.instance_id AS author_instance_id, \
                        p.last_user_comment_at \
                 FROM engine.world_posts p \
                 JOIN engine.world_enrollments we \
                   ON we.owner_uid = p.owner_uid AND we.town_enabled \
                 JOIN engine.persona_instances pi \
                   ON pi.id = p.instance_id AND pi.status = 'active' \
                 WHERE p.last_user_comment_at > now() - make_interval(secs => $1) \
                   AND p.last_user_comment_at <= now() - make_interval(secs => $2) \
                   AND (p.last_reply_at IS NULL \
                        OR p.last_reply_at < now() - make_interval(secs => $3)) \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM engine.world_post_comments a \
                       WHERE a.post_id = p.id AND a.author_instance_id IS NOT NULL \
                         AND a.created_at > p.last_user_comment_at) \
                 ORDER BY p.owner_uid, p.last_user_comment_at ASC \
             ) sub \
             ORDER BY last_user_comment_at ASC \
             LIMIT $4",
        )
        .bind(window.as_secs_f64())
        .bind(debounce.as_secs_f64())
        .bind(cooldown.as_secs_f64())
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// Reply-responder comments spent today (UTC day) for this owner —
    /// counts ONLY source = 'reply' rows (spec §3.3 gate 2).
    pub async fn count_replies_today(&self, owner_uid: Uuid) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT count(*) FROM engine.world_post_comments c \
             JOIN engine.world_posts p ON p.id = c.post_id \
             WHERE p.owner_uid = $1 AND c.source = 'reply' \
               AND c.created_at >= (date_trunc('day', now() AT TIME ZONE 'utc') AT TIME ZONE 'utc')",
        )
        .bind(owner_uid)
        .fetch_one(self.pool)
        .await
    }

    /// Thread-cooldown CAS doubling as the multi-instance claim (spec §3.3
    /// gate 3). `true` = this instance owns the response.
    pub async fn claim_reply_cooldown(
        &self,
        post_id: Uuid,
        cooldown: Duration,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            "UPDATE engine.world_posts SET last_reply_at = now() \
             WHERE id = $1 AND (last_reply_at IS NULL OR last_reply_at < now() - make_interval(secs => $2))",
        )
        .bind(post_id)
        .bind(cooldown.as_secs_f64())
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Insert the responder's comment (source = 'reply'). Validation already
    /// happened in `list_reply_candidates` + the cooldown CAS.
    pub async fn insert_reply_comment(
        &self,
        post_id: Uuid,
        author_instance_id: Uuid,
        content: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO engine.world_post_comments \
                 (post_id, author_instance_id, source, content) \
             VALUES ($1, $2, 'reply', $3)",
        )
        .bind(post_id)
        .bind(author_instance_id)
        .bind(content)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// One post with author name, for the reply-responder payload.
    pub async fn get_post(&self, post_id: Uuid) -> Result<Option<FeedPost>, sqlx::Error> {
        sqlx::query_as(
            "SELECT p.id AS post_id, p.instance_id, pg.name AS author_name, \
                    p.content, p.published_at \
             FROM engine.world_posts p \
             JOIN engine.persona_instances pi ON pi.id = p.instance_id \
             JOIN engine.persona_genomes pg ON pg.id = pi.genome_id \
             WHERE p.id = $1 AND p.published_at IS NOT NULL",
        )
        .bind(post_id)
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

    #[sqlx::test]
    async fn claim_comment_round_cas_due_not_due_contended(pool: PgPool) {
        let (owner, _inst) = seed_town_owner(&pool).await;
        let repo = WorldTownRepo { pool: &pool };
        let round = std::time::Duration::from_secs(3600);

        // First claim: NULL prev, claimed.
        let prev = repo.claim_comment_round(owner, round).await.unwrap();
        assert_eq!(prev, Some(None), "first round claims with NULL prev");

        // Immediately after: not due.
        assert_eq!(repo.claim_comment_round(owner, round).await.unwrap(), None);

        // Backdate the stamp past the round: due again, prev returned.
        sqlx::query(
            "UPDATE engine.world_states \
             SET last_comment_round_at = now() - interval '2 hours' WHERE owner_uid = $1",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        let prev = repo.claim_comment_round(owner, round).await.unwrap();
        assert!(matches!(prev, Some(Some(_))), "prev stamp returned");
    }

    #[sqlx::test]
    async fn round_candidates_and_activity_window(pool: PgPool) {
        let (owner, inst) = seed_town_owner(&pool).await;
        let repo = WorldTownRepo { pool: &pool };
        let round = std::time::Duration::from_secs(3600);

        // Never-run owner is a candidate.
        let due = repo.list_round_candidates(round).await.unwrap();
        assert!(due.contains(&owner));

        // NULL since ⇒ any published post counts.
        assert!(!repo.has_town_activity_since(owner, None).await.unwrap());
        let post = seed_post(&pool, owner, inst, "p", true).await;
        assert!(repo.has_town_activity_since(owner, None).await.unwrap());

        // Activity strictly after `since`.
        let future = Utc::now() + chrono::Duration::hours(1);
        assert!(!repo
            .has_town_activity_since(owner, Some(future))
            .await
            .unwrap());
        // A fresh user comment moves the window.
        repo.insert_user_comment(owner, post, "hi")
            .await
            .unwrap()
            .unwrap();
        let past = Utc::now() - chrono::Duration::seconds(30);
        assert!(repo
            .has_town_activity_since(owner, Some(past))
            .await
            .unwrap());
    }

    #[sqlx::test]
    async fn round_comment_insert_validates_author_and_post(pool: PgPool) {
        let (owner, author) = seed_town_owner(&pool).await;
        let (other_owner, foreign_author) = seed_town_owner(&pool).await;
        // Second active persona in owner's world to author valid comments.
        let genome: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('Rin','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let rin: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1,$2) RETURNING id",
        )
        .bind(genome)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        let post = seed_post(&pool, owner, author, "hello", true).await;
        let repo = WorldTownRepo { pool: &pool };

        assert!(repo
            .insert_round_comment(owner, post, rin, "不错")
            .await
            .unwrap());
        // Self-reply via round path rejected.
        assert!(!repo
            .insert_round_comment(owner, post, author, "自评")
            .await
            .unwrap());
        // Foreign author rejected.
        assert!(!repo
            .insert_round_comment(owner, post, foreign_author, "串门")
            .await
            .unwrap());
        // Inactive author rejected.
        sqlx::query("UPDATE engine.persona_instances SET status = 'archived' WHERE id = $1")
            .bind(rin)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!repo
            .insert_round_comment(owner, post, rin, "再来")
            .await
            .unwrap());
        // Unpublished post rejected.
        let draft = seed_post(&pool, owner, author, "draft", false).await;
        assert!(!repo
            .insert_round_comment(owner, draft, rin, "x")
            .await
            .unwrap());
        let _ = other_owner;
    }

    #[sqlx::test]
    async fn reply_scan_debounce_and_persona_after_user_exclusion(pool: PgPool) {
        let (owner, inst) = seed_town_owner(&pool).await;
        let post = seed_post(&pool, owner, inst, "hello", true).await;
        let repo = WorldTownRepo { pool: &pool };
        let window = std::time::Duration::from_secs(604_800);
        let debounce = std::time::Duration::from_secs(90);
        let cooldown = std::time::Duration::from_secs(600);

        // No user comment yet ⇒ no candidate.
        assert!(repo
            .list_reply_candidates(window, debounce, cooldown, 10)
            .await
            .unwrap()
            .is_empty());

        // Fresh user comment (stamp = now(), inside debounce) ⇒ still no candidate.
        repo.insert_user_comment(owner, post, "在吗")
            .await
            .unwrap()
            .unwrap();
        assert!(repo
            .list_reply_candidates(window, debounce, cooldown, 10)
            .await
            .unwrap()
            .is_empty());

        // Age the STAMP past the debounce ⇒ candidate appears.
        sqlx::query(
            "UPDATE engine.world_posts SET last_user_comment_at = now() - interval '3 minutes' \
             WHERE id = $1",
        )
        .bind(post)
        .execute(&pool)
        .await
        .unwrap();
        let cands = repo
            .list_reply_candidates(window, debounce, cooldown, 10)
            .await
            .unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].post_id, post);
        assert_eq!(cands[0].author_instance_id, inst);

        // Persona reply after the user comment clears the candidate.
        repo.insert_reply_comment(post, inst, "在的").await.unwrap();
        assert!(repo
            .list_reply_candidates(window, debounce, cooldown, 10)
            .await
            .unwrap()
            .is_empty());

        // A NEWER user comment re-stamps to now() ⇒ back inside debounce ⇒
        // no candidate until it ages again.
        repo.insert_user_comment(owner, post, "又来")
            .await
            .unwrap()
            .unwrap();
        assert!(repo
            .list_reply_candidates(window, debounce, cooldown, 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[sqlx::test]
    async fn user_comment_stamps_last_user_comment_at(pool: PgPool) {
        let (owner, inst) = seed_town_owner(&pool).await;
        let post = seed_post(&pool, owner, inst, "hello", true).await;
        let repo = WorldTownRepo { pool: &pool };

        let stamp0: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT last_user_comment_at FROM engine.world_posts WHERE id = $1")
                .bind(post)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(stamp0.is_none(), "no user comment ⇒ NULL stamp");

        repo.insert_user_comment(owner, post, "在吗")
            .await
            .unwrap()
            .unwrap();
        let stamp1: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT last_user_comment_at FROM engine.world_posts WHERE id = $1")
                .bind(post)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(stamp1.is_some(), "user comment sets the stamp");

        // Persona comments (round + reply) must NOT move the stamp.
        repo.insert_round_comment_unchecked_for_test(post, inst, "round", "r")
            .await;
        repo.insert_reply_comment(post, inst, "reply")
            .await
            .unwrap();
        let stamp2: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT last_user_comment_at FROM engine.world_posts WHERE id = $1")
                .bind(post)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stamp2, stamp1, "persona comments leave the stamp untouched");

        // Not-visible (unpublished) post ⇒ None and no stamp side effect.
        let unpub = seed_post(&pool, owner, inst, "unpub", false).await;
        assert!(repo
            .insert_user_comment(owner, unpub, "x")
            .await
            .unwrap()
            .is_none());
        let stamp3: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT last_user_comment_at FROM engine.world_posts WHERE id = $1")
                .bind(unpub)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(stamp3.is_none(), "unpublished post never gets a stamp");
    }

    #[sqlx::test]
    async fn reply_cap_counts_only_reply_source_and_cooldown_cas(pool: PgPool) {
        let (owner, inst) = seed_town_owner(&pool).await;
        let post = seed_post(&pool, owner, inst, "hello", true).await;
        let repo = WorldTownRepo { pool: &pool };

        assert_eq!(repo.count_replies_today(owner).await.unwrap(), 0);
        repo.insert_round_comment_unchecked_for_test(post, inst, "round", "round row")
            .await;
        repo.insert_reply_comment(post, inst, "reply row")
            .await
            .unwrap();
        // User rows never count either.
        repo.insert_user_comment(owner, post, "user row")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            repo.count_replies_today(owner).await.unwrap(),
            1,
            "only source='reply'"
        );

        // Cooldown CAS: first claim wins, immediate second claim loses,
        // backdated stamp reopens.
        let cooldown = std::time::Duration::from_secs(600);
        assert!(repo.claim_reply_cooldown(post, cooldown).await.unwrap());
        assert!(!repo.claim_reply_cooldown(post, cooldown).await.unwrap());
        sqlx::query(
            "UPDATE engine.world_posts SET last_reply_at = now() - interval '11 minutes' \
             WHERE id = $1",
        )
        .bind(post)
        .execute(&pool)
        .await
        .unwrap();
        assert!(repo.claim_reply_cooldown(post, cooldown).await.unwrap());
    }

    #[sqlx::test]
    async fn reply_scan_filters_cooldown_and_dedupes_per_owner(pool: PgPool) {
        let (owner_a, inst_a) = seed_town_owner(&pool).await;
        let post1 = seed_post(&pool, owner_a, inst_a, "post1", true).await;
        let post2 = seed_post(&pool, owner_a, inst_a, "post2", true).await;
        let repo = WorldTownRepo { pool: &pool };
        let window = std::time::Duration::from_secs(604_800);
        let debounce = std::time::Duration::from_secs(90);
        let cooldown = std::time::Duration::from_secs(600);

        // Two eligible threads for owner A; post1's user comment is older.
        repo.insert_user_comment(owner_a, post1, "post1 user")
            .await
            .unwrap()
            .unwrap();
        // post1 (owner A) — oldest.
        sqlx::query(
            "UPDATE engine.world_posts SET last_user_comment_at = now() - interval '10 minutes' \
             WHERE id = $1",
        )
        .bind(post1)
        .execute(&pool)
        .await
        .unwrap();
        repo.insert_user_comment(owner_a, post2, "post2 user")
            .await
            .unwrap()
            .unwrap();
        // post2 (owner A).
        sqlx::query(
            "UPDATE engine.world_posts SET last_user_comment_at = now() - interval '3 minutes' \
             WHERE id = $1",
        )
        .bind(post2)
        .execute(&pool)
        .await
        .unwrap();

        // Only ONE candidate surfaces for owner A: the older thread (post1).
        let cands = repo
            .list_reply_candidates(window, debounce, cooldown, 10)
            .await
            .unwrap();
        assert_eq!(cands.len(), 1, "one candidate per owner");
        assert_eq!(cands[0].post_id, post1, "oldest thread wins for owner A");

        // Claiming the cooldown on post1 excludes it from the scan; owner A's
        // other pending thread (post2) surfaces instead — never both.
        assert!(repo.claim_reply_cooldown(post1, cooldown).await.unwrap());
        let cands = repo
            .list_reply_candidates(window, debounce, cooldown, 10)
            .await
            .unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].post_id, post2, "cooldown-excluded post1 skipped");

        // Owner B has one eligible thread, older than owner A's remaining one.
        let (owner_b, inst_b) = seed_town_owner(&pool).await;
        let post_b = seed_post(&pool, owner_b, inst_b, "postB", true).await;
        repo.insert_user_comment(owner_b, post_b, "B user")
            .await
            .unwrap()
            .unwrap();
        // post_b (owner B) — between the two.
        sqlx::query(
            "UPDATE engine.world_posts SET last_user_comment_at = now() - interval '5 minutes' \
             WHERE id = $1",
        )
        .bind(post_b)
        .execute(&pool)
        .await
        .unwrap();

        // Both owners get exactly one slot each, oldest thread first overall.
        let cands = repo
            .list_reply_candidates(window, debounce, cooldown, 10)
            .await
            .unwrap();
        assert_eq!(cands.len(), 2, "one candidate per owner, both eligible");
        assert_eq!(cands[0].post_id, post_b, "owner B's older thread first");
        assert_eq!(cands[0].owner_uid, owner_b);
        assert_eq!(cands[1].post_id, post2);
        assert_eq!(cands[1].owner_uid, owner_a);
    }

    #[sqlx::test]
    async fn reply_scan_excludes_posts_outside_window(pool: PgPool) {
        let (owner, inst) = seed_town_owner(&pool).await;
        let post = seed_post(&pool, owner, inst, "old", true).await;
        let repo = WorldTownRepo { pool: &pool };
        let window = std::time::Duration::from_secs(604_800); // 7d
        let debounce = std::time::Duration::from_secs(90);
        let cooldown = std::time::Duration::from_secs(600);

        repo.insert_user_comment(owner, post, "old comment")
            .await
            .unwrap()
            .unwrap();
        // Stamp older than the window ⇒ dropped (the new bound).
        sqlx::query(
            "UPDATE engine.world_posts SET last_user_comment_at = now() - interval '8 days' \
             WHERE id = $1",
        )
        .bind(post)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            repo.list_reply_candidates(window, debounce, cooldown, 10)
                .await
                .unwrap()
                .is_empty(),
            "user comment older than the window drops out of the scan"
        );

        // A fresh user comment re-stamps to now() (re-enters the window); aged
        // past debounce ⇒ candidate again. Proves the window keys off recent
        // user activity, not post age.
        repo.insert_user_comment(owner, post, "fresh comment")
            .await
            .unwrap()
            .unwrap();
        sqlx::query(
            "UPDATE engine.world_posts SET last_user_comment_at = now() - interval '3 minutes' \
             WHERE id = $1",
        )
        .bind(post)
        .execute(&pool)
        .await
        .unwrap();
        let cands = repo
            .list_reply_candidates(window, debounce, cooldown, 10)
            .await
            .unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].post_id, post);
    }

    #[sqlx::test]
    async fn reply_scan_suppressed_by_round_comment_after_user(pool: PgPool) {
        let (owner, inst) = seed_town_owner(&pool).await;
        // A second active instance to author the round comment.
        let g2: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ('Rin','p','{}'::jsonb) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let commenter: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) VALUES ($1,$2) RETURNING id",
        )
        .bind(g2)
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();

        let post = seed_post(&pool, owner, inst, "hello", true).await;
        let repo = WorldTownRepo { pool: &pool };
        let window = std::time::Duration::from_secs(604_800);
        let debounce = std::time::Duration::from_secs(90);
        let cooldown = std::time::Duration::from_secs(600);

        repo.insert_user_comment(owner, post, "在吗")
            .await
            .unwrap()
            .unwrap();
        sqlx::query(
            "UPDATE engine.world_posts SET last_user_comment_at = now() - interval '3 minutes' \
             WHERE id = $1",
        )
        .bind(post)
        .execute(&pool)
        .await
        .unwrap();
        // A ROUND comment (source='round', NOT tracked by last_reply_at) lands
        // after the user comment. It must suppress the reply — enforced by the
        // NOT EXISTS check, NOT by last_reply_at.
        repo.insert_round_comment_unchecked_for_test(post, commenter, "round", "round after user")
            .await;
        assert!(
            repo.list_reply_candidates(window, debounce, cooldown, 10)
                .await
                .unwrap()
                .is_empty(),
            "a round comment after the user comment suppresses the reply"
        );
    }

    // Test-only impl block for helper methods used in tests.
    impl<'a> WorldTownRepo<'a> {
        /// Test-only: raw insert bypassing round validation, for cap counting.
        async fn insert_round_comment_unchecked_for_test(
            &self,
            post_id: Uuid,
            author: Uuid,
            source: &str,
            content: &str,
        ) {
            sqlx::query(
                "INSERT INTO engine.world_post_comments \
                     (post_id, author_instance_id, source, content) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(post_id)
            .bind(author)
            .bind(source)
            .bind(content)
            .execute(self.pool)
            .await
            .unwrap();
        }
    }
}

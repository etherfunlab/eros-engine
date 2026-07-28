// SPDX-License-Identifier: AGPL-3.0-only
//! World Memories persistence: enrollment reads, director scheduling state,
//! and script-fragment storage/recall.
//!
//! Ownership split (spec §1): `world_enrollments` is downstream-written and
//! engine-read; `world_states` / `world_memories` are engine-private.
//! Scheduling uses the dreaming-lite claim pattern — a single
//! `UPDATE ... WHERE ... IN (SELECT ... FOR UPDATE SKIP LOCKED)` statement so
//! concurrent engine instances claim disjoint owners.

use crate::memory::format_vector;
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RosterEntry {
    pub instance_id: Uuid,
    pub name: String,
    pub tip_personality: Option<String>,
    pub art_metadata: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct FragmentInsert {
    pub instance_id: Uuid,
    pub content: String,
    pub embedding: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct PostInsert {
    pub instance_id: Uuid,
    pub content: String,
    pub scheduled_at: DateTime<Utc>,
}

pub struct WorldRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> WorldRepo<'a> {
    /// Backfill a `world_states` row for every enrollment that lacks one.
    /// Empty seed/digests (`{}`) marks a never-run world; the director prompt
    /// takes an "initialize world" branch for it. Returns rows inserted.
    pub async fn ensure_states_for_enrollments(&self) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests) \
             SELECT owner_uid, '{}'::jsonb, '{}'::jsonb FROM engine.world_enrollments \
             ON CONFLICT (owner_uid) DO NOTHING",
        )
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Atomically claim up to `batch` due owners: enrolled AND has a
    /// non-blank worldview AND (past their interval OR the worldview was
    /// touched since the last run) AND not freshly claimed. Same statement
    /// shape as the dreaming picker: concurrent sweepers see disjoint sets.
    /// A content-identical touch (trigger only bumps `updated_at` on real
    /// change, see Task 1) still costs at most one extra normal round —
    /// `mark_ran`/`persist_round` advance `last_run_at` past it, so it
    /// converges. Returns each owner alongside the `claimed_at` timestamp
    /// just written — the caller's ownership token, threaded back into
    /// `release_claim`/`mark_ran`/`persist_round` so a worker that outlives
    /// the stale window can never clobber a newer sweeper's claim on the
    /// same owner.
    pub async fn claim_due(
        &self,
        interval: Duration,
        stale: Duration,
        batch: i64,
    ) -> Result<Vec<(Uuid, DateTime<Utc>)>, sqlx::Error> {
        let now = Utc::now();
        let due_cutoff: DateTime<Utc> =
            now - chrono::Duration::from_std(interval).unwrap_or_default();
        let stale_cutoff: DateTime<Utc> =
            now - chrono::Duration::from_std(stale).unwrap_or_default();
        sqlx::query_as(
            "UPDATE engine.world_states SET claimed_at = now() \
             WHERE owner_uid IN ( \
                 SELECT ws.owner_uid FROM engine.world_states ws \
                 JOIN engine.world_enrollments we USING (owner_uid) \
                 JOIN engine.world_worldviews ww USING (owner_uid) \
                 WHERE btrim(ww.content, E' \\t\\r\\n\\f\\v') <> '' \
                   AND (ws.last_run_at IS NULL OR ws.last_run_at < $1 \
                        OR ww.updated_at > ws.last_run_at) \
                   AND (ws.claimed_at IS NULL OR ws.claimed_at < $2) \
                 ORDER BY ws.last_run_at ASC NULLS FIRST \
                 LIMIT $3 \
                 FOR UPDATE SKIP LOCKED \
             ) \
             RETURNING owner_uid, claimed_at",
        )
        .bind(due_cutoff)
        .bind(stale_cutoff)
        .bind(batch)
        .fetch_all(self.pool)
        .await
    }

    /// Reset the claim after a failed round so the owner retries at the next
    /// due scan instead of waiting out the stale window. Guarded on the
    /// ownership token: if a newer sweeper has since reclaimed this owner
    /// (past `WORLD_CLAIM_STALE`), `claimed_at` no longer matches and this is
    /// a no-op — we must not clear a claim we no longer hold.
    pub async fn release_claim(
        &self,
        owner_uid: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.world_states SET claimed_at = NULL \
             WHERE owner_uid = $1 AND claimed_at = $2",
        )
        .bind(owner_uid)
        .bind(claimed_at)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Stamp a round that produced nothing to persist (e.g. empty roster):
    /// advances last_run_at and clears the claim without touching the seed.
    /// Guarded on the ownership token — see `release_claim`.
    pub async fn mark_ran(
        &self,
        owner_uid: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.world_states \
             SET last_run_at = now(), claimed_at = NULL, updated_at = now() \
             WHERE owner_uid = $1 AND claimed_at = $2",
        )
        .bind(owner_uid)
        .bind(claimed_at)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Current seed for the director prompt. `None` when no state row exists.
    pub async fn load_seed(
        &self,
        owner_uid: Uuid,
    ) -> Result<Option<serde_json::Value>, sqlx::Error> {
        sqlx::query_scalar("SELECT seed FROM engine.world_states WHERE owner_uid = $1")
            .bind(owner_uid)
            .fetch_optional(self.pool)
            .await
    }

    /// Whether this owner has opted into the town feed. Unenrolled ⇒ false.
    pub async fn town_enabled(&self, owner_uid: Uuid) -> Result<bool, sqlx::Error> {
        let v: Option<bool> = sqlx::query_scalar(
            "SELECT town_enabled FROM engine.world_enrollments WHERE owner_uid = $1",
        )
        .bind(owner_uid)
        .fetch_optional(self.pool)
        .await?;
        Ok(v.unwrap_or(false))
    }

    /// Whether this owner has opted into the stories layer. Unenrolled ⇒ false.
    pub async fn stories_enabled(&self, owner_uid: Uuid) -> Result<bool, sqlx::Error> {
        let v: Option<bool> = sqlx::query_scalar(
            "SELECT stories_enabled FROM engine.world_enrollments WHERE owner_uid = $1",
        )
        .bind(owner_uid)
        .fetch_optional(self.pool)
        .await?;
        Ok(v.unwrap_or(false))
    }

    /// The owner's current worldview (trimmed) plus the hash recorded by the
    /// last completed round, plus the worldview row's own `updated_at`.
    /// `None` = no usable worldview (absent row or blank content): the
    /// caller must not run any World System LLM round for this owner (spec
    /// §1). The caller that runs a director round must thread
    /// `updated_at` back into `persist_round`'s `worldview_updated_at` —
    /// that guard is what keeps a mid-round downstream update from
    /// committing stale output (see `persist_round`).
    pub async fn worldview_state(
        &self,
        owner_uid: Uuid,
    ) -> Result<Option<(String, Option<String>, DateTime<Utc>)>, sqlx::Error> {
        let row: Option<(String, Option<String>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT ww.content, ws.worldview_hash, ww.updated_at \
             FROM engine.world_worldviews ww \
             JOIN engine.world_states ws USING (owner_uid) \
             WHERE ww.owner_uid = $1",
        )
        .bind(owner_uid)
        .fetch_optional(self.pool)
        .await?;
        Ok(row.and_then(|(content, hash, updated_at)| {
            let trimmed = content
                .trim_matches(|c: char| c.is_ascii_whitespace())
                .to_string();
            (!trimmed.is_empty()).then_some((trimmed, hash, updated_at))
        }))
    }

    /// Enrolled owners with no usable worldview — the sweeper's aggregate
    /// warn counter (spec §3). Worldview-less owners are excluded from every
    /// claim/candidate query, so this count is the only place they surface.
    pub async fn count_enrolled_missing_worldview(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT count(*) FROM engine.world_enrollments we \
             LEFT JOIN engine.world_worldviews ww USING (owner_uid) \
             WHERE ww.owner_uid IS NULL OR btrim(ww.content, E' \\t\\r\\n\\f\\v') = ''",
        )
        .fetch_one(self.pool)
        .await
    }

    /// The owner's active persona roster (earliest-created first) joined to
    /// genome display data. Caller passes cap+1 and truncates so it can log
    /// the spec's roster-cap warning.
    pub async fn list_active_roster(
        &self,
        owner_uid: Uuid,
        limit: i64,
    ) -> Result<Vec<RosterEntry>, sqlx::Error> {
        sqlx::query_as(
            "SELECT pi.id AS instance_id, pg.name, pg.tip_personality, pg.art_metadata \
             FROM engine.persona_instances pi \
             JOIN engine.persona_genomes pg ON pg.id = pi.genome_id \
             WHERE pi.owner_uid = $1 AND pi.status = 'active' \
             ORDER BY pi.created_at ASC \
             LIMIT $2",
        )
        .bind(owner_uid)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// Memory feedback for the director: the owner's most recent extracted
    /// profile-layer rows (dreaming-lite output; `category IS NOT NULL`).
    /// Relationship-layer rows are raw user lines and are deliberately
    /// excluded (spec §0).
    pub async fn recent_extracted_memories(
        &self,
        owner_uid: Uuid,
        k: i64,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT content FROM engine.companion_memories \
             WHERE user_id = $1 AND instance_id IS NULL AND category IS NOT NULL \
             ORDER BY created_at DESC LIMIT $2",
        )
        .bind(owner_uid)
        .bind(k)
        .fetch_all(self.pool)
        .await
    }

    /// Persist one director round in a single transaction (spec §2.4):
    /// reset purge (if `reset`) + retention delete + fragment inserts +
    /// scheduled-post inserts + state update (seed_version++, last_run_at=
    /// now, claimed_at=NULL, worldview_hash stamped, worldview_set_at
    /// stamped iff `reset`). All-or-nothing: any failure rolls back and the
    /// caller releases the claim. `posts` (town-enabled owners only; empty
    /// otherwise) ride the same transaction and the same claim-ownership-
    /// token guard as everything else here.
    ///
    /// `worldview_hash` is stamped onto `world_states` every round (spec
    /// §2). `reset` is true exactly when this round's worldview hash
    /// differs from the previously stored one (or none was stored) —
    /// caller's job (Task 6); this method just executes the purge + era
    /// stamp for whatever `reset` it is given. When `reset`, the purge below
    /// runs first in the same transaction, and `worldview_set_at` is bumped
    /// to `now()` to mark the new era's start.
    ///
    /// `claimed_at` is the ownership token from `claim_due`. The final state
    /// update is guarded on it (`AND claimed_at = $N`); if a newer sweeper
    /// has since reclaimed this owner the guard matches zero rows and we
    /// return `Err(RowNotFound)` BEFORE committing — the transaction is
    /// dropped, which rolls back the reset purge, retention delete, fragment
    /// inserts, and post inserts too, so a lost-claim round writes nothing.
    ///
    /// `worldview_updated_at` is the worldview row's `updated_at` as read by
    /// the caller alongside the content that produced this round's output
    /// (`WorldRepo::worldview_state`). Before doing any purge/insert work,
    /// this method re-checks — `FOR SHARE`, inside this same transaction —
    /// that the worldview row still has that exact `updated_at`. If it
    /// doesn't (the worldview changed, or the row vanished, between the
    /// caller's read and this commit), the round's output was generated
    /// from now-stale content: we abort with `Err(RowNotFound)` and never
    /// commit. This matters because `persist_round` also stamps
    /// `last_run_at = now()`; if a stale round were allowed to commit,
    /// `last_run_at` would land AFTER the new worldview's `updated_at`, so
    /// `claim_due`'s touch-dueness condition (`ww.updated_at >
    /// ws.last_run_at`) would never fire and the fresh worldview would sit
    /// unprocessed for up to a full `interval_hours`. Aborting instead
    /// leaves `last_run_at` untouched, so the owner re-claims on the very
    /// next tick and picks up the new content. `FOR SHARE` also closes the
    /// race window itself: it blocks a concurrent downstream `UPDATE` on
    /// that row from committing until this transaction ends, so there is no
    /// gap between "we checked" and "we committed" for a change to sneak
    /// through.
    #[allow(clippy::too_many_arguments)]
    pub async fn persist_round(
        &self,
        owner_uid: Uuid,
        seed: &serde_json::Value,
        digests: &serde_json::Value,
        fragments: &[FragmentInsert],
        posts: &[PostInsert],
        script_date: NaiveDate,
        retention_days: u32,
        worldview_hash: &str,
        reset: bool,
        worldview_updated_at: DateTime<Utc>,
        claimed_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let still_fresh: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM engine.world_worldviews \
             WHERE owner_uid = $1 AND updated_at = $2 FOR SHARE",
        )
        .bind(owner_uid)
        .bind(worldview_updated_at)
        .fetch_optional(&mut *tx)
        .await?;
        if still_fresh.is_none() {
            // The worldview changed (or the row vanished) between the
            // caller's `worldview_state` read and now: this round's output
            // is stale. Drop `tx` uncommitted — see the doc comment above
            // for why aborting here (rather than committing) is what keeps
            // touch-dueness correct.
            return Err(sqlx::Error::RowNotFound);
        }
        if reset {
            // Worldview changed (spec §2 reset inventory): the fictional
            // world restarts. Old fragments must stop injecting into chat;
            // scheduled-but-unpublished posts carry stale-era content and
            // must not surface; published posts + comments stay as the
            // feed's history. Story rows purge so lives re-derive under the
            // new worldview — deleting a claimed persona_story_insights row
            // mid-story-round is safe: that round's token-guarded UPDATE
            // then matches zero rows and rolls itself back.
            sqlx::query("DELETE FROM engine.world_memories WHERE owner_uid = $1")
                .bind(owner_uid)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "DELETE FROM engine.world_posts WHERE owner_uid = $1 AND published_at IS NULL",
            )
            .bind(owner_uid)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM engine.persona_story_memories WHERE owner_uid = $1")
                .bind(owner_uid)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM engine.persona_story_events WHERE owner_uid = $1")
                .bind(owner_uid)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM engine.persona_story_insights WHERE owner_uid = $1")
                .bind(owner_uid)
                .execute(&mut *tx)
                .await?;
        }
        let retention_cutoff = script_date - chrono::Days::new(u64::from(retention_days));
        sqlx::query("DELETE FROM engine.world_memories WHERE owner_uid = $1 AND script_date < $2")
            .bind(owner_uid)
            .bind(retention_cutoff)
            .execute(&mut *tx)
            .await?;
        for frag in fragments {
            sqlx::query(
                "INSERT INTO engine.world_memories \
                     (owner_uid, instance_id, content, embedding, script_date) \
                 VALUES ($1, $2, $3, $4::vector, $5)",
            )
            .bind(owner_uid)
            .bind(frag.instance_id)
            .bind(&frag.content)
            .bind(format_vector(&frag.embedding))
            .bind(script_date)
            .execute(&mut *tx)
            .await?;
        }
        for post in posts {
            sqlx::query(
                "INSERT INTO engine.world_posts \
                     (owner_uid, instance_id, content, scheduled_at) \
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(owner_uid)
            .bind(post.instance_id)
            .bind(&post.content)
            .bind(post.scheduled_at)
            .execute(&mut *tx)
            .await?;
        }
        let res = sqlx::query(
            "UPDATE engine.world_states \
             SET seed = $2, digests = $3, seed_version = seed_version + 1, \
                 last_run_at = now(), claimed_at = NULL, updated_at = now(), \
                 worldview_hash = $5, \
                 worldview_set_at = CASE WHEN $6 THEN now() ELSE worldview_set_at END \
             WHERE owner_uid = $1 AND claimed_at = $4",
        )
        .bind(owner_uid)
        .bind(seed)
        .bind(digests)
        .bind(claimed_at)
        .bind(worldview_hash)
        .bind(reset)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            // Lost the claim mid-round (a newer sweeper reclaimed this owner
            // past WORLD_CLAIM_STALE). Do not commit — drop `tx` so the
            // retention delete + fragment inserts above roll back too.
            return Err(sqlx::Error::RowNotFound);
        }
        tx.commit().await
    }

    /// Chat-time resident digest for one persona. Single query that also
    /// performs the enrollment check (JOIN): unenrolled ⇒ `None`.
    pub async fn fetch_digest(
        &self,
        owner_uid: Uuid,
        instance_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        let row: Option<Option<String>> = sqlx::query_scalar(
            "SELECT ws.digests ->> $2 FROM engine.world_states ws \
             JOIN engine.world_enrollments we USING (owner_uid) \
             WHERE ws.owner_uid = $1",
        )
        .bind(owner_uid)
        .bind(instance_id.to_string())
        .fetch_optional(self.pool)
        .await?;
        Ok(row.flatten().filter(|d| !d.trim().is_empty()))
    }

    /// Cosine top-k script fragments for one persona.
    pub async fn search_fragments(
        &self,
        owner_uid: Uuid,
        instance_id: Uuid,
        query_embedding: &[f32],
        k: i32,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT content FROM engine.world_memories \
             WHERE owner_uid = $1 AND instance_id = $2 \
             ORDER BY embedding <=> $3::vector \
             LIMIT $4",
        )
        .bind(owner_uid)
        .bind(instance_id)
        .bind(format_vector(query_embedding))
        .bind(k as i64)
        .fetch_all(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn enroll(pool: &PgPool, owner: Uuid) {
        sqlx::query("INSERT INTO engine.world_enrollments (owner_uid) VALUES ($1)")
            .bind(owner)
            .execute(pool)
            .await
            .unwrap();
        set_worldview(pool, owner, "现代都市").await;
    }

    /// Enrollment WITHOUT a worldview — the skip case (spec §1).
    async fn enroll_without_worldview(pool: &PgPool, owner: Uuid) {
        sqlx::query("INSERT INTO engine.world_enrollments (owner_uid) VALUES ($1)")
            .bind(owner)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn set_worldview(pool: &PgPool, owner: Uuid, content: &str) {
        sqlx::query(
            "INSERT INTO engine.world_worldviews (owner_uid, content) VALUES ($1, $2) \
             ON CONFLICT (owner_uid) DO UPDATE SET content = EXCLUDED.content",
        )
        .bind(owner)
        .bind(content)
        .execute(pool)
        .await
        .unwrap();
    }

    /// `persist_round`'s freshness guard needs the worldview row's current
    /// `updated_at` — tests fetch it right after `enroll`/`set_worldview` so
    /// the value they pass in matches what a real caller would have read
    /// via `worldview_state` moments earlier.
    async fn worldview_updated_at(pool: &PgPool, owner: Uuid) -> DateTime<Utc> {
        sqlx::query_scalar("SELECT updated_at FROM engine.world_worldviews WHERE owner_uid = $1")
            .bind(owner)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    const DAY: Duration = Duration::from_secs(24 * 3600);
    const STALE: Duration = Duration::from_secs(1800);

    async fn seed_instance(pool: &PgPool, owner: Uuid, name: &str, status: &str) -> Uuid {
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ($1, 'sp', '{\"backstory\":\"bs\"}'::jsonb) RETURNING id",
        )
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid, status) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(genome_id)
        .bind(owner)
        .bind(status)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn unit_embedding(seed: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; 512];
        v[seed % 512] = 1.0;
        v
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ensure_states_backfills_only_missing(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        enroll(&pool, a).await;
        enroll(&pool, b).await;

        assert_eq!(repo.ensure_states_for_enrollments().await.unwrap(), 2);
        // Idempotent: second run inserts nothing.
        assert_eq!(repo.ensure_states_for_enrollments().await.unwrap(), 0);

        let seed = repo.load_seed(a).await.unwrap().unwrap();
        assert_eq!(seed, serde_json::json!({}), "fresh world has empty seed");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_claims_never_run_enrolled_owner_once(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();

        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert_eq!(
            claimed.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
            vec![owner],
            "never-run world is due"
        );

        // Immediately re-claiming yields nothing (claimed_at is fresh).
        let again = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert!(again.is_empty(), "fresh claim must block re-claim");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_skips_unenrolled_and_recently_run(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        // State row WITHOUT enrollment (unenrolled leftover) must never claim.
        let orphan = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests) \
             VALUES ($1, '{}'::jsonb, '{}'::jsonb)",
        )
        .bind(orphan)
        .execute(&pool)
        .await
        .unwrap();
        // Enrolled but ran 1h ago with a 24h interval → not due.
        let recent = Uuid::new_v4();
        enroll(&pool, recent).await;
        // Pin the worldview BEFORE the last run so this owner tests pure
        // time-dueness, not the worldview-touch path.
        sqlx::query(
            "UPDATE engine.world_worldviews SET updated_at = now() - interval '2 hours' \
             WHERE owner_uid = $1",
        )
        .bind(recent)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests, last_run_at) \
             VALUES ($1, '{}'::jsonb, '{}'::jsonb, now() - interval '1 hour')",
        )
        .bind(recent)
        .execute(&pool)
        .await
        .unwrap();

        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert!(claimed.is_empty(), "orphan + not-due must both be skipped");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_skips_enrolled_owner_without_worldview(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll_without_worldview(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();

        assert!(
            repo.claim_due(DAY, STALE, 5).await.unwrap().is_empty(),
            "no worldview ⇒ never claimed"
        );
        // Blank (whitespace-only) content passes the DDL CHECK but must
        // still be treated as missing.
        set_worldview(&pool, owner, "  ").await;
        assert!(
            repo.claim_due(DAY, STALE, 5).await.unwrap().is_empty(),
            "blank worldview ⇒ never claimed"
        );
        // Providing one self-heals on the next scan.
        set_worldview(&pool, owner, "古代仙侠").await;
        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert_eq!(claimed.len(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_treats_worldview_touch_as_due(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        // Ran 1h ago (24h interval ⇒ not time-due), worldview from before
        // that run ⇒ not due at all.
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests, last_run_at) \
             VALUES ($1, '{}'::jsonb, '{}'::jsonb, now() - interval '1 hour')",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE engine.world_worldviews SET updated_at = now() - interval '2 hours' \
             WHERE owner_uid = $1",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        assert!(repo.claim_due(DAY, STALE, 5).await.unwrap().is_empty());

        // Touch the worldview (trigger bumps updated_at past last_run_at)
        // ⇒ due ahead of the interval (spec §3: change lands within one tick).
        set_worldview(&pool, owner, "科幻星际").await;
        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert_eq!(
            claimed.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
            vec![owner],
            "worldview touched after last run ⇒ immediately due"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn worldview_state_trims_and_carries_hash(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll_without_worldview(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();

        assert!(repo.worldview_state(owner).await.unwrap().is_none());
        set_worldview(&pool, owner, "  古代宫廷  ").await;
        let (content, hash, updated_at1) = repo.worldview_state(owner).await.unwrap().unwrap();
        assert_eq!(content, "古代宫廷", "content is trimmed");
        assert!(hash.is_none(), "no round yet ⇒ no stored hash");

        sqlx::query("UPDATE engine.world_states SET worldview_hash = 'abc' WHERE owner_uid = $1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        let (_, hash, updated_at2) = repo.worldview_state(owner).await.unwrap().unwrap();
        assert_eq!(hash.as_deref(), Some("abc"));
        assert_eq!(
            updated_at2, updated_at1,
            "worldview_hash lives on world_states, not the worldview row — updated_at is untouched"
        );

        set_worldview(&pool, owner, "   ").await;
        assert!(
            repo.worldview_state(owner).await.unwrap().is_none(),
            "blank content reads as missing"
        );
    }

    /// The trigger must stamp `clock_timestamp()`, not `now()`. `now()` is
    /// transaction-stable (pinned to this transaction's START), so a
    /// long-running UPDATE (e.g. one that blocked on `persist_round`'s
    /// `FOR SHARE` guard and only proceeds after that transaction commits)
    /// would stamp a time from BEFORE it actually ran, potentially earlier
    /// than the just-committed `last_run_at` — defeating touch-dueness
    /// again. `pg_sleep` inside the transaction makes the two clocks
    /// diverge deterministically: with `clock_timestamp()`, `updated_at` is
    /// stamped AFTER the sleep and so is later than `now()` (pinned before
    /// the sleep); with the old `now()` they would be equal and this
    /// assertion would fail.
    #[sqlx::test(migrations = "./migrations")]
    async fn worldview_trigger_stamps_wall_clock_time(pool: PgPool) {
        let owner = Uuid::new_v4();
        enroll_without_worldview(&pool, owner).await;
        set_worldview(&pool, owner, "现代都市").await;

        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SELECT pg_sleep(0.05)")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("UPDATE engine.world_worldviews SET content = '科幻星际' WHERE owner_uid = $1")
            .bind(owner)
            .execute(&mut *tx)
            .await
            .unwrap();
        let stamped_after_now: bool = sqlx::query_scalar(
            "SELECT updated_at > now() FROM engine.world_worldviews WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert!(
            stamped_after_now,
            "trigger must use clock_timestamp() (wall clock at execution time), \
             not now() (pinned to transaction start) — otherwise a blocked \
             UPDATE stamps a time from before it actually ran"
        );
        tx.rollback().await.unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn count_enrolled_missing_worldview_counts_absent_and_blank(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let with = Uuid::new_v4();
        let without = Uuid::new_v4();
        let blank = Uuid::new_v4();
        enroll(&pool, with).await;
        enroll_without_worldview(&pool, without).await;
        enroll_without_worldview(&pool, blank).await;
        set_worldview(&pool, blank, " ").await;

        assert_eq!(repo.count_enrolled_missing_worldview().await.unwrap(), 2);
    }

    /// SQL `btrim` and Rust `str::trim_matches(is_ascii_whitespace)` must
    /// agree on what "blank" means — otherwise content like a lone "\n"
    /// passes the DDL CHECK and claim_due's plain `btrim(...) <> ''` gate
    /// (which only strips spaces) while `worldview_state`'s old `.trim()`
    /// (which strips all Unicode whitespace) reads it as missing: per-tick
    /// claim churn plus a `worldview missing at round time` warn that the
    /// aggregate count also misses. Both sides now use the same ASCII
    /// whitespace charset (space, tab, CR, LF, FF), so a Unicode-only
    /// whitespace character like U+3000 (outside that charset) is
    /// consistently treated as PRESENT content on both sides.
    #[sqlx::test(migrations = "./migrations")]
    async fn blank_worldview_definition_agrees_between_sql_and_rust(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll_without_worldview(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();

        // A lone newline passes the DDL CHECK (non-empty) but is blank under
        // the ASCII-whitespace definition on both sides.
        set_worldview(&pool, owner, "\n").await;
        assert!(
            repo.claim_due(DAY, STALE, 5).await.unwrap().is_empty(),
            "newline-only content must not be claimable"
        );
        assert_eq!(
            repo.count_enrolled_missing_worldview().await.unwrap(),
            1,
            "newline-only content must count toward the aggregate warn"
        );
        assert!(
            repo.worldview_state(owner).await.unwrap().is_none(),
            "newline-only content must read as no worldview"
        );

        // Real content self-heals immediately.
        set_worldview(&pool, owner, "现代都市").await;
        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert_eq!(claimed.len(), 1, "real content becomes claimable");
        let (_, token) = claimed[0];
        assert_eq!(repo.count_enrolled_missing_worldview().await.unwrap(), 0);
        // Release so the next claim_due call below isn't blocked by the
        // fresh claim just taken — this test checks claimability, not the
        // claim-token lifecycle (covered elsewhere).
        repo.release_claim(owner, token).await.unwrap();

        // Full-width space (U+3000) IS Unicode whitespace but is outside the
        // ASCII charset both sides now trim on — it must read as PRESENT,
        // not blank, consistently.
        set_worldview(&pool, owner, "\u{3000}").await;
        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert_eq!(
            claimed.len(),
            1,
            "full-width-space-only content is claimable (treated as present)"
        );
        let (content, _hash, _updated_at) = repo
            .worldview_state(owner)
            .await
            .unwrap()
            .expect("full-width-space-only content must read as PRESENT");
        assert_eq!(content, "\u{3000}");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_reclaims_stale_claims(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        // Claimed 2h ago (stale window 30 min) and never finished.
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests, claimed_at) \
             VALUES ($1, '{}'::jsonb, '{}'::jsonb, now() - interval '2 hours')",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();

        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert_eq!(
            claimed.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
            vec![owner],
            "stale claim must be recovered"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn release_claim_makes_owner_due_again(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();

        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        let (o, token) = claimed[0];
        assert_eq!(o, owner);
        repo.release_claim(owner, token).await.unwrap();
        let reclaimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert_eq!(
            reclaimed.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
            vec![owner],
            "released claim must be immediately re-claimable"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn release_claim_only_clears_matching_token(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();

        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        let (_o, token1) = claimed[0];

        // Simulate a reclaim by a newer sweeper: bump claimed_at forward.
        sqlx::query(
            "UPDATE engine.world_states SET claimed_at = now() + interval '1 second' \
             WHERE owner_uid = $1",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();

        // The stale worker's release, using the OLD token, must be a no-op.
        repo.release_claim(owner, token1).await.unwrap();

        let claimed_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT claimed_at FROM engine.world_states WHERE owner_uid = $1")
                .bind(owner)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            claimed_at.is_some(),
            "stale worker must not clear the newer sweeper's claim"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn mark_ran_advances_last_run_and_clears_claim(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();
        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        let (_o, token) = claimed[0];

        repo.mark_ran(owner, token).await.unwrap();
        let (last_run, claimed): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT last_run_at, claimed_at FROM engine.world_states WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(last_run.is_some());
        assert!(claimed.is_none());
        // And it's no longer due under a 24h interval.
        assert!(repo.claim_due(DAY, STALE, 5).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn roster_lists_active_only_in_created_order(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let first = seed_instance(&pool, owner, "First", "active").await;
        // 'archived' is the repo convention for non-active (persona.rs tests);
        // status has no CHECK constraint — the roster filter is `= 'active'`.
        let _archived = seed_instance(&pool, owner, "Gone", "archived").await;
        let second = seed_instance(&pool, owner, "Second", "active").await;
        let _other_owner = seed_instance(&pool, Uuid::new_v4(), "Foreign", "active").await;

        let roster = repo.list_active_roster(owner, 9).await.unwrap();
        let ids: Vec<Uuid> = roster.iter().map(|r| r.instance_id).collect();
        assert_eq!(ids, vec![first, second]);
        assert_eq!(roster[0].name, "First");
        assert_eq!(roster[0].art_metadata["backstory"], "bs");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_extracted_memories_filters_layers_and_categories(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let instance = seed_instance(&pool, owner, "M", "active").await;
        let session: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(owner)
        .bind(instance)
        .fetch_one(&pool)
        .await
        .unwrap();
        let mem = crate::memory::MemoryRepo { pool: &pool };
        // Extracted profile row → included.
        mem.upsert(
            crate::memory::MemoryLayer::Profile,
            session,
            owner,
            None,
            "喜欢旅行",
            &unit_embedding(1),
            Some("preference"),
            None,
        )
        .await
        .unwrap();
        // Uncategorised profile row → excluded.
        mem.upsert(
            crate::memory::MemoryLayer::Profile,
            session,
            owner,
            None,
            "raw-profile",
            &unit_embedding(2),
            None,
            None,
        )
        .await
        .unwrap();
        // Relationship row (raw user line) → excluded even with a category.
        mem.upsert(
            crate::memory::MemoryLayer::Relationship,
            session,
            owner,
            Some(instance),
            "用户：原始台词",
            &unit_embedding(3),
            Some("fact"),
            None,
        )
        .await
        .unwrap();

        let feedback = repo.recent_extracted_memories(owner, 15).await.unwrap();
        assert_eq!(feedback, vec!["喜欢旅行".to_string()]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_round_writes_fragments_bumps_state_and_prunes(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();
        // Claim the owner so claimed_at is actually SET before persist_round
        // runs — otherwise the `assert!(claimed.is_none())` below is vacuous
        // (never-claimed rows are already NULL).
        let claimed = repo
            .claim_due(Duration::from_secs(24 * 3600), Duration::from_secs(1800), 5)
            .await
            .unwrap();
        let (_o, token) = claimed[0];
        let wv_at = worldview_updated_at(&pool, owner).await;
        let instance = seed_instance(&pool, owner, "P", "active").await;
        let today = Utc::now().date_naive();

        // Pre-existing OLD fragment (41 days ago, retention 30) → pruned.
        sqlx::query(
            "INSERT INTO engine.world_memories (owner_uid, instance_id, content, embedding, script_date) \
             VALUES ($1, $2, 'ancient', $3::vector, $4)",
        )
        .bind(owner)
        .bind(instance)
        .bind(format_vector(&unit_embedding(9)))
        .bind(today - chrono::Days::new(41))
        .execute(&pool)
        .await
        .unwrap();

        let seed = serde_json::json!({"relationships": [{"a": "P", "b": "Q", "bond": "friends"}]});
        let digests = serde_json::json!({ instance.to_string(): "P 最近在忙咖啡店开业" });
        let frags = vec![FragmentInsert {
            instance_id: instance,
            content: "P 试营业当天把咖啡机弄坏了".into(),
            embedding: unit_embedding(7),
        }];
        repo.persist_round(
            owner,
            &seed,
            &digests,
            &frags,
            &[],
            today,
            30,
            "h",
            false,
            wv_at,
            token,
        )
        .await
        .unwrap();

        let contents: Vec<String> =
            sqlx::query_scalar("SELECT content FROM engine.world_memories WHERE owner_uid = $1")
                .bind(owner)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(contents, vec!["P 试营业当天把咖啡机弄坏了".to_string()]);

        let (version, last_run, claimed): (i32, Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
            sqlx::query_as(
                "SELECT seed_version, last_run_at, claimed_at FROM engine.world_states \
                 WHERE owner_uid = $1",
            )
            .bind(owner)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, 2, "seed_version must increment");
        assert!(last_run.is_some());
        assert!(claimed.is_none());
        assert_eq!(repo.load_seed(owner).await.unwrap().unwrap(), seed);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_round_aborts_when_claim_lost(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();
        let instance = seed_instance(&pool, owner, "P", "active").await;
        let wv_at = worldview_updated_at(&pool, owner).await;
        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        let (_o, token1) = claimed[0];

        // A newer sweeper reclaims this owner (simulating WORLD_CLAIM_STALE
        // having elapsed) before the original worker's round finishes.
        sqlx::query(
            "UPDATE engine.world_states SET claimed_at = now() + interval '1 second' \
             WHERE owner_uid = $1",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();

        let seed = serde_json::json!({"x": 1});
        let frags = vec![FragmentInsert {
            instance_id: instance,
            content: "late fragment".into(),
            embedding: unit_embedding(3),
        }];
        let today = Utc::now().date_naive();
        let result = repo
            .persist_round(
                owner,
                &seed,
                &serde_json::json!({}),
                &frags,
                &[],
                today,
                30,
                "h",
                false,
                wv_at,
                token1,
            )
            .await;
        assert!(
            result.is_err(),
            "stale worker's persist must abort once its token no longer matches"
        );

        let version: i32 =
            sqlx::query_scalar("SELECT seed_version FROM engine.world_states WHERE owner_uid = $1")
                .bind(owner)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(version, 1, "state update must roll back");

        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.world_memories WHERE owner_uid = $1")
                .bind(owner)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 0, "fragment insert must roll back too");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_round_reset_purges_world_and_story_data_keeps_published_posts(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();
        let inst = seed_instance(&pool, owner, "P", "active").await;
        let today = Utc::now().date_naive();

        // Old-era data of every purgeable kind.
        sqlx::query(
            "INSERT INTO engine.world_memories (owner_uid, instance_id, content, embedding, script_date) \
             VALUES ($1, $2, '旧剧本', $3::vector, $4)",
        )
        .bind(owner).bind(inst).bind(format_vector(&unit_embedding(1))).bind(today)
        .execute(&pool).await.unwrap();
        let published: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.world_posts (owner_uid, instance_id, content, scheduled_at, published_at) \
             VALUES ($1, $2, '旧贴文', now() - interval '1 day', now() - interval '1 day') RETURNING id",
        )
        .bind(owner).bind(inst).fetch_one(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO engine.world_post_comments (post_id, author_instance_id, source, content) \
             VALUES ($1, NULL, NULL, '用户旧评论')",
        )
        .bind(published).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO engine.world_posts (owner_uid, instance_id, content, scheduled_at) \
             VALUES ($1, $2, '未发布贴文', now() + interval '1 hour')",
        )
        .bind(owner)
        .bind(inst)
        .execute(&pool)
        .await
        .unwrap();
        // persona_story_insights is a flat typed table (no opaque JSONB
        // stage, per 0038's own comment) — no `insight` column exists.
        sqlx::query(
            "INSERT INTO engine.persona_story_insights (instance_id, owner_uid, digest) \
             VALUES ($1, $2, '旧近况')",
        )
        .bind(inst)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        let old_event: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_story_events (owner_uid, instance_id, category, content, story_date) \
             VALUES ($1, $2, 'work', '旧事件', current_date) RETURNING id",
        )
        .bind(owner).bind(inst).fetch_one(&pool).await.unwrap();
        // persona_story_memories.event_id is NOT NULL (FK to
        // persona_story_events) — thread the row just inserted above.
        sqlx::query(
            "INSERT INTO engine.persona_story_memories (owner_uid, instance_id, event_id, content, embedding, story_date) \
             VALUES ($1, $2, $3, '旧记忆', $4::vector, current_date)",
        )
        .bind(owner).bind(inst).bind(old_event).bind(format_vector(&unit_embedding(2)))
        .execute(&pool).await.unwrap();

        let wv_at = worldview_updated_at(&pool, owner).await;
        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        let (_o, token) = claimed[0];
        let frags = vec![FragmentInsert {
            instance_id: inst,
            content: "新世界第一幕".into(),
            embedding: unit_embedding(7),
        }];
        repo.persist_round(
            owner,
            &serde_json::json!({"arc": "新"}),
            &serde_json::json!({}),
            &frags,
            &[],
            today,
            30,
            "newhash",
            true,
            wv_at,
            token,
        )
        .await
        .unwrap();

        let frag_contents: Vec<String> =
            sqlx::query_scalar("SELECT content FROM engine.world_memories WHERE owner_uid = $1")
                .bind(owner)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            frag_contents,
            vec!["新世界第一幕".to_string()],
            "old fragments purged, new kept"
        );

        let posts: Vec<String> =
            sqlx::query_scalar("SELECT content FROM engine.world_posts WHERE owner_uid = $1")
                .bind(owner)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            posts,
            vec!["旧贴文".to_string()],
            "published kept, unpublished purged"
        );
        let comments: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.world_post_comments WHERE post_id = $1",
        )
        .bind(published)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(comments, 1, "user comment on published post kept");

        for table in [
            "persona_story_insights",
            "persona_story_events",
            "persona_story_memories",
        ] {
            let n: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM engine.{table} WHERE owner_uid = $1"
            ))
            .bind(owner)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(n, 0, "{table} purged on reset");
        }

        let (hash, set_at): (Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT worldview_hash, worldview_set_at FROM engine.world_states WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(hash.as_deref(), Some("newhash"));
        assert!(set_at.is_some(), "reset stamps the era start");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_round_normal_stamps_hash_without_purge_or_era(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();
        let inst = seed_instance(&pool, owner, "P", "active").await;
        let today = Utc::now().date_naive();
        sqlx::query(
            "INSERT INTO engine.persona_story_events (owner_uid, instance_id, category, content, story_date) \
             VALUES ($1, $2, 'work', '既有事件', current_date)",
        )
        .bind(owner).bind(inst).execute(&pool).await.unwrap();

        let wv_at = worldview_updated_at(&pool, owner).await;
        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        let (_o, token) = claimed[0];
        repo.persist_round(
            owner,
            &serde_json::json!({}),
            &serde_json::json!({}),
            &[],
            &[],
            today,
            30,
            "h1",
            false,
            wv_at,
            token,
        )
        .await
        .unwrap();

        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.persona_story_events WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "normal round purges nothing");
        let (hash, set_at): (Option<String>, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT worldview_hash, worldview_set_at FROM engine.world_states WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(hash.as_deref(), Some("h1"), "hash stamped every round");
        assert!(set_at.is_none(), "era untouched on normal rounds");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_round_reset_rolls_back_purge_on_lost_claim(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();
        let inst = seed_instance(&pool, owner, "P", "active").await;
        sqlx::query(
            "INSERT INTO engine.persona_story_events (owner_uid, instance_id, category, content, story_date) \
             VALUES ($1, $2, 'work', '必须幸存', current_date)",
        )
        .bind(owner).bind(inst).execute(&pool).await.unwrap();
        let wv_at = worldview_updated_at(&pool, owner).await;
        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        let (_o, token1) = claimed[0];
        // A newer sweeper reclaims mid-round.
        sqlx::query(
            "UPDATE engine.world_states SET claimed_at = now() + interval '1 second' \
             WHERE owner_uid = $1",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();

        let result = repo
            .persist_round(
                owner,
                &serde_json::json!({}),
                &serde_json::json!({}),
                &[],
                &[],
                Utc::now().date_naive(),
                30,
                "h",
                true,
                wv_at,
                token1,
            )
            .await;
        assert!(result.is_err(), "lost claim must abort the reset");
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.persona_story_events WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1, "the purge must roll back with the transaction");
    }

    /// The P1 fix: a downstream worldview UPDATE that lands between the
    /// caller's `worldview_state` read and `persist_round`'s commit must
    /// abort the round rather than commit stale output — and, crucially,
    /// must leave `last_run_at` untouched so `claim_due`'s touch-dueness
    /// condition (`ww.updated_at > ws.last_run_at`) fires on the very next
    /// scan instead of waiting out the full interval.
    #[sqlx::test(migrations = "./migrations")]
    async fn persist_round_aborts_when_worldview_touched_mid_round(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();
        let inst = seed_instance(&pool, owner, "P", "active").await;
        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        let (_o, token) = claimed[0];
        // The stale value: what a caller would have read at round start,
        // BEFORE the downstream update below.
        let stale_wv_at = worldview_updated_at(&pool, owner).await;

        // Downstream changes the worldview mid-round (trigger bumps
        // updated_at because content actually changed).
        set_worldview(&pool, owner, "科幻星际").await;
        let fresh_wv_at = worldview_updated_at(&pool, owner).await;
        assert!(
            fresh_wv_at > stale_wv_at,
            "content change must bump updated_at"
        );

        let frags = vec![FragmentInsert {
            instance_id: inst,
            content: "stale-content fragment".into(),
            embedding: unit_embedding(11),
        }];
        let result = repo
            .persist_round(
                owner,
                &serde_json::json!({"arc": "stale"}),
                &serde_json::json!({}),
                &frags,
                &[],
                Utc::now().date_naive(),
                30,
                "stalehash",
                false,
                stale_wv_at,
                token,
            )
            .await;
        assert!(
            result.is_err(),
            "a worldview touched mid-round must abort persist_round"
        );

        let version: i32 =
            sqlx::query_scalar("SELECT seed_version FROM engine.world_states WHERE owner_uid = $1")
                .bind(owner)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(version, 1, "state update must roll back");
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM engine.world_memories WHERE owner_uid = $1")
                .bind(owner)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 0, "fragment insert must roll back too");

        // Mirror the real caller (pipeline/world.rs `direct_world`): on a
        // persist_round error it releases the claim so the owner isn't
        // stuck behind WORLD_CLAIM_STALE.
        repo.release_claim(owner, token).await.unwrap();

        // The point of the fix: last_run_at was never advanced, so the
        // owner is immediately due again under the touch-dueness rule —
        // no waiting out DAY-length interval.
        let reclaimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        assert_eq!(
            reclaimed.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
            vec![owner],
            "aborted round must not blunt touch-dueness — owner re-claims next tick"
        );
    }

    #[sqlx::test]
    async fn town_enabled_reflects_enrollment_flag(pool: PgPool) {
        let owner = Uuid::new_v4();
        let repo = WorldRepo { pool: &pool };
        assert!(
            !repo.town_enabled(owner).await.unwrap(),
            "unenrolled ⇒ false"
        );
        enroll(&pool, owner).await;
        assert!(!repo.town_enabled(owner).await.unwrap(), "default false");
        sqlx::query("UPDATE engine.world_enrollments SET town_enabled = true WHERE owner_uid = $1")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        assert!(repo.town_enabled(owner).await.unwrap());
    }

    #[sqlx::test]
    async fn persist_round_inserts_scheduled_posts(pool: PgPool) {
        let owner = Uuid::new_v4();
        let inst = seed_instance(&pool, owner, "P", "active").await;
        enroll(&pool, owner).await;
        let repo = WorldRepo { pool: &pool };
        repo.ensure_states_for_enrollments().await.unwrap();
        let claimed = repo
            .claim_due(
                std::time::Duration::from_secs(24 * 3600),
                std::time::Duration::from_secs(1800),
                5,
            )
            .await
            .unwrap();
        let (_o, token) = claimed[0];
        let wv_at = worldview_updated_at(&pool, owner).await;

        let at = Utc::now() + chrono::Duration::hours(3);
        let posts = vec![PostInsert {
            instance_id: inst,
            content: "今天试了新的拉花".into(),
            scheduled_at: at,
        }];
        repo.persist_round(
            owner,
            &serde_json::json!({"arc": "a"}),
            &serde_json::json!({}),
            &[],
            &posts,
            Utc::now().date_naive(),
            30,
            "h",
            false,
            wv_at,
            token,
        )
        .await
        .unwrap();

        let (content, published_at): (String, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT content, published_at FROM engine.world_posts WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(content, "今天试了新的拉花");
        assert!(published_at.is_none(), "inserted unpublished");
    }

    #[sqlx::test]
    async fn stories_enabled_reflects_flag(pool: PgPool) {
        let repo = crate::world::WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        assert!(
            !repo.stories_enabled(owner).await.unwrap(),
            "unenrolled ⇒ false"
        );
        sqlx::query("INSERT INTO engine.world_enrollments (owner_uid) VALUES ($1)")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!repo.stories_enabled(owner).await.unwrap(), "default false");
        sqlx::query(
            "UPDATE engine.world_enrollments SET stories_enabled = true WHERE owner_uid = $1",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        assert!(repo.stories_enabled(owner).await.unwrap());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn fetch_digest_requires_enrollment_and_nonblank_entry(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let instance = seed_instance(&pool, owner, "D", "active").await;
        let other_instance = seed_instance(&pool, owner, "E", "active").await;

        // State without enrollment → None (unenrolled stops injection).
        sqlx::query(
            "INSERT INTO engine.world_states (owner_uid, seed, digests) \
             VALUES ($1, '{}'::jsonb, $2)",
        )
        .bind(owner)
        .bind(serde_json::json!({ instance.to_string(): "近况摘要" }))
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(repo.fetch_digest(owner, instance).await.unwrap(), None);

        enroll(&pool, owner).await;
        assert_eq!(
            repo.fetch_digest(owner, instance).await.unwrap(),
            Some("近况摘要".to_string())
        );
        // Instance with no digest entry → None.
        assert_eq!(
            repo.fetch_digest(owner, other_instance).await.unwrap(),
            None
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn search_fragments_scopes_by_owner_and_instance(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();
        let a = seed_instance(&pool, owner, "A", "active").await;
        let b = seed_instance(&pool, owner, "B", "active").await;
        let today = Utc::now().date_naive();
        let wv_at = worldview_updated_at(&pool, owner).await;
        let claimed = repo.claim_due(DAY, STALE, 5).await.unwrap();
        let (_o, token) = claimed[0];

        let frags = vec![
            FragmentInsert {
                instance_id: a,
                content: "near-a".into(),
                embedding: unit_embedding(42),
            },
            FragmentInsert {
                instance_id: a,
                content: "far-a".into(),
                embedding: unit_embedding(400),
            },
            FragmentInsert {
                instance_id: b,
                content: "near-b".into(),
                embedding: unit_embedding(42),
            },
        ];
        repo.persist_round(
            owner,
            &serde_json::json!({}),
            &serde_json::json!({}),
            &frags,
            &[],
            today,
            30,
            "h",
            false,
            wv_at,
            token,
        )
        .await
        .unwrap();

        let hits = repo
            .search_fragments(owner, a, &unit_embedding(42), 3)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "only instance A's fragments");
        assert_eq!(hits[0], "near-a", "cosine order");
        assert!(!hits.contains(&"near-b".to_string()));
    }
}

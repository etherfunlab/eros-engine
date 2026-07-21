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

    /// Atomically claim up to `batch` due owners (enrolled AND past their
    /// interval AND not freshly claimed). Same statement shape as the
    /// dreaming picker: concurrent sweepers see disjoint sets.
    pub async fn claim_due(
        &self,
        interval: Duration,
        stale: Duration,
        batch: i64,
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        let now = Utc::now();
        let due_cutoff: DateTime<Utc> =
            now - chrono::Duration::from_std(interval).unwrap_or_default();
        let stale_cutoff: DateTime<Utc> =
            now - chrono::Duration::from_std(stale).unwrap_or_default();
        sqlx::query_scalar(
            "UPDATE engine.world_states SET claimed_at = now() \
             WHERE owner_uid IN ( \
                 SELECT ws.owner_uid FROM engine.world_states ws \
                 JOIN engine.world_enrollments we USING (owner_uid) \
                 WHERE (ws.last_run_at IS NULL OR ws.last_run_at < $1) \
                   AND (ws.claimed_at IS NULL OR ws.claimed_at < $2) \
                 ORDER BY ws.last_run_at ASC NULLS FIRST \
                 LIMIT $3 \
                 FOR UPDATE SKIP LOCKED \
             ) \
             RETURNING owner_uid",
        )
        .bind(due_cutoff)
        .bind(stale_cutoff)
        .bind(batch)
        .fetch_all(self.pool)
        .await
    }

    /// Reset the claim after a failed round so the owner retries at the next
    /// due scan instead of waiting out the stale window.
    pub async fn release_claim(&self, owner_uid: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE engine.world_states SET claimed_at = NULL WHERE owner_uid = $1")
            .bind(owner_uid)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    /// Stamp a round that produced nothing to persist (e.g. empty roster):
    /// advances last_run_at and clears the claim without touching the seed.
    pub async fn mark_ran(&self, owner_uid: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.world_states \
             SET last_run_at = now(), claimed_at = NULL, updated_at = now() \
             WHERE owner_uid = $1",
        )
        .bind(owner_uid)
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
    /// retention delete + fragment inserts + state update (seed_version++,
    /// last_run_at=now, claimed_at=NULL). All-or-nothing: any failure rolls
    /// back and the caller releases the claim.
    pub async fn persist_round(
        &self,
        owner_uid: Uuid,
        seed: &serde_json::Value,
        digests: &serde_json::Value,
        fragments: &[FragmentInsert],
        script_date: NaiveDate,
        retention_days: u32,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
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
        sqlx::query(
            "UPDATE engine.world_states \
             SET seed = $2, digests = $3, seed_version = seed_version + 1, \
                 last_run_at = now(), claimed_at = NULL, updated_at = now() \
             WHERE owner_uid = $1",
        )
        .bind(owner_uid)
        .bind(seed)
        .bind(digests)
        .execute(&mut *tx)
        .await?;
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
        assert_eq!(claimed, vec![owner], "never-run world is due");

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
        assert_eq!(claimed, vec![owner], "stale claim must be recovered");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn release_claim_makes_owner_due_again(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();

        assert_eq!(repo.claim_due(DAY, STALE, 5).await.unwrap(), vec![owner]);
        repo.release_claim(owner).await.unwrap();
        assert_eq!(
            repo.claim_due(DAY, STALE, 5).await.unwrap(),
            vec![owner],
            "released claim must be immediately re-claimable"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn mark_ran_advances_last_run_and_clears_claim(pool: PgPool) {
        let repo = WorldRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll(&pool, owner).await;
        repo.ensure_states_for_enrollments().await.unwrap();
        repo.claim_due(DAY, STALE, 5).await.unwrap();

        repo.mark_ran(owner).await.unwrap();
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
        repo.persist_round(owner, &seed, &digests, &frags, today, 30)
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
            today,
            30,
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

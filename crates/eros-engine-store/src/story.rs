// SPDX-License-Identifier: AGPL-3.0-only
//! World Stories persistence (spec: docs/superpowers/specs/2026-07-23-world-stories-design.md).
//!
//! `persona_story_insights` is both the flat typed life base AND the story
//! sweeper's per-instance scheduling row (world_states claim shape).
//! `persona_story_events` is the append-only progression log;
//! `persona_story_memories` its 1:1 embedded recall mirror.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::time::Duration;
use uuid::Uuid;

/// The fixed insight column list, DDL order. Single source for the
/// full-column UPDATE and the prompt-schema lockstep test (server side).
pub const STORY_INSIGHT_FIELDS: &[&str] = &[
    "city",
    "location",
    "hometown",
    "nationality",
    "occupation",
    "mbti_guess",
    "love_values",
    "emotional_needs",
    "life_rhythm",
    "education",
    "family",
    "relationship_history",
    "social_pattern",
    "future_plans",
    "finance_status",
    "interests",
    "personality_traits",
    "preferred_gender",
    "age_min",
    "age_max",
    "deal_breakers",
    "work_history",
    "romance_history",
    "family_of_origin",
    "user_relationship",
];

/// Flat typed life base. Serde derives serve the director's structured output
/// (unknown keys are ignored by serde; the server warns on them separately).
#[derive(Debug, Clone, Default, Serialize, Deserialize, sqlx::FromRow)]
pub struct StoryInsight {
    pub city: Option<String>,
    pub location: Option<String>,
    pub hometown: Option<String>,
    pub nationality: Option<String>,
    pub occupation: Option<String>,
    pub mbti_guess: Option<String>,
    pub love_values: Option<String>,
    pub emotional_needs: Option<String>,
    pub life_rhythm: Option<String>,
    pub education: Option<String>,
    pub family: Option<String>,
    pub relationship_history: Option<String>,
    pub social_pattern: Option<String>,
    pub future_plans: Option<String>,
    pub finance_status: Option<String>,
    #[serde(default)]
    pub interests: Vec<String>,
    #[serde(default)]
    pub personality_traits: Vec<String>,
    pub preferred_gender: Option<String>,
    pub age_min: Option<i32>,
    pub age_max: Option<i32>,
    #[serde(default)]
    pub deal_breakers: Vec<String>,
    pub work_history: Option<String>,
    pub romance_history: Option<String>,
    pub family_of_origin: Option<String>,
    pub user_relationship: Option<String>,
}

pub struct StoryRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> StoryRepo<'a> {
    /// Backfill a story row for every eligible instance: owner enrolled with
    /// stories_enabled × active roster (first `roster_cap` by created_at, the
    /// same cap+order as the WM roster). Returns rows inserted.
    pub async fn ensure_insight_rows(&self, roster_cap: i64) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            "INSERT INTO engine.persona_story_insights (instance_id, owner_uid) \
             SELECT ranked.id, ranked.owner_uid FROM ( \
                 SELECT pi.id, pi.owner_uid, \
                        row_number() OVER (PARTITION BY pi.owner_uid ORDER BY pi.created_at ASC) AS rn \
                 FROM engine.persona_instances pi \
                 JOIN engine.world_enrollments we \
                   ON we.owner_uid = pi.owner_uid AND we.stories_enabled \
                 WHERE pi.status = 'active' \
             ) ranked WHERE ranked.rn <= $1 \
             ON CONFLICT (instance_id) DO NOTHING",
        )
        .bind(roster_cap)
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Claim up to `batch` due instances. Due = past `interval`, not freshly
    /// claimed, owner still stories-enabled, instance still active, AND the
    /// activity gate: the pair chatted within `active_window`. Returns
    /// (instance_id, owner_uid, claimed_at token) — token semantics identical
    /// to `WorldRepo::claim_due`.
    pub async fn claim_due(
        &self,
        interval: Duration,
        stale: Duration,
        active_window: Duration,
        batch: i64,
    ) -> Result<Vec<(Uuid, Uuid, DateTime<Utc>)>, sqlx::Error> {
        let now = Utc::now();
        let due_cutoff = now - chrono::Duration::from_std(interval).unwrap_or_default();
        let stale_cutoff = now - chrono::Duration::from_std(stale).unwrap_or_default();
        let activity_cutoff = now - chrono::Duration::from_std(active_window).unwrap_or_default();
        sqlx::query_as(
            "UPDATE engine.persona_story_insights SET claimed_at = now() \
             WHERE instance_id IN ( \
                 SELECT psi.instance_id FROM engine.persona_story_insights psi \
                 JOIN engine.world_enrollments we \
                   ON we.owner_uid = psi.owner_uid AND we.stories_enabled \
                 JOIN engine.persona_instances pi \
                   ON pi.id = psi.instance_id AND pi.status = 'active' \
                 WHERE (psi.last_run_at IS NULL OR psi.last_run_at < $1) \
                   AND (psi.claimed_at IS NULL OR psi.claimed_at < $2) \
                   AND EXISTS ( \
                       SELECT 1 FROM engine.chat_sessions cs \
                       WHERE cs.user_id = psi.owner_uid \
                         AND cs.instance_id = psi.instance_id \
                         AND cs.last_active_at > $3) \
                 ORDER BY psi.last_run_at ASC NULLS FIRST \
                 LIMIT $4 \
                 FOR UPDATE SKIP LOCKED \
             ) \
             RETURNING instance_id, owner_uid, claimed_at",
        )
        .bind(due_cutoff)
        .bind(stale_cutoff)
        .bind(activity_cutoff)
        .bind(batch)
        .fetch_all(self.pool)
        .await
    }

    /// Token-guarded claim release — see `WorldRepo::release_claim`.
    pub async fn release_claim(
        &self,
        instance_id: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.persona_story_insights SET claimed_at = NULL \
             WHERE instance_id = $1 AND claimed_at = $2",
        )
        .bind(instance_id)
        .bind(claimed_at)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Token-guarded no-op-round stamp — see `WorldRepo::mark_ran`.
    pub async fn mark_ran(
        &self,
        instance_id: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE engine.persona_story_insights \
             SET last_run_at = now(), claimed_at = NULL, updated_at = now() \
             WHERE instance_id = $1 AND claimed_at = $2",
        )
        .bind(instance_id)
        .bind(claimed_at)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story_insight_fields_count_and_serde_roundtrip() {
        assert_eq!(STORY_INSIGHT_FIELDS.len(), 25);
        let v = serde_json::json!({
            "occupation": "咖啡店店主",
            "interests": ["手冲咖啡"],
            "age_min": 25, "age_max": 35,
            "user_relationship": "刚确认恋爱关系",
            "totally_unknown_key": "ignored"
        });
        let ins: StoryInsight = serde_json::from_value(v).expect("unknown keys ignored");
        assert_eq!(ins.occupation.as_deref(), Some("咖啡店店主"));
        assert_eq!(ins.interests, vec!["手冲咖啡"]);
        assert_eq!(ins.age_min, Some(25));
        // Serialize side: every field key present for payload rendering.
        let out = serde_json::to_value(&ins).unwrap();
        for f in STORY_INSIGHT_FIELDS {
            assert!(out.get(*f).is_some(), "missing {f} in serialized insight");
        }
    }

    async fn enroll_stories(pool: &PgPool, owner: Uuid) {
        sqlx::query(
            "INSERT INTO engine.world_enrollments (owner_uid, stories_enabled) VALUES ($1, true)",
        )
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    }

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

    /// Stamp chat activity for the (owner, instance) pair so the claim gate opens.
    async fn touch_activity(pool: &PgPool, owner: Uuid, instance: Uuid, ago: &str) {
        sqlx::query(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, last_active_at) \
             VALUES ($1, $2, now() - $3::interval)",
        )
        .bind(owner)
        .bind(instance)
        .bind(ago)
        .execute(pool)
        .await
        .unwrap();
    }

    const EIGHT_H: Duration = Duration::from_secs(8 * 3600);
    const STALE: Duration = Duration::from_secs(1800);
    const WINDOW: Duration = Duration::from_secs(72 * 3600);

    #[sqlx::test(migrations = "./migrations")]
    async fn ensure_insight_rows_honors_flag_status_and_cap(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let stories_owner = Uuid::new_v4();
        enroll_stories(&pool, stories_owner).await;
        let a = seed_instance(&pool, stories_owner, "A", "active").await;
        let _archived = seed_instance(&pool, stories_owner, "Z", "archived").await;
        // Enrolled WITHOUT stories flag ⇒ no rows.
        let plain_owner = Uuid::new_v4();
        sqlx::query("INSERT INTO engine.world_enrollments (owner_uid) VALUES ($1)")
            .bind(plain_owner)
            .execute(&pool)
            .await
            .unwrap();
        let _plain = seed_instance(&pool, plain_owner, "P", "active").await;

        assert_eq!(repo.ensure_insight_rows(8).await.unwrap(), 1);
        assert_eq!(repo.ensure_insight_rows(8).await.unwrap(), 0, "idempotent");
        let ids: Vec<Uuid> =
            sqlx::query_scalar("SELECT instance_id FROM engine.persona_story_insights")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(ids, vec![a]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ensure_insight_rows_caps_roster(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll_stories(&pool, owner).await;
        let first = seed_instance(&pool, owner, "1st", "active").await;
        let second = seed_instance(&pool, owner, "2nd", "active").await;
        let _third = seed_instance(&pool, owner, "3rd", "active").await;
        assert_eq!(repo.ensure_insight_rows(2).await.unwrap(), 2);
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT instance_id FROM engine.persona_story_insights ORDER BY updated_at, instance_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            ids.contains(&first) && ids.contains(&second),
            "earliest-created win the cap"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_due_requires_activity_flag_and_status(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll_stories(&pool, owner).await;
        let inst = seed_instance(&pool, owner, "A", "active").await;
        repo.ensure_insight_rows(8).await.unwrap();

        // No chat activity ⇒ never claimed.
        assert!(repo
            .claim_due(EIGHT_H, STALE, WINDOW, 8)
            .await
            .unwrap()
            .is_empty());

        // Stale activity (beyond 72h window) ⇒ still not claimed.
        touch_activity(&pool, owner, inst, "100 hours").await;
        assert!(repo
            .claim_due(EIGHT_H, STALE, WINDOW, 8)
            .await
            .unwrap()
            .is_empty());

        // Fresh activity ⇒ claimed once; fresh claim blocks re-claim.
        touch_activity(&pool, owner, inst, "1 hour").await;
        let claimed = repo.claim_due(EIGHT_H, STALE, WINDOW, 8).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].0, inst);
        assert_eq!(claimed[0].1, owner);
        assert!(repo
            .claim_due(EIGHT_H, STALE, WINDOW, 8)
            .await
            .unwrap()
            .is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn claim_release_and_token_guard(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll_stories(&pool, owner).await;
        let inst = seed_instance(&pool, owner, "A", "active").await;
        touch_activity(&pool, owner, inst, "1 hour").await;
        repo.ensure_insight_rows(8).await.unwrap();

        let claimed = repo.claim_due(EIGHT_H, STALE, WINDOW, 8).await.unwrap();
        let (_i, _o, token) = claimed[0];
        repo.release_claim(inst, token).await.unwrap();
        // NOTE: `claim_due` both reads AND claims (mutates `claimed_at`), so
        // this call is the re-claim itself — its result supplies `token2`
        // below rather than a fresh, separate `claim_due` call (which would
        // find nothing due, since this call already claimed the row).
        let claimed = repo.claim_due(EIGHT_H, STALE, WINDOW, 8).await.unwrap();
        assert_eq!(claimed.len(), 1, "released claim re-claimable");

        // mark_ran advances last_run_at ⇒ no longer due.
        let (_i, _o, token2) = claimed[0];
        repo.mark_ran(inst, token2).await.unwrap();
        assert!(repo
            .claim_due(EIGHT_H, STALE, WINDOW, 8)
            .await
            .unwrap()
            .is_empty());

        // Stale-token release must not clear a newer claim.
        sqlx::query("UPDATE engine.persona_story_insights SET last_run_at = NULL, claimed_at = now() + interval '1 second' WHERE instance_id = $1")
            .bind(inst)
            .execute(&pool)
            .await
            .unwrap();
        repo.release_claim(inst, token2).await.unwrap();
        let still: Option<DateTime<Utc>> = sqlx::query_scalar(
            "SELECT claimed_at FROM engine.persona_story_insights WHERE instance_id = $1",
        )
        .bind(inst)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(still.is_some(), "old token must not clear newer claim");
    }
}

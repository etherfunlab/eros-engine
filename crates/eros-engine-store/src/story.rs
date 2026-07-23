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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoryPersona {
    pub name: String,
    pub tip_personality: Option<String>,
    pub art_metadata: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct StoryInsightRow {
    pub insight: StoryInsight,
    pub digest: String,
    pub insight_version: i32,
    pub last_run_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoryAffinity {
    pub warmth: f64,
    pub trust: f64,
    pub intrigue: f64,
    pub intimacy: f64,
    pub patience: f64,
    pub tension: f64,
    pub bond: f64,
    pub chemistry: f64,
    pub relationship_label: Option<String>,
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

    /// Genome display data for one instance (story payload header).
    pub async fn instance_persona(
        &self,
        instance_id: Uuid,
    ) -> Result<Option<StoryPersona>, sqlx::Error> {
        sqlx::query_as(
            "SELECT pg.name, pg.tip_personality, pg.art_metadata \
             FROM engine.persona_instances pi \
             JOIN engine.persona_genomes pg ON pg.id = pi.genome_id \
             WHERE pi.id = $1",
        )
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await
    }

    /// The instance's full story row. `last_run_at IS NULL` ⇒ init branch.
    pub async fn load_insight_row(
        &self,
        instance_id: Uuid,
    ) -> Result<Option<StoryInsightRow>, sqlx::Error> {
        #[allow(clippy::type_complexity)] // sqlx tuple query — type alias would add noise
        let row: Option<(
            sqlx::types::Json<StoryInsight>,
            String,
            i32,
            Option<DateTime<Utc>>,
        )> = sqlx::query_as(
            "SELECT to_jsonb(psi.*) - 'instance_id' - 'owner_uid' - 'digest' \
                        - 'insight_version' - 'last_run_at' - 'claimed_at' - 'updated_at', \
                        digest, insight_version, last_run_at \
                 FROM engine.persona_story_insights psi WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(row.map(
            |(insight, digest, insight_version, last_run_at)| StoryInsightRow {
                insight: insight.0,
                digest,
                insight_version,
                last_run_at,
            },
        ))
    }

    /// Latest `limit` events, returned oldest→newest (payload order).
    pub async fn recent_events(
        &self,
        instance_id: Uuid,
        limit: i64,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        let mut rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT category, content FROM engine.persona_story_events \
             WHERE instance_id = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(instance_id)
        .bind(limit)
        .fetch_all(self.pool)
        .await?;
        rows.reverse();
        Ok(rows)
    }

    /// Cross-session chat evidence for the pair, `window_days` back, capped at
    /// `cap_turns` most-recent complete user→assistant pairs, chronological.
    /// Same role/channel/truncation filters and pairing walk as
    /// `ChatRepo::recent_turn_pairs`, but scoped to (owner, instance) across
    /// sessions instead of one session.
    pub async fn chat_evidence(
        &self,
        owner_uid: Uuid,
        instance_id: Uuid,
        window_days: u32,
        cap_turns: u8,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        let fetch_n: i64 = (cap_turns as i64) * 2 + 2;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT cm.role, cm.content \
             FROM engine.chat_messages cm \
             JOIN engine.chat_sessions cs ON cs.id = cm.session_id \
             WHERE cs.user_id = $1 AND cs.instance_id = $2 \
               AND cm.sent_at > now() - make_interval(days => $3) \
               AND cm.truncated = FALSE \
               AND cm.channel IS NULL \
               AND cm.role IN ('user', 'gift_user', 'assistant') \
             ORDER BY cm.sent_at DESC \
             LIMIT $4",
        )
        .bind(owner_uid)
        .bind(instance_id)
        .bind(window_days as i32)
        .bind(fetch_n)
        .fetch_all(self.pool)
        .await?;
        let mut chrono_rows = rows;
        chrono_rows.reverse();
        let mut pairs: Vec<(String, String)> = Vec::new();
        let mut i = 0;
        while i + 1 < chrono_rows.len() {
            let (role_a, content_a) = &chrono_rows[i];
            let (role_b, content_b) = &chrono_rows[i + 1];
            if (role_a == "user" || role_a == "gift_user") && role_b == "assistant" {
                pairs.push((content_a.clone(), content_b.clone()));
                i += 2;
            } else {
                i += 1;
            }
        }
        let want = cap_turns as usize;
        if pairs.len() > want {
            let drop = pairs.len() - want;
            pairs.drain(..drop);
        }
        Ok(pairs)
    }

    /// Current affinity for the pair via its latest-active session. Advisory
    /// input only (spec rule 2: chat records are the relationship ground truth).
    pub async fn affinity_snapshot(
        &self,
        owner_uid: Uuid,
        instance_id: Uuid,
    ) -> Result<Option<StoryAffinity>, sqlx::Error> {
        sqlx::query_as(
            "SELECT ca.warmth, ca.trust, ca.intrigue, ca.intimacy, ca.patience, ca.tension, \
                    ca.bond, ca.chemistry, ca.relationship_label \
             FROM engine.companion_affinity ca \
             JOIN engine.chat_sessions cs ON cs.id = ca.session_id \
             WHERE ca.user_id = $1 AND ca.instance_id = $2 \
             ORDER BY cs.last_active_at DESC LIMIT 1",
        )
        .bind(owner_uid)
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await
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

    #[sqlx::test(migrations = "./migrations")]
    async fn load_insight_row_roundtrips_columns(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll_stories(&pool, owner).await;
        let inst = seed_instance(&pool, owner, "A", "active").await;
        repo.ensure_insight_rows(8).await.unwrap();
        sqlx::query(
            "UPDATE engine.persona_story_insights \
             SET occupation = '咖啡店店主', interests = ARRAY['手冲咖啡'], age_min = 25, \
                 digest = '正在筹备开店', last_run_at = now() \
             WHERE instance_id = $1",
        )
        .bind(inst)
        .execute(&pool)
        .await
        .unwrap();
        let row = repo.load_insight_row(inst).await.unwrap().expect("row");
        assert_eq!(row.insight.occupation.as_deref(), Some("咖啡店店主"));
        assert_eq!(row.insight.interests, vec!["手冲咖啡"]);
        assert_eq!(row.insight.age_min, Some(25));
        assert_eq!(row.digest, "正在筹备开店");
        assert!(row.last_run_at.is_some());
        assert!(repo
            .load_insight_row(Uuid::new_v4())
            .await
            .unwrap()
            .is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn instance_persona_joins_genome(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let inst = seed_instance(&pool, owner, "Aria", "active").await;
        let p = repo.instance_persona(inst).await.unwrap().expect("persona");
        assert_eq!(p.name, "Aria");
        assert_eq!(p.art_metadata["backstory"], "bs");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recent_events_chronological_and_limited(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let inst = seed_instance(&pool, owner, "A", "active").await;
        for (i, c) in ["e1", "e2", "e3"].iter().enumerate() {
            sqlx::query(
                "INSERT INTO engine.persona_story_events \
                 (owner_uid, instance_id, category, content, story_date, created_at) \
                 VALUES ($1, $2, 'life', $3, current_date, now() - make_interval(mins => $4))",
            )
            .bind(owner)
            .bind(inst)
            .bind(c)
            .bind((3 - i as i32) * 10)
            .execute(&pool)
            .await
            .unwrap();
        }
        let ev = repo.recent_events(inst, 2).await.unwrap();
        assert_eq!(
            ev.iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>(),
            vec!["e2", "e3"],
            "latest 2, oldest→newest"
        );
        assert_eq!(ev[0].0, "life");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn chat_evidence_windows_pairs_and_scopes(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let inst = seed_instance(&pool, owner, "A", "active").await;
        let other = seed_instance(&pool, owner, "B", "active").await;
        let session: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(owner)
        .bind(inst)
        .fetch_one(&pool)
        .await
        .unwrap();
        let other_session: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(owner)
        .bind(other)
        .fetch_one(&pool)
        .await
        .unwrap();
        // In-window pair + out-of-window pair + other-instance pair.
        for (s, u, a, ago_days) in [
            (session, "你今天忙吗", "在准备开店的事", 1),
            (session, "老掉牙", "旧回复", 10),
            (other_session, "别的角色", "别的回复", 1),
        ] {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
                 VALUES ($1, 'user', $2, now() - make_interval(days => $4, mins => 1)), \
                        ($1, 'assistant', $3, now() - make_interval(days => $4))",
            )
            .bind(s)
            .bind(u)
            .bind(a)
            .bind(ago_days)
            .execute(&pool)
            .await
            .unwrap();
        }
        let pairs = repo.chat_evidence(owner, inst, 7, 60).await.unwrap();
        assert_eq!(
            pairs,
            vec![("你今天忙吗".to_string(), "在准备开店的事".to_string())]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn affinity_snapshot_uses_latest_active_session(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let inst = seed_instance(&pool, owner, "A", "active").await;
        assert!(repo.affinity_snapshot(owner, inst).await.unwrap().is_none());
        for (label, ago) in [("旧识", "10 days"), ("挚友", "1 hour")] {
            let s: Uuid = sqlx::query_scalar(
                "INSERT INTO engine.chat_sessions (user_id, instance_id, last_active_at) \
                 VALUES ($1, $2, now() - $3::interval) RETURNING id",
            )
            .bind(owner)
            .bind(inst)
            .bind(ago)
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO engine.companion_affinity \
                 (session_id, user_id, instance_id, relationship_label) VALUES ($1, $2, $3, $4)",
            )
            .bind(s)
            .bind(owner)
            .bind(inst)
            .bind(label)
            .execute(&pool)
            .await
            .unwrap();
        }
        let snap = repo
            .affinity_snapshot(owner, inst)
            .await
            .unwrap()
            .expect("snapshot");
        assert_eq!(snap.relationship_label.as_deref(), Some("挚友"));
        assert!(snap.bond >= 0.0 && snap.chemistry >= 0.0);
    }
}

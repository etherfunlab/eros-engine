// SPDX-License-Identifier: AGPL-3.0-only
//! World Stories persistence (spec: docs/superpowers/specs/2026-07-23-world-stories-design.md).
//!
//! `persona_story_insights` is both the flat typed life base AND the story
//! sweeper's per-instance scheduling row (world_states claim shape).
//! `persona_story_events` is the append-only progression log;
//! `persona_story_memories` its 1:1 embedded recall mirror.

use chrono::{DateTime, NaiveDate, Utc};
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

#[derive(Debug, Clone)]
pub struct StoryEventInsert {
    pub category: String,
    pub content: String,
    pub embedding: Vec<f32>,
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
    /// Evidence is gathered across all of the pair's sessions, but pairing
    /// itself is session-scoped: a user turn only pairs with an assistant
    /// reply in the SAME `session_id`, so two overlapping sessions with the
    /// same persona can never produce a fabricated cross-session pair. Same
    /// role/channel/truncation filters and single-session pairing semantics
    /// (consecutive users collapse to the latest, lone assistants drop) as
    /// `ChatRepo::recent_turn_pairs`.
    pub async fn chat_evidence(
        &self,
        owner_uid: Uuid,
        instance_id: Uuid,
        window_days: u32,
        cap_turns: u8,
    ) -> Result<Vec<(String, String)>, sqlx::Error> {
        let fetch_n: i64 = (cap_turns as i64) * 2 + 2;
        let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
            "SELECT cm.session_id, cm.role, cm.content \
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
        use std::collections::HashMap;
        let mut pending: HashMap<Uuid, String> = HashMap::new(); // session_id -> latest unpaired user content
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (session_id, role, content) in chrono_rows {
            if role == "user" || role == "gift_user" {
                pending.insert(session_id, content); // consecutive users collapse to the latest, matching recent_turn_pairs
            } else {
                // role == "assistant"
                if let Some(user_content) = pending.remove(&session_id) {
                    pairs.push((user_content, content));
                }
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

    /// Persist one story round in a single transaction: retention prune on
    /// BOTH tables + event inserts + 1:1 memory inserts (event_id linked) +
    /// token-guarded full-column insight/digest update. Any failure rolls
    /// back completely — semantics copied from `WorldRepo::persist_round`.
    #[allow(clippy::too_many_arguments)]
    pub async fn persist_round(
        &self,
        instance_id: Uuid,
        owner_uid: Uuid,
        insight: &StoryInsight,
        digest: &str,
        events: &[StoryEventInsert],
        story_date: NaiveDate,
        retention_days: u32,
        claimed_at: DateTime<Utc>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let cutoff = story_date - chrono::Days::new(u64::from(retention_days));
        sqlx::query(
            "DELETE FROM engine.persona_story_memories WHERE instance_id = $1 AND story_date < $2",
        )
        .bind(instance_id)
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM engine.persona_story_events WHERE instance_id = $1 AND story_date < $2",
        )
        .bind(instance_id)
        .bind(cutoff)
        .execute(&mut *tx)
        .await?;
        for ev in events {
            let event_id: Uuid = sqlx::query_scalar(
                "INSERT INTO engine.persona_story_events \
                     (owner_uid, instance_id, category, content, story_date) \
                 VALUES ($1, $2, $3, $4, $5) RETURNING id",
            )
            .bind(owner_uid)
            .bind(instance_id)
            .bind(&ev.category)
            .bind(&ev.content)
            .bind(story_date)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO engine.persona_story_memories \
                     (owner_uid, instance_id, event_id, content, embedding, story_date) \
                 VALUES ($1, $2, $3, $4, $5::vector, $6)",
            )
            .bind(owner_uid)
            .bind(instance_id)
            .bind(event_id)
            .bind(&ev.content)
            .bind(crate::memory::format_vector(&ev.embedding))
            .bind(story_date)
            .execute(&mut *tx)
            .await?;
        }
        let res = sqlx::query(
            "UPDATE engine.persona_story_insights SET \
                 city = $2, location = $3, hometown = $4, nationality = $5, \
                 occupation = $6, mbti_guess = $7, love_values = $8, \
                 emotional_needs = $9, life_rhythm = $10, education = $11, \
                 family = $12, relationship_history = $13, social_pattern = $14, \
                 future_plans = $15, finance_status = $16, interests = $17, \
                 personality_traits = $18, preferred_gender = $19, age_min = $20, \
                 age_max = $21, deal_breakers = $22, work_history = $23, \
                 romance_history = $24, family_of_origin = $25, user_relationship = $26, \
                 digest = $27, insight_version = insight_version + 1, \
                 last_run_at = now(), claimed_at = NULL, updated_at = now() \
             WHERE instance_id = $1 AND claimed_at = $28",
        )
        .bind(instance_id)
        .bind(&insight.city)
        .bind(&insight.location)
        .bind(&insight.hometown)
        .bind(&insight.nationality)
        .bind(&insight.occupation)
        .bind(&insight.mbti_guess)
        .bind(&insight.love_values)
        .bind(&insight.emotional_needs)
        .bind(&insight.life_rhythm)
        .bind(&insight.education)
        .bind(&insight.family)
        .bind(&insight.relationship_history)
        .bind(&insight.social_pattern)
        .bind(&insight.future_plans)
        .bind(&insight.finance_status)
        .bind(&insight.interests)
        .bind(&insight.personality_traits)
        .bind(&insight.preferred_gender)
        .bind(insight.age_min)
        .bind(insight.age_max)
        .bind(&insight.deal_breakers)
        .bind(&insight.work_history)
        .bind(&insight.romance_history)
        .bind(&insight.family_of_origin)
        .bind(&insight.user_relationship)
        .bind(digest)
        .bind(claimed_at)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() == 0 {
            // Lost the claim mid-round — drop tx so everything rolls back.
            return Err(sqlx::Error::RowNotFound);
        }
        tx.commit().await
    }

    /// Chat-time resident digest; single query that also performs the
    /// enrollment + stories_enabled check. Blank digest ⇒ None.
    pub async fn fetch_story_digest(
        &self,
        owner_uid: Uuid,
        instance_id: Uuid,
    ) -> Result<Option<String>, sqlx::Error> {
        let row: Option<String> = sqlx::query_scalar(
            "SELECT psi.digest FROM engine.persona_story_insights psi \
             JOIN engine.world_enrollments we \
               ON we.owner_uid = psi.owner_uid AND we.stories_enabled \
             WHERE psi.owner_uid = $1 AND psi.instance_id = $2",
        )
        .bind(owner_uid)
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await?;
        Ok(row.filter(|d| !d.trim().is_empty()))
    }

    /// Cosine top-k story memories for one instance.
    pub async fn search_story_memories(
        &self,
        owner_uid: Uuid,
        instance_id: Uuid,
        query_embedding: &[f32],
        k: i32,
    ) -> Result<Vec<String>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT content FROM engine.persona_story_memories \
             WHERE owner_uid = $1 AND instance_id = $2 \
             ORDER BY embedding <=> $3::vector \
             LIMIT $4",
        )
        .bind(owner_uid)
        .bind(instance_id)
        .bind(crate::memory::format_vector(query_embedding))
        .bind(k as i64)
        .fetch_all(self.pool)
        .await
    }

    /// Recent-life feed for the WM director: per-instance events since
    /// `since`, capped at `cap_per_instance` newest per instance,
    /// chronological within each instance.
    pub async fn events_since(
        &self,
        owner_uid: Uuid,
        since: DateTime<Utc>,
        cap_per_instance: usize,
    ) -> Result<std::collections::HashMap<Uuid, Vec<(String, String)>>, sqlx::Error> {
        let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
            "SELECT instance_id, category, content FROM engine.persona_story_events \
             WHERE owner_uid = $1 AND created_at > $2 \
             ORDER BY created_at ASC",
        )
        .bind(owner_uid)
        .bind(since)
        .fetch_all(self.pool)
        .await?;
        let mut map: std::collections::HashMap<Uuid, Vec<(String, String)>> =
            std::collections::HashMap::new();
        for (inst, category, content) in rows {
            map.entry(inst).or_default().push((category, content));
        }
        for v in map.values_mut() {
            if v.len() > cap_per_instance {
                let drop = v.len() - cap_per_instance;
                v.drain(..drop);
            }
        }
        Ok(map)
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
    async fn chat_evidence_never_pairs_across_sessions(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let inst = seed_instance(&pool, owner, "A", "active").await;
        let s1: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(owner)
        .bind(inst)
        .fetch_one(&pool)
        .await
        .unwrap();
        let s2: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(owner)
        .bind(inst)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Global time order (decreasing "mins ago"): t0 S2-user, t1 S1-user,
        // t2 S2-assistant, t3 S1-assistant. A naive global walk pairs
        // S1-user↔S2-assistant at (t1,t2) — the bug. A session-scoped walk
        // must instead produce S1-user↔S1-assistant and S2-user↔S2-assistant.
        for (session, role, content, mins_ago) in [
            (s2, "user", "S2问题", 40),
            (s1, "user", "S1问题", 30),
            (s2, "assistant", "S2回答", 20),
            (s1, "assistant", "S1回答", 10),
        ] {
            sqlx::query(
                "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
                 VALUES ($1, $2, $3, now() - make_interval(mins => $4))",
            )
            .bind(session)
            .bind(role)
            .bind(content)
            .bind(mins_ago)
            .execute(&pool)
            .await
            .unwrap();
        }
        let pairs = repo.chat_evidence(owner, inst, 7, 60).await.unwrap();
        assert!(
            pairs.contains(&("S1问题".to_string(), "S1回答".to_string())),
            "same-session S1 pair must survive"
        );
        assert!(
            pairs.contains(&("S2问题".to_string(), "S2回答".to_string())),
            "same-session S2 pair must survive"
        );
        assert!(
            !pairs.iter().any(|(u, a)| u == "S1问题" && a == "S2回答"),
            "must not pair across sessions"
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

    fn unit_embedding(seed: usize) -> Vec<f32> {
        let mut v = vec![0.0_f32; 512];
        v[seed % 512] = 1.0;
        v
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_round_writes_events_memories_and_insight(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll_stories(&pool, owner).await;
        let inst = seed_instance(&pool, owner, "A", "active").await;
        touch_activity(&pool, owner, inst, "1 hour").await;
        repo.ensure_insight_rows(8).await.unwrap();
        let (_i, _o, token) = repo.claim_due(EIGHT_H, STALE, WINDOW, 8).await.unwrap()[0];
        let today = Utc::now().date_naive();

        // Pre-existing OLD event+memory (41d, retention 30) → both pruned.
        let old_event: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_story_events \
             (owner_uid, instance_id, category, content, story_date) \
             VALUES ($1, $2, 'life', 'ancient', $3) RETURNING id",
        )
        .bind(owner)
        .bind(inst)
        .bind(today - chrono::Days::new(41))
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.persona_story_memories \
             (owner_uid, instance_id, event_id, content, embedding, story_date) \
             VALUES ($1, $2, $3, 'ancient', $4::vector, $5)",
        )
        .bind(owner)
        .bind(inst)
        .bind(old_event)
        .bind(crate::memory::format_vector(&unit_embedding(9)))
        .bind(today - chrono::Days::new(41))
        .execute(&pool)
        .await
        .unwrap();

        let insight = StoryInsight {
            occupation: Some("咖啡店店主".into()),
            user_relationship: Some("刚确认恋爱关系".into()),
            ..Default::default()
        };
        let events = vec![StoryEventInsert {
            category: "work".into(),
            content: "试营业当天把咖啡机弄坏了".into(),
            embedding: unit_embedding(7),
        }];
        repo.persist_round(
            inst,
            owner,
            &insight,
            "开店进入倒计时",
            &events,
            today,
            30,
            token,
        )
        .await
        .unwrap();

        let (occ, digest, version, claimed): (Option<String>, String, i32, Option<DateTime<Utc>>) =
            sqlx::query_as(
                "SELECT occupation, digest, insight_version, claimed_at \
                 FROM engine.persona_story_insights WHERE instance_id = $1",
            )
            .bind(inst)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(occ.as_deref(), Some("咖啡店店主"));
        assert_eq!(digest, "开店进入倒计时");
        assert_eq!(version, 2);
        assert!(claimed.is_none());

        let events_left: Vec<String> = sqlx::query_scalar(
            "SELECT content FROM engine.persona_story_events WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            events_left,
            vec!["试营业当天把咖啡机弄坏了"],
            "old pruned, new inserted"
        );

        let (mem_content, linked): (String, Uuid) = sqlx::query_as(
            "SELECT m.content, m.event_id FROM engine.persona_story_memories m \
             WHERE m.owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(mem_content, "试营业当天把咖啡机弄坏了", "1:1 mirror");
        let linked_ok: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM engine.persona_story_events WHERE id = $1 AND content = $2)",
        )
        .bind(linked)
        .bind("试营业当天把咖啡机弄坏了")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(linked_ok, "event_id links the mirror row");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn persist_round_aborts_when_claim_lost(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll_stories(&pool, owner).await;
        let inst = seed_instance(&pool, owner, "A", "active").await;
        touch_activity(&pool, owner, inst, "1 hour").await;
        repo.ensure_insight_rows(8).await.unwrap();
        let (_i, _o, token) = repo.claim_due(EIGHT_H, STALE, WINDOW, 8).await.unwrap()[0];
        sqlx::query(
            "UPDATE engine.persona_story_insights SET claimed_at = now() + interval '1 second' \
             WHERE instance_id = $1",
        )
        .bind(inst)
        .execute(&pool)
        .await
        .unwrap();

        let events = vec![StoryEventInsert {
            category: "life".into(),
            content: "late".into(),
            embedding: unit_embedding(3),
        }];
        let res = repo
            .persist_round(
                inst,
                owner,
                &StoryInsight::default(),
                "d",
                &events,
                Utc::now().date_naive(),
                30,
                token,
            )
            .await;
        assert!(res.is_err(), "stale token must abort");
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.persona_story_events WHERE owner_uid = $1",
        )
        .bind(owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 0, "event insert must roll back");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn fetch_story_digest_requires_flag_and_nonblank(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let inst = seed_instance(&pool, owner, "A", "active").await;
        // Enrolled but stories_enabled=false ⇒ None even with a digest present.
        sqlx::query("INSERT INTO engine.world_enrollments (owner_uid) VALUES ($1)")
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO engine.persona_story_insights (instance_id, owner_uid, digest) \
             VALUES ($1, $2, '近况')",
        )
        .bind(inst)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        assert!(repo
            .fetch_story_digest(owner, inst)
            .await
            .unwrap()
            .is_none());
        sqlx::query(
            "UPDATE engine.world_enrollments SET stories_enabled = true WHERE owner_uid = $1",
        )
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            repo.fetch_story_digest(owner, inst)
                .await
                .unwrap()
                .as_deref(),
            Some("近况")
        );
        sqlx::query(
            "UPDATE engine.persona_story_insights SET digest = '  ' WHERE instance_id = $1",
        )
        .bind(inst)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            repo.fetch_story_digest(owner, inst)
                .await
                .unwrap()
                .is_none(),
            "blank ⇒ None"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn search_story_memories_scopes_and_orders(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        enroll_stories(&pool, owner).await;
        let a = seed_instance(&pool, owner, "A", "active").await;
        let b = seed_instance(&pool, owner, "B", "active").await;
        touch_activity(&pool, owner, a, "1 hour").await;
        touch_activity(&pool, owner, b, "1 hour").await;
        repo.ensure_insight_rows(8).await.unwrap();
        let claimed = repo.claim_due(EIGHT_H, STALE, WINDOW, 8).await.unwrap();
        for (inst, token) in claimed.iter().map(|(i, _o, t)| (*i, *t)) {
            let (near, far) = if inst == a {
                ("near-a", "far-a")
            } else {
                ("near-b", "far-b")
            };
            let events = vec![
                StoryEventInsert {
                    category: "life".into(),
                    content: near.into(),
                    embedding: unit_embedding(42),
                },
                StoryEventInsert {
                    category: "life".into(),
                    content: far.into(),
                    embedding: unit_embedding(400),
                },
            ];
            repo.persist_round(
                inst,
                owner,
                &StoryInsight::default(),
                "d",
                &events,
                Utc::now().date_naive(),
                30,
                token,
            )
            .await
            .unwrap();
        }
        let hits = repo
            .search_story_memories(owner, a, &unit_embedding(42), 3)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "only A's memories");
        assert_eq!(hits[0], "near-a", "cosine order");
        assert!(!hits.contains(&"near-b".to_string()));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn events_since_windows_and_caps_per_instance(pool: PgPool) {
        let repo = StoryRepo { pool: &pool };
        let owner = Uuid::new_v4();
        let a = seed_instance(&pool, owner, "A", "active").await;
        let b = seed_instance(&pool, owner, "B", "active").await;
        for (inst, content, mins_ago) in [
            (a, "a-old", 2000_i32),
            (a, "a-1", 30),
            (a, "a-2", 20),
            (b, "b-1", 10),
        ] {
            sqlx::query(
                "INSERT INTO engine.persona_story_events \
                 (owner_uid, instance_id, category, content, story_date, created_at) \
                 VALUES ($1, $2, 'life', $3, current_date, now() - make_interval(mins => $4))",
            )
            .bind(owner)
            .bind(inst)
            .bind(content)
            .bind(mins_ago)
            .execute(&pool)
            .await
            .unwrap();
        }
        let since = Utc::now() - chrono::Duration::hours(24);
        let map = repo.events_since(owner, since, 1).await.unwrap();
        assert_eq!(
            map[&a].iter().map(|(_, c)| c.as_str()).collect::<Vec<_>>(),
            vec!["a-2"],
            "cap keeps the newest, window drops a-old"
        );
        assert_eq!(map[&b].len(), 1);
    }
}

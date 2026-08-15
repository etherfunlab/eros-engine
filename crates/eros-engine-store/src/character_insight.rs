// SPDX-License-Identifier: AGPL-3.0-only
//! The AI character's conversation-derived profile, keyed on the relationship
//! (`persona_instances.id`). Written incrementally by the character-insight
//! extractor; read only by the audit path and the profile route. Deliberately
//! never injected into any prompt.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CharacterInsightsRow {
    pub instance_id: Uuid,
    pub location: Option<String>,
    pub occupation: Option<String>,
    pub current_situation: Option<String>,
    pub desires: Option<String>,
    pub vulnerabilities: Option<String>,
    pub habits: Option<String>,
    pub personal_values: Option<String>,
    pub likes: Vec<String>,
    pub dislikes: Vec<String>,
    pub relationships: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

pub struct CharacterInsightRepo<'a> {
    pub pool: &'a PgPool,
}

impl CharacterInsightRepo<'_> {
    pub async fn load(
        &self,
        instance_id: Uuid,
    ) -> Result<Option<CharacterInsightsRow>, sqlx::Error> {
        sqlx::query_as::<_, CharacterInsightsRow>(
            "SELECT * FROM engine.character_insights WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::seed_persona_instance;

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0047_creates_all_three_tables(pool: PgPool) {
        let instance_id = seed_persona_instance(&pool, Uuid::new_v4()).await;

        // Profile table: every column exists and round-trips, and the array
        // columns default to '{}' rather than NULL.
        sqlx::query(
            "INSERT INTO engine.character_insights \
               (instance_id, location, occupation, current_situation, desires, \
                vulnerabilities, habits, personal_values, likes, dislikes, relationships) \
             VALUES ($1,'公司','兼职策展','连着两周没休息','想去海边','怕被丢下', \
                     '凌晨才睡','很看重守约',$2,$3,$4)",
        )
        .bind(instance_id)
        .bind(vec!["下雨天的味道".to_string()])
        .bind(vec!["被当成小孩哄".to_string()])
        .bind(vec!["妹妹在读高三".to_string()])
        .execute(&pool)
        .await
        .expect("insert into character_insights");

        let row = CharacterInsightRepo { pool: &pool }
            .load(instance_id)
            .await
            .unwrap()
            .expect("row loads");
        assert_eq!(row.location.as_deref(), Some("公司"));
        assert_eq!(row.personal_values.as_deref(), Some("很看重守约"));
        assert_eq!(row.likes, vec!["下雨天的味道"]);
        assert_eq!(row.relationships, vec!["妹妹在读高三"]);

        // Events table exists with every column.
        sqlx::query(
            "INSERT INTO engine.character_insights_events \
               (run_id, instance_id, session_id, message_id, stage, status, payload, \
                model, usage, generation_id) \
             VALUES ($1,$2,$3,$4,'extraction','ok','[]'::jsonb,'m','{}'::jsonb,'g')",
        )
        .bind(Uuid::new_v4())
        .bind(instance_id)
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .expect("insert into character_insights_events");

        // Snapshot table exists.
        sqlx::query(
            "INSERT INTO engine.character_insights_snapshot (instance_id, snapshot, captured_at) \
             VALUES ($1, '{}'::jsonb, now())",
        )
        .bind(instance_id)
        .execute(&pool)
        .await
        .expect("insert into character_insights_snapshot");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn stage_check_rejects_the_human_side_vocabulary(pool: PgPool) {
        // 'facts'/'structured' are the human chain's values. This table names
        // its config blocks instead, so the old vocabulary must not slip in.
        let instance_id = seed_persona_instance(&pool, Uuid::new_v4()).await;
        let err = sqlx::query(
            "INSERT INTO engine.character_insights_events (run_id, instance_id, stage, status) \
             VALUES ($1,$2,'facts','ok')",
        )
        .bind(Uuid::new_v4())
        .bind(instance_id)
        .execute(&pool)
        .await;
        assert!(err.is_err(), "stage='facts' must violate the CHECK");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn deleting_the_instance_cascades_the_profile_but_keeps_the_audit(pool: PgPool) {
        let instance_id = seed_persona_instance(&pool, Uuid::new_v4()).await;
        sqlx::query("INSERT INTO engine.character_insights (instance_id) VALUES ($1)")
            .bind(instance_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO engine.character_insights_events (run_id, instance_id, stage, status) \
             VALUES ($1,$2,'extraction','ok')",
        )
        .bind(Uuid::new_v4())
        .bind(instance_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.character_insights_snapshot (instance_id, snapshot, captured_at) \
             VALUES ($1,'{}'::jsonb, now())",
        )
        .bind(instance_id)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query("DELETE FROM engine.persona_instances WHERE id = $1")
            .bind(instance_id)
            .execute(&pool)
            .await
            .expect("instance deletes — no FK blocks it");

        let profiles: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.character_insights WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(profiles, 0, "profile cascades with the relationship");

        // The audit trail is append-only and deliberately has NO FK, so it
        // survives the instance it describes.
        let events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.character_insights_events WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events, 1);
        let snaps: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.character_insights_snapshot WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(snaps, 1);
    }
}

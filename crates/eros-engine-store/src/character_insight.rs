// SPDX-License-Identifier: AGPL-3.0-only
//! The AI character's conversation-derived profile, keyed on the relationship
//! (`persona_instances.id`). Written incrementally by the character-insight
//! extractor; read only by the audit path and the profile route. Deliberately
//! never injected into any prompt.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// The parsed, typed columns ready to UPSERT. Owned values so the caller can
/// move them straight into `.bind(...)`.
#[derive(Debug, Default, PartialEq)]
pub struct CharacterColumns {
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
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

/// Collect a JSON array under `key` into its string items only. Missing /
/// non-array / non-string items are dropped, yielding `[]` rather than an
/// error: these columns are `Vec<String>` on the Rust side, so a NULL or
/// numeric element would make every later read of the row fail to decode.
fn str_array(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// The single definition of the extraction JSON -> character_insights column
/// mapping. Pure; unit-tested without a database. Every key is top-level —
/// there is no nested object in this schema.
pub fn project_columns(insights: &serde_json::Value) -> CharacterColumns {
    CharacterColumns {
        location: str_field(insights, "location"),
        occupation: str_field(insights, "occupation"),
        current_situation: str_field(insights, "current_situation"),
        desires: str_field(insights, "desires"),
        vulnerabilities: str_field(insights, "vulnerabilities"),
        habits: str_field(insights, "habits"),
        personal_values: str_field(insights, "personal_values"),
        likes: str_array(insights, "likes"),
        dislikes: str_array(insights, "dislikes"),
        relationships: str_array(insights, "relationships"),
    }
}

fn put_str(obj: &mut serde_json::Map<String, serde_json::Value>, key: &str, v: &Option<String>) {
    if let Some(s) = v {
        obj.insert(key.into(), serde_json::Value::String(s.clone()));
    }
}

fn put_arr(obj: &mut serde_json::Map<String, serde_json::Value>, key: &str, v: &[String]) {
    if !v.is_empty() {
        obj.insert(key.into(), serde_json::json!(v));
    }
}

/// Reverse projection: rebuild the extraction-schema JSON from the typed row,
/// for the structuring stage's "existing profile" context. Emits only
/// populated fields (NULL scalars and empty arrays omitted) and never emits
/// `instance_id` / `updated_at` — those are storage bookkeeping, not schema.
pub fn existing_as_extraction_json(row: &CharacterInsightsRow) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    put_str(&mut obj, "location", &row.location);
    put_str(&mut obj, "occupation", &row.occupation);
    put_str(&mut obj, "current_situation", &row.current_situation);
    put_str(&mut obj, "desires", &row.desires);
    put_str(&mut obj, "vulnerabilities", &row.vulnerabilities);
    put_str(&mut obj, "habits", &row.habits);
    put_str(&mut obj, "personal_values", &row.personal_values);
    put_arr(&mut obj, "likes", &row.likes);
    put_arr(&mut obj, "dislikes", &row.dislikes);
    put_arr(&mut obj, "relationships", &row.relationships);
    serde_json::Value::Object(obj)
}

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

    #[test]
    fn project_columns_reads_every_field_from_the_top_level() {
        let v = serde_json::json!({
            "location": "还在公司，加班到十点",
            "occupation": "在用户介绍的画廊做兼职策展",
            "current_situation": "刚接了个大项目，连着两周没休息",
            "desires": "想周末两个人一起去海边",
            "vulnerabilities": "怕被丢下，所以总是先说没关系",
            "habits": "习惯凌晨才睡，早上靠冰美式醒",
            "personal_values": "把守约看得很重",
            "likes": ["喜欢下雨天的味道"],
            "dislikes": ["讨厌被当成小孩哄"],
            "relationships": ["妹妹在读高三，每周打电话催她吃饭"]
        });
        let c = project_columns(&v);
        assert_eq!(c.location.as_deref(), Some("还在公司，加班到十点"));
        assert_eq!(c.occupation.as_deref(), Some("在用户介绍的画廊做兼职策展"));
        assert_eq!(
            c.current_situation.as_deref(),
            Some("刚接了个大项目，连着两周没休息")
        );
        assert_eq!(c.desires.as_deref(), Some("想周末两个人一起去海边"));
        assert_eq!(
            c.vulnerabilities.as_deref(),
            Some("怕被丢下，所以总是先说没关系")
        );
        assert_eq!(c.habits.as_deref(), Some("习惯凌晨才睡，早上靠冰美式醒"));
        assert_eq!(c.personal_values.as_deref(), Some("把守约看得很重"));
        assert_eq!(c.likes, vec!["喜欢下雨天的味道"]);
        assert_eq!(c.dislikes, vec!["讨厌被当成小孩哄"]);
        assert_eq!(c.relationships, vec!["妹妹在读高三，每周打电话催她吃饭"]);
    }

    #[test]
    fn project_columns_missing_fields_are_null_and_empty() {
        let c = project_columns(&serde_json::json!({}));
        assert_eq!(c, CharacterColumns::default());
        assert_eq!(c.location, None);
        assert!(c.likes.is_empty());
    }

    #[test]
    fn project_columns_drops_non_string_array_items_rather_than_erroring() {
        // A `text[]` column is decoded into Vec<String>; a null or number
        // slipping through would make every later read of this row fail to
        // decode, with no way to repair it.
        let v = serde_json::json!({ "likes": ["咖啡", null, 7, "雨天"] });
        let c = project_columns(&v);
        assert_eq!(c.likes, vec!["咖啡", "雨天"]);
    }

    #[test]
    fn project_columns_ignores_unknown_and_genome_owned_keys() {
        // appearance / background / personality_traits are deliberately not
        // columns — the model emitting them anyway must be a no-op, not a panic.
        let v = serde_json::json!({
            "appearance": "长发", "background": "出身贵族",
            "personality_traits": ["温柔"], "location": "家里"
        });
        let c = project_columns(&v);
        assert_eq!(c.location.as_deref(), Some("家里"));
        assert_eq!(
            c,
            CharacterColumns {
                location: Some("家里".into()),
                ..Default::default()
            }
        );
    }

    #[test]
    fn existing_as_extraction_json_round_trips_and_omits_empties() {
        let row = CharacterInsightsRow {
            instance_id: Uuid::new_v4(),
            location: Some("公司".into()),
            occupation: None,
            current_situation: None,
            desires: None,
            vulnerabilities: None,
            habits: None,
            personal_values: None,
            likes: vec!["雨天".into()],
            dislikes: vec![],
            relationships: vec![],
            updated_at: Utc::now(),
        };
        let json = existing_as_extraction_json(&row);
        let obj = json.as_object().expect("object");
        assert_eq!(obj.get("location").and_then(|v| v.as_str()), Some("公司"));
        assert_eq!(obj.get("likes"), Some(&serde_json::json!(["雨天"])));
        // NULL scalars and empty arrays are omitted entirely, so the stage-2
        // prompt is not padded with a wall of nulls.
        assert!(!obj.contains_key("occupation"));
        assert!(!obj.contains_key("dislikes"));
        // instance_id and updated_at are storage bookkeeping, not extraction
        // schema — they must never reach the prompt.
        assert!(!obj.contains_key("instance_id"));
        assert!(!obj.contains_key("updated_at"));

        // Feeding the reverse projection back through project_columns is the
        // identity on every value that survives a store round-trip.
        let back = project_columns(&json);
        assert_eq!(back.location.as_deref(), Some("公司"));
        assert_eq!(back.likes, vec!["雨天"]);
    }
}

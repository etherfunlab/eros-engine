// SPDX-License-Identifier: AGPL-3.0-only
//! The real user's conversation-derived profile, keyed on the relationship
//! (`persona_instances.id`) — what this user has revealed inside ONE
//! relationship. The mirror of `character_insight.rs`.
//!
//! NOT `human_insights`. That table is keyed on `user_id`, answers "who is
//! this user" globally, and feeds prompt injection and matching. This one is
//! per-relationship, record-only, and is deliberately NEVER injected into any
//! prompt (chat, voice, PDE, world).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// The parsed, typed columns ready to UPSERT. Owned values so the caller can
/// move them straight into `.bind(...)`.
#[derive(Debug, Default, PartialEq)]
pub struct UserColumns {
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

/// The single definition of the extraction JSON -> user_insights column
/// mapping. Pure; unit-tested without a database. Every key is top-level.
pub fn project_columns(insights: &serde_json::Value) -> UserColumns {
    UserColumns {
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
pub fn existing_as_extraction_json(row: &UserInsightsRow) -> serde_json::Value {
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
pub struct UserInsightsRow {
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

pub struct UserInsightRepo<'a> {
    pub pool: &'a PgPool,
}

impl UserInsightRepo<'_> {
    /// Apply one extraction result incrementally, and append the post-merge
    /// state to `user_insights_snapshot` in the same statement.
    ///
    /// Merge rules (identical to the other two chains): extracted scalars
    /// overwrite, absent/null scalars keep the stored value, arrays overwrite
    /// only when the extraction produced a non-empty array. There is
    /// deliberately no *explicit* erase path; an empty-string scalar DOES
    /// overwrite (COALESCE only treats NULL as absent, and `""` is non-NULL).
    /// Single statement — no read-modify-write, so concurrent extractions
    /// degrade to column-level (not whole-row) last-write-wins.
    ///
    /// The snapshot rides a data-modifying CTE rather than a second query so it
    /// cannot drift from the row it claims to describe. There is no sweeper for
    /// that table; this is its only writer.
    pub async fn apply_extraction(
        &self,
        instance_id: Uuid,
        insights: &serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        let c = project_columns(insights);
        // A parseable reply carrying none of our keys projects to all-defaults.
        // Writing it would stamp updated_at and append a snapshot for a row with
        // nothing in it, so the read endpoint would report "we have a profile"
        // when we have none. The audit row already records what the model said.
        if c == UserColumns::default() {
            return Ok(());
        }
        sqlx::query(
            "WITH upserted AS ( \
                 INSERT INTO engine.user_insights \
                     (instance_id, location, occupation, current_situation, desires, \
                      vulnerabilities, habits, personal_values, likes, dislikes, relationships) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
                 ON CONFLICT (instance_id) DO UPDATE SET \
                     location          = COALESCE(EXCLUDED.location, user_insights.location), \
                     occupation        = COALESCE(EXCLUDED.occupation, user_insights.occupation), \
                     current_situation = COALESCE(EXCLUDED.current_situation, user_insights.current_situation), \
                     desires           = COALESCE(EXCLUDED.desires, user_insights.desires), \
                     vulnerabilities   = COALESCE(EXCLUDED.vulnerabilities, user_insights.vulnerabilities), \
                     habits            = COALESCE(EXCLUDED.habits, user_insights.habits), \
                     personal_values   = COALESCE(EXCLUDED.personal_values, user_insights.personal_values), \
                     likes             = CASE WHEN EXCLUDED.likes = '{}' \
                                              THEN user_insights.likes ELSE EXCLUDED.likes END, \
                     dislikes          = CASE WHEN EXCLUDED.dislikes = '{}' \
                                              THEN user_insights.dislikes ELSE EXCLUDED.dislikes END, \
                     relationships     = CASE WHEN EXCLUDED.relationships = '{}' \
                                              THEN user_insights.relationships ELSE EXCLUDED.relationships END, \
                     updated_at        = now() \
                 RETURNING * \
             ) \
             INSERT INTO engine.user_insights_snapshot (instance_id, snapshot, captured_at) \
             SELECT instance_id, to_jsonb(upserted), now() FROM upserted",
        )
        .bind(instance_id)
        .bind(c.location)
        .bind(c.occupation)
        .bind(c.current_situation)
        .bind(c.desires)
        .bind(c.vulnerabilities)
        .bind(c.habits)
        .bind(c.personal_values)
        .bind(c.likes)
        .bind(c.dislikes)
        .bind(c.relationships)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn load(&self, instance_id: Uuid) -> Result<Option<UserInsightsRow>, sqlx::Error> {
        sqlx::query_as::<_, UserInsightsRow>(
            "SELECT * FROM engine.user_insights WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_optional(self.pool)
        .await
    }
}

// ─── Audit ─────────────────────────────────────────────────────────

/// One `user_insights_events` row to insert. `payload` is the
/// `{facts, details}` object (`stage='extraction'`), the structured object plus
/// `_existing_keys` (`stage='structuring'`), or `{raw}` on a parse error.
pub struct UserInsightEventInsert<'a> {
    pub run_id: Uuid,
    pub instance_id: Uuid,
    pub session_id: Option<Uuid>,
    pub message_id: Option<Uuid>,
    pub stage: &'a str,
    pub status: &'a str,
    pub payload: Option<serde_json::Value>,
    pub meta: crate::OpenRouterCallMeta,
}

pub struct UserInsightEventRepo<'a> {
    pub pool: &'a PgPool,
}

impl UserInsightEventRepo<'_> {
    /// Append one audit row. Append-only; no FK on `instance_id`, so the trail
    /// outlives the instance it describes.
    pub async fn record(&self, ev: UserInsightEventInsert<'_>) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO engine.user_insights_events \
               (run_id, instance_id, session_id, message_id, stage, status, payload, \
                model, usage, generation_id) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
        )
        .bind(ev.run_id)
        .bind(ev.instance_id)
        .bind(ev.session_id)
        .bind(ev.message_id)
        .bind(ev.stage)
        .bind(ev.status)
        .bind(ev.payload)
        .bind(ev.meta.model)
        .bind(ev.meta.usage)
        .bind(ev.meta.generation_id)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::seed_persona_instance;
    use sqlx::PgPool;
    use uuid::Uuid;

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0055_creates_all_three_tables(pool: PgPool) {
        // session_id / message_id are FK-referenced since 0058.
        let seeded_session = crate::testutil::seed_chat_session(&pool, Uuid::new_v4()).await;
        let seeded_message = crate::testutil::seed_chat_message(&pool, seeded_session).await;
        let instance_id = seed_persona_instance(&pool, Uuid::new_v4()).await;

        sqlx::query(
            "INSERT INTO engine.user_insights \
               (instance_id, location, occupation, current_situation, desires, \
                vulnerabilities, habits, personal_values, likes, dislikes, relationships) \
             VALUES ($1,'深圳南山','后端工程师','刚换组，项目节奏很紧','想年底请长假', \
                     '怕被说不够努力','习惯半夜写代码','很在意说到做到',$2,$3,$4)",
        )
        .bind(instance_id)
        .bind(vec!["爬山".to_string()])
        .bind(vec!["无效会议".to_string()])
        .bind(vec!["母亲住在老家".to_string()])
        .execute(&pool)
        .await
        .expect("insert into user_insights");

        sqlx::query(
            "INSERT INTO engine.user_insights_events \
               (run_id, instance_id, session_id, message_id, stage, status, payload, \
                model, usage, generation_id) \
             VALUES ($1,$2,$3,$4,'extraction','ok','{}'::jsonb,'m','{}'::jsonb,'g')",
        )
        .bind(Uuid::new_v4())
        .bind(instance_id)
        .bind(seeded_session)
        .bind(seeded_message)
        .execute(&pool)
        .await
        .expect("insert into user_insights_events");

        sqlx::query(
            "INSERT INTO engine.user_insights_snapshot (instance_id, snapshot, captured_at) \
             VALUES ($1, '{}'::jsonb, now())",
        )
        .bind(instance_id)
        .execute(&pool)
        .await
        .expect("insert into user_insights_snapshot");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn apply_extraction_merges_incrementally_and_never_erases(pool: PgPool) {
        let instance_id = seed_persona_instance(&pool, Uuid::new_v4()).await;
        let repo = UserInsightRepo { pool: &pool };

        repo.apply_extraction(
            instance_id,
            &serde_json::json!({
                "location": "深圳南山",
                "desires": "想年底请长假",
                "likes": ["爬山"],
            }),
        )
        .await
        .unwrap();

        // Second run: one scalar overwrites, one is absent (keeps), one array
        // is empty (keeps), one array is new (overwrites).
        repo.apply_extraction(
            instance_id,
            &serde_json::json!({
                "location": "回老家了",
                "likes": [],
                "dislikes": ["无效会议"],
            }),
        )
        .await
        .unwrap();

        let row = repo.load(instance_id).await.unwrap().expect("row exists");
        assert_eq!(row.location.as_deref(), Some("回老家了")); // overwritten
        assert_eq!(row.desires.as_deref(), Some("想年底请长假")); // absent => kept
        assert_eq!(row.likes, vec!["爬山".to_string()]); // empty array => kept
        assert_eq!(row.dislikes, vec!["无效会议".to_string()]); // non-empty => written
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn apply_extraction_appends_one_post_merge_snapshot_per_call(pool: PgPool) {
        let instance_id = seed_persona_instance(&pool, Uuid::new_v4()).await;
        let repo = UserInsightRepo { pool: &pool };

        repo.apply_extraction(instance_id, &serde_json::json!({"location": "深圳南山"}))
            .await
            .unwrap();
        repo.apply_extraction(
            instance_id,
            &serde_json::json!({"occupation": "后端工程师"}),
        )
        .await
        .unwrap();

        let snaps: Vec<serde_json::Value> = sqlx::query_scalar(
            "SELECT snapshot FROM engine.user_insights_snapshot \
             WHERE instance_id = $1 ORDER BY captured_at",
        )
        .bind(instance_id)
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(snaps.len(), 2, "one snapshot per applied call");
        // Post-merge, not the delta: the second snapshot carries BOTH fields.
        assert_eq!(snaps[1]["location"], "深圳南山");
        assert_eq!(snaps[1]["occupation"], "后端工程师");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn apply_extraction_ignores_a_reply_carrying_none_of_our_keys(pool: PgPool) {
        let instance_id = seed_persona_instance(&pool, Uuid::new_v4()).await;
        let repo = UserInsightRepo { pool: &pool };

        repo.apply_extraction(instance_id, &serde_json::json!({"mood": "开心"}))
            .await
            .unwrap();

        assert!(
            repo.load(instance_id).await.unwrap().is_none(),
            "an all-defaults projection must not create a row"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn deleting_the_instance_drops_the_profile_but_keeps_the_trail(pool: PgPool) {
        let instance_id = seed_persona_instance(&pool, Uuid::new_v4()).await;
        UserInsightRepo { pool: &pool }
            .apply_extraction(instance_id, &serde_json::json!({"location": "深圳南山"}))
            .await
            .unwrap();
        UserInsightEventRepo { pool: &pool }
            .record(UserInsightEventInsert {
                run_id: Uuid::new_v4(),
                instance_id,
                session_id: None,
                message_id: None,
                stage: "extraction",
                status: "ok",
                payload: None,
                meta: crate::OpenRouterCallMeta::default(),
            })
            .await
            .unwrap();

        sqlx::query("DELETE FROM engine.persona_instances WHERE id = $1")
            .bind(instance_id)
            .execute(&pool)
            .await
            .unwrap();

        let profiles: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM engine.user_insights WHERE instance_id = $1")
                .bind(instance_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(profiles, 0, "CASCADE removes the profile");

        let events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.user_insights_events WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events, 1, "the audit trail outlives the instance");

        let snaps: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM engine.user_insights_snapshot WHERE instance_id = $1",
        )
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(snaps, 1, "snapshots outlive the instance");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn event_repo_round_trips_payload_usage_and_generation_id(pool: PgPool) {
        let instance_id = seed_persona_instance(&pool, Uuid::new_v4()).await;
        let run_id = Uuid::new_v4();
        // session_id / message_id are FK-referenced since 0058.
        let session_id = crate::testutil::seed_chat_session(&pool, Uuid::new_v4()).await;
        let message_id = crate::testutil::seed_chat_message(&pool, session_id).await;

        UserInsightEventRepo { pool: &pool }
            .record(UserInsightEventInsert {
                run_id,
                instance_id,
                session_id: Some(session_id),
                message_id: Some(message_id),
                stage: "structuring",
                status: "ok",
                payload: Some(serde_json::json!({"location": "深圳南山", "_existing_keys": []})),
                meta: crate::OpenRouterCallMeta {
                    generation_id: Some("gen-user".into()),
                    model: Some("ins/m".into()),
                    usage: Some(serde_json::json!({"total_tokens": 11})),
                },
            })
            .await
            .unwrap();

        // Round-trip all three so a bind-order swap fails here, not at the DB.
        let (stage, gen, payload, usage): (
            String,
            Option<String>,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
        ) = sqlx::query_as(
            "SELECT stage, generation_id, payload, usage \
             FROM engine.user_insights_events WHERE run_id = $1",
        )
        .bind(run_id)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(stage, "structuring");
        assert_eq!(gen.as_deref(), Some("gen-user"));
        assert_eq!(
            payload,
            Some(serde_json::json!({"location": "深圳南山", "_existing_keys": []}))
        );
        assert_eq!(usage, Some(serde_json::json!({"total_tokens": 11})));
    }

    #[test]
    fn project_columns_drops_non_string_array_items_and_ignores_unknown_keys() {
        let c = project_columns(&serde_json::json!({
            "location": "深圳南山",
            "occupation": "后端工程师",
            "current_situation": "刚换组",
            "desires": "想请长假",
            "vulnerabilities": "怕被说不够努力",
            "habits": "半夜写代码",
            "personal_values": "说到做到",
            "likes": ["爬山", 42, null],
            "dislikes": ["无效会议"],
            "relationships": ["母亲住在老家"],
            "unknown_key": "ignored",
        }));
        assert_eq!(c.location.as_deref(), Some("深圳南山"));
        assert_eq!(c.personal_values.as_deref(), Some("说到做到"));
        assert_eq!(c.likes, vec!["爬山".to_string()]);
        assert_eq!(c.relationships, vec!["母亲住在老家".to_string()]);
    }

    #[test]
    fn project_columns_reads_every_field_from_the_top_level() {
        // Ten DISTINCT values, distinct from every other test in this file, so
        // a swapped str_field(insights, "habits") <-> str_field(insights,
        // "vulnerabilities") (or any other pairwise swap) fails here instead
        // of compiling silently.
        let v = serde_json::json!({
            "location": "在成都出差，下周三回",
            "occupation": "做用户增长，刚转正",
            "current_situation": "手头项目要 validate，压力大",
            "desires": "想攒钱换个大一点的房子",
            "vulnerabilities": "受不了被当众否定",
            "habits": "喜欢边跑步边听播客",
            "personal_values": "认为公平比效率更重要",
            "likes": ["秋天的桂花香"],
            "dislikes": ["迟到不打招呼的人"],
            "relationships": ["表哥在同一家公司不同组"]
        });
        let c = project_columns(&v);
        assert_eq!(c.location.as_deref(), Some("在成都出差，下周三回"));
        assert_eq!(c.occupation.as_deref(), Some("做用户增长，刚转正"));
        assert_eq!(
            c.current_situation.as_deref(),
            Some("手头项目要 validate，压力大")
        );
        assert_eq!(c.desires.as_deref(), Some("想攒钱换个大一点的房子"));
        assert_eq!(c.vulnerabilities.as_deref(), Some("受不了被当众否定"));
        assert_eq!(c.habits.as_deref(), Some("喜欢边跑步边听播客"));
        assert_eq!(c.personal_values.as_deref(), Some("认为公平比效率更重要"));
        assert_eq!(c.likes, vec!["秋天的桂花香"]);
        assert_eq!(c.dislikes, vec!["迟到不打招呼的人"]);
        assert_eq!(c.relationships, vec!["表哥在同一家公司不同组"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn apply_extraction_round_trips_all_ten_columns(pool: PgPool) {
        // Ten DISTINCT values. Seven of the eleven binds are Option<String> and
        // three are Vec<String>, so a positional swap inside either group is
        // type-correct and silent — only distinct values catch it. This also
        // covers every COALESCE target and every reverse-projection key.
        let instance_id = seed_persona_instance(&pool, Uuid::new_v4()).await;
        let all = serde_json::json!({
            "location": "u-loc-1",
            "occupation": "u-occ-2",
            "current_situation": "u-cur-3",
            "desires": "u-des-4",
            "vulnerabilities": "u-vul-5",
            "habits": "u-hab-6",
            "personal_values": "u-val-7",
            "likes": ["u-lik-8"],
            "dislikes": ["u-dis-9"],
            "relationships": ["u-rel-10"]
        });

        let repo = UserInsightRepo { pool: &pool };
        repo.apply_extraction(instance_id, &all).await.unwrap();

        let row = repo.load(instance_id).await.unwrap().expect("row");
        let back = existing_as_extraction_json(&row);
        assert_eq!(
            back, all,
            "every column must survive the write→read→project cycle in place"
        );
    }

    #[test]
    fn existing_as_extraction_json_emits_only_populated_fields() {
        let row = UserInsightsRow {
            instance_id: Uuid::nil(),
            location: Some("深圳南山".into()),
            occupation: None,
            current_situation: None,
            desires: None,
            vulnerabilities: None,
            habits: None,
            personal_values: None,
            likes: vec!["爬山".into()],
            dislikes: vec![],
            relationships: vec![],
            updated_at: chrono::Utc::now(),
        };
        let v = existing_as_extraction_json(&row);
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 2, "only location + likes, got {obj:?}");
        assert_eq!(obj["location"], "深圳南山");
        assert_eq!(obj["likes"], serde_json::json!(["爬山"]));
        assert!(!obj.contains_key("instance_id"));
        assert!(!obj.contains_key("updated_at"));
    }
}

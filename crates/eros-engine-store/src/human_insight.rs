// SPDX-License-Identifier: AGPL-3.0-only
//! Flat, typed store of the conversation-derived user profile. Written
//! incrementally by the insight extractor (`apply_extraction`); read by
//! prompt building and the profile route.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// The parsed, typed columns ready to UPSERT. Owned values so the caller can
/// move them straight into `.bind(...)`.
#[derive(Debug, Default, PartialEq)]
pub struct ProjectedColumns {
    pub city: Option<String>,
    pub location: Option<String>,
    pub hometown: Option<String>,
    pub nationality: Option<String>,
    pub occupation: Option<String>,
    pub mbti_guess: Option<String>,
    pub love_values: Option<String>,
    pub emotional_needs: Option<String>,
    pub life_rhythm: Option<String>,
    pub interests: Vec<String>,
    pub personality_traits: Vec<String>,
    pub preferred_gender: Option<String>,
    pub age_min: Option<i32>,
    pub age_max: Option<i32>,
    pub deal_breakers: Vec<String>,
    pub education: Option<String>,
    pub family: Option<String>,
    pub relationship_history: Option<String>,
    pub social_pattern: Option<String>,
    pub future_plans: Option<String>,
    pub finance_status: Option<String>,
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

/// Collect a JSON array under `key` into the string items only. Missing /
/// non-array / non-string items are dropped, yielding `[]` rather than an error.
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

/// Parse `matching_preferences.age_range` ([min, max]) into two i32s. Any
/// shape other than a 2-element array of in-range integers yields `(None,
/// None)` — including values outside i32 range, which degrade to NULL rather
/// than wrapping silently.
fn parse_age_range(prefs: Option<&serde_json::Value>) -> (Option<i32>, Option<i32>) {
    prefs
        .and_then(|p| p.get("age_range"))
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            if arr.len() == 2 {
                match (
                    arr[0].as_i64().and_then(|n| i32::try_from(n).ok()),
                    arr[1].as_i64().and_then(|n| i32::try_from(n).ok()),
                ) {
                    (Some(lo), Some(hi)) => Some((Some(lo), Some(hi))),
                    _ => None,
                }
            } else {
                None
            }
        })
        .unwrap_or((None, None))
}

/// The single definition of the extraction JSONB (the `companion_insights`
/// schema the extractor emits) -> human_insights columns mapping. Pure;
/// unit-tested without a database.
pub fn project_columns(insights: &serde_json::Value) -> ProjectedColumns {
    let prefs = insights.get("matching_preferences");
    let (age_min, age_max) = parse_age_range(prefs);
    ProjectedColumns {
        city: str_field(insights, "city"),
        location: str_field(insights, "location"),
        hometown: str_field(insights, "hometown"),
        nationality: str_field(insights, "nationality"),
        occupation: str_field(insights, "occupation"),
        mbti_guess: str_field(insights, "mbti_guess"),
        love_values: str_field(insights, "love_values"),
        emotional_needs: str_field(insights, "emotional_needs"),
        life_rhythm: str_field(insights, "life_rhythm"),
        interests: str_array(insights, "interests"),
        personality_traits: str_array(insights, "personality_traits"),
        preferred_gender: prefs.and_then(|p| str_field(p, "preferred_gender")),
        age_min,
        age_max,
        deal_breakers: prefs
            .map(|p| str_array(p, "deal_breakers"))
            .unwrap_or_default(),
        education: str_field(insights, "education"),
        family: str_field(insights, "family"),
        relationship_history: str_field(insights, "relationship_history"),
        social_pattern: str_field(insights, "social_pattern"),
        future_plans: str_field(insights, "future_plans"),
        finance_status: str_field(insights, "finance_status"),
    }
}

fn put_str(obj: &mut serde_json::Map<String, serde_json::Value>, key: &str, v: &Option<String>) {
    if let Some(s) = v {
        obj.insert(key.into(), serde_json::Value::String(s.clone()));
    }
}

/// Reverse projection: rebuild the extraction-schema JSON shape from the
/// typed row, for the stage-2 prompt's "existing insights" context. Emits
/// only populated fields (NULL scalars and empty arrays are omitted), and
/// re-nests the matching trio into `matching_preferences`; `age_range` is
/// emitted only when both bounds are set. Inverse of `project_columns` for
/// every value that survives a store round-trip.
pub fn existing_as_extraction_json(row: &HumanInsightsRow) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    put_str(&mut obj, "city", &row.city);
    put_str(&mut obj, "location", &row.location);
    put_str(&mut obj, "hometown", &row.hometown);
    put_str(&mut obj, "nationality", &row.nationality);
    put_str(&mut obj, "occupation", &row.occupation);
    put_str(&mut obj, "mbti_guess", &row.mbti_guess);
    put_str(&mut obj, "love_values", &row.love_values);
    put_str(&mut obj, "emotional_needs", &row.emotional_needs);
    put_str(&mut obj, "life_rhythm", &row.life_rhythm);
    if !row.interests.is_empty() {
        obj.insert("interests".into(), serde_json::json!(row.interests));
    }
    if !row.personality_traits.is_empty() {
        obj.insert(
            "personality_traits".into(),
            serde_json::json!(row.personality_traits),
        );
    }
    let mut prefs = serde_json::Map::new();
    put_str(&mut prefs, "preferred_gender", &row.preferred_gender);
    if let (Some(lo), Some(hi)) = (row.age_min, row.age_max) {
        prefs.insert("age_range".into(), serde_json::json!([lo, hi]));
    }
    if !row.deal_breakers.is_empty() {
        prefs.insert("deal_breakers".into(), serde_json::json!(row.deal_breakers));
    }
    if !prefs.is_empty() {
        obj.insert(
            "matching_preferences".into(),
            serde_json::Value::Object(prefs),
        );
    }
    put_str(&mut obj, "education", &row.education);
    put_str(&mut obj, "family", &row.family);
    put_str(&mut obj, "relationship_history", &row.relationship_history);
    put_str(&mut obj, "social_pattern", &row.social_pattern);
    put_str(&mut obj, "future_plans", &row.future_plans);
    put_str(&mut obj, "finance_status", &row.finance_status);
    serde_json::Value::Object(obj)
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct HumanInsightsRow {
    pub user_id: Uuid,
    pub city: Option<String>,
    pub location: Option<String>,
    pub hometown: Option<String>,
    pub nationality: Option<String>,
    pub occupation: Option<String>,
    pub mbti_guess: Option<String>,
    pub love_values: Option<String>,
    pub emotional_needs: Option<String>,
    pub life_rhythm: Option<String>,
    pub interests: Vec<String>,
    pub personality_traits: Vec<String>,
    pub preferred_gender: Option<String>,
    pub age_min: Option<i32>,
    pub age_max: Option<i32>,
    pub deal_breakers: Vec<String>,
    pub education: Option<String>,
    pub family: Option<String>,
    pub relationship_history: Option<String>,
    pub social_pattern: Option<String>,
    pub future_plans: Option<String>,
    pub finance_status: Option<String>,
    pub updated_at: DateTime<Utc>,
}

pub struct HumanInsightRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> HumanInsightRepo<'a> {
    /// Apply one extraction result incrementally: extracted scalars overwrite,
    /// absent/null scalars keep the stored value; arrays overwrite only when
    /// the extraction produced a non-empty array. Single statement — no
    /// read-modify-write, so concurrent extractions degrade to column-level
    /// (not whole-row) last-write-wins. There is deliberately no erase path.
    pub async fn apply_extraction(
        &self,
        user_id: Uuid,
        insights: &serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        let c = project_columns(insights);
        sqlx::query(
            "INSERT INTO engine.human_insights \
                (user_id, city, occupation, mbti_guess, love_values, emotional_needs, \
                 life_rhythm, interests, personality_traits, preferred_gender, \
                 age_min, age_max, deal_breakers, location, hometown, nationality, \
                 education, family, relationship_history, social_pattern, \
                 future_plans, finance_status) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
                     $17, $18, $19, $20, $21, $22) \
             ON CONFLICT (user_id) DO UPDATE SET \
                 city                 = COALESCE(EXCLUDED.city, human_insights.city), \
                 occupation           = COALESCE(EXCLUDED.occupation, human_insights.occupation), \
                 mbti_guess           = COALESCE(EXCLUDED.mbti_guess, human_insights.mbti_guess), \
                 love_values          = COALESCE(EXCLUDED.love_values, human_insights.love_values), \
                 emotional_needs      = COALESCE(EXCLUDED.emotional_needs, human_insights.emotional_needs), \
                 life_rhythm          = COALESCE(EXCLUDED.life_rhythm, human_insights.life_rhythm), \
                 interests            = CASE WHEN EXCLUDED.interests = '{}' \
                                             THEN human_insights.interests ELSE EXCLUDED.interests END, \
                 personality_traits   = CASE WHEN EXCLUDED.personality_traits = '{}' \
                                             THEN human_insights.personality_traits ELSE EXCLUDED.personality_traits END, \
                 preferred_gender     = COALESCE(EXCLUDED.preferred_gender, human_insights.preferred_gender), \
                 age_min              = COALESCE(EXCLUDED.age_min, human_insights.age_min), \
                 age_max              = COALESCE(EXCLUDED.age_max, human_insights.age_max), \
                 deal_breakers        = CASE WHEN EXCLUDED.deal_breakers = '{}' \
                                             THEN human_insights.deal_breakers ELSE EXCLUDED.deal_breakers END, \
                 location             = COALESCE(EXCLUDED.location, human_insights.location), \
                 hometown             = COALESCE(EXCLUDED.hometown, human_insights.hometown), \
                 nationality          = COALESCE(EXCLUDED.nationality, human_insights.nationality), \
                 education            = COALESCE(EXCLUDED.education, human_insights.education), \
                 family               = COALESCE(EXCLUDED.family, human_insights.family), \
                 relationship_history = COALESCE(EXCLUDED.relationship_history, human_insights.relationship_history), \
                 social_pattern       = COALESCE(EXCLUDED.social_pattern, human_insights.social_pattern), \
                 future_plans         = COALESCE(EXCLUDED.future_plans, human_insights.future_plans), \
                 finance_status       = COALESCE(EXCLUDED.finance_status, human_insights.finance_status), \
                 updated_at           = now()",
        )
        .bind(user_id)
        .bind(c.city)
        .bind(c.occupation)
        .bind(c.mbti_guess)
        .bind(c.love_values)
        .bind(c.emotional_needs)
        .bind(c.life_rhythm)
        .bind(c.interests)
        .bind(c.personality_traits)
        .bind(c.preferred_gender)
        .bind(c.age_min)
        .bind(c.age_max)
        .bind(c.deal_breakers)
        .bind(c.location)
        .bind(c.hometown)
        .bind(c.nationality)
        .bind(c.education)
        .bind(c.family)
        .bind(c.relationship_history)
        .bind(c.social_pattern)
        .bind(c.future_plans)
        .bind(c.finance_status)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn load(&self, user_id: Uuid) -> Result<Option<HumanInsightsRow>, sqlx::Error> {
        sqlx::query_as::<_, HumanInsightsRow>(
            "SELECT * FROM engine.human_insights WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Append one snapshot row per human_insights record at the given
    /// instant. `snapshot` is `to_jsonb(row)` — self-contained, and future
    /// column additions flow through with no snapshot migration. Single
    /// server-side INSERT … SELECT; returns rows written.
    pub async fn snapshot_all_users(
        &self,
        captured_at: DateTime<Utc>,
    ) -> Result<usize, sqlx::Error> {
        let res = sqlx::query(
            "INSERT INTO engine.human_insights_snapshot (user_id, snapshot, captured_at)
             SELECT hi.user_id, to_jsonb(hi), $1 FROM engine.human_insights hi",
        )
        .bind(captured_at)
        .execute(self.pool)
        .await?;
        Ok(res.rows_affected() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_columns_full_blob() {
        let v = serde_json::json!({
            "city": "Shanghai",
            "occupation": "engineer",
            "mbti_guess": "INFP",
            "love_values": "slow burn",
            "emotional_needs": "validation",
            "life_rhythm": "night owl",
            "interests": ["coffee", "hiking"],
            "personality_traits": ["curious", "calm"],
            "matching_preferences": {
                "preferred_gender": "any",
                "age_range": [18, 30],
                "deal_breakers": ["smoking"]
            }
        });
        let c = project_columns(&v);
        assert_eq!(c.city.as_deref(), Some("Shanghai"));
        assert_eq!(c.mbti_guess.as_deref(), Some("INFP"));
        assert_eq!(c.interests, vec!["coffee", "hiking"]);
        assert_eq!(c.personality_traits, vec!["curious", "calm"]);
        assert_eq!(c.preferred_gender.as_deref(), Some("any"));
        assert_eq!(c.age_min, Some(18));
        assert_eq!(c.age_max, Some(30));
        assert_eq!(c.deal_breakers, vec!["smoking"]);
    }

    #[test]
    fn project_columns_geo_fields() {
        let v = serde_json::json!({
            "city": "深圳", "location": "台北", "hometown": "新界", "nationality": "中国香港"
        });
        let c = project_columns(&v);
        assert_eq!(c.city.as_deref(), Some("深圳"));
        assert_eq!(c.location.as_deref(), Some("台北"));
        assert_eq!(c.hometown.as_deref(), Some("新界"));
        assert_eq!(c.nationality.as_deref(), Some("中国香港"));
    }

    #[test]
    fn project_columns_expansion_fields() {
        let v = serde_json::json!({
            "education": "985 本科计算机，毕业五年",
            "family": "独生子，父母在老家，未婚",
            "relationship_history": "去年和异地恋三年的前任分手，之后一直单身",
            "social_pattern": "周末宅家，社交主要靠线上游戏开黑",
            "future_plans": "想两年内跳去外企，攒钱在老家买房",
            "finance_status": "月薪两万出头，房贷压力大"
        });
        let c = project_columns(&v);
        assert_eq!(c.education.as_deref(), Some("985 本科计算机，毕业五年"));
        assert_eq!(c.family.as_deref(), Some("独生子，父母在老家，未婚"));
        assert_eq!(
            c.relationship_history.as_deref(),
            Some("去年和异地恋三年的前任分手，之后一直单身")
        );
        assert_eq!(
            c.social_pattern.as_deref(),
            Some("周末宅家，社交主要靠线上游戏开黑")
        );
        assert_eq!(
            c.future_plans.as_deref(),
            Some("想两年内跳去外企，攒钱在老家买房")
        );
        assert_eq!(
            c.finance_status.as_deref(),
            Some("月薪两万出头，房贷压力大")
        );
    }

    #[test]
    fn project_columns_missing_fields_are_null_and_empty() {
        let c = project_columns(&serde_json::json!({}));
        assert_eq!(c.city, None);
        assert_eq!(c.location, None);
        assert_eq!(c.hometown, None);
        assert_eq!(c.nationality, None);
        assert_eq!(c.preferred_gender, None);
        assert_eq!(c.age_min, None);
        assert_eq!(c.age_max, None);
        assert!(c.interests.is_empty());
        assert!(c.personality_traits.is_empty());
        assert!(c.deal_breakers.is_empty());
        assert_eq!(c.education, None);
        assert_eq!(c.family, None);
        assert_eq!(c.relationship_history, None);
        assert_eq!(c.social_pattern, None);
        assert_eq!(c.future_plans, None);
        assert_eq!(c.finance_status, None);
    }

    #[test]
    fn project_columns_malformed_age_range_is_null() {
        for bad in [
            serde_json::json!("18-30"),
            serde_json::json!([18]),
            serde_json::json!([18, 30, 40]),
            serde_json::json!(["a", "b"]),
            serde_json::json!([i64::MAX, 30]),
        ] {
            let v = serde_json::json!({ "matching_preferences": { "age_range": bad } });
            let c = project_columns(&v);
            assert_eq!(c.age_min, None, "age_min for {bad:?}");
            assert_eq!(c.age_max, None, "age_max for {bad:?}");
        }
    }

    #[test]
    fn project_columns_array_drops_non_strings() {
        let v = serde_json::json!({ "interests": ["coffee", 1, null, "tea"] });
        let c = project_columns(&v);
        assert_eq!(c.interests, vec!["coffee", "tea"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn arrays_roundtrip(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        repo.apply_extraction(
            user_id,
            &serde_json::json!({
                "interests": ["a", "b"],
                "personality_traits": ["x"],
                "matching_preferences": { "deal_breakers": ["d1", "d2"] }
            }),
        )
        .await
        .unwrap();
        let row = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(row.interests, vec!["a", "b"]);
        assert_eq!(row.personality_traits, vec!["x"]);
        assert_eq!(row.deal_breakers, vec!["d1", "d2"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn geo_fields_roundtrip(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        repo.apply_extraction(
            user_id,
            &serde_json::json!({
                "city": "深圳", "location": "台北", "hometown": "新界", "nationality": "中国香港"
            }),
        )
        .await
        .unwrap();
        let row = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(row.city.as_deref(), Some("深圳"));
        assert_eq!(row.location.as_deref(), Some("台北"));
        assert_eq!(row.hometown.as_deref(), Some("新界"));
        assert_eq!(row.nationality.as_deref(), Some("中国香港"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn expansion_fields_roundtrip(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        repo.apply_extraction(
            user_id,
            &serde_json::json!({
                "education": "本科在读",
                "family": "有个妹妹",
                "relationship_history": "单身两年",
                "social_pattern": "喜欢小圈子聚会",
                "future_plans": "准备考研",
                "finance_status": "靠奖学金和兼职"
            }),
        )
        .await
        .unwrap();
        let row = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(row.education.as_deref(), Some("本科在读"));
        assert_eq!(row.family.as_deref(), Some("有个妹妹"));
        assert_eq!(row.relationship_history.as_deref(), Some("单身两年"));
        assert_eq!(row.social_pattern.as_deref(), Some("喜欢小圈子聚会"));
        assert_eq!(row.future_plans.as_deref(), Some("准备考研"));
        assert_eq!(row.finance_status.as_deref(), Some("靠奖学金和兼职"));

        repo.apply_extraction(user_id, &serde_json::json!({ "city": "上海" }))
            .await
            .unwrap();
        let kept = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(kept.education.as_deref(), Some("本科在读"), "absent keeps");
        assert_eq!(kept.finance_status.as_deref(), Some("靠奖学金和兼职"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn gin_overlap_query_matches(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let want = Uuid::new_v4();
        let other = Uuid::new_v4();
        repo.apply_extraction(
            want,
            &serde_json::json!({ "interests": ["coffee", "hiking"] }),
        )
        .await
        .unwrap();
        repo.apply_extraction(other, &serde_json::json!({ "interests": ["gaming"] }))
            .await
            .unwrap();

        let hits: Vec<Uuid> =
            sqlx::query_scalar("SELECT user_id FROM engine.human_insights WHERE interests && $1")
                .bind(vec!["coffee".to_string()])
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(hits, vec![want]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn load_returns_none_for_unknown_user(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        assert!(repo.load(Uuid::new_v4()).await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn snapshot_all_users_writes_one_row_per_user_at_same_ts(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        repo.apply_extraction(u1, &serde_json::json!({ "city": "深圳" }))
            .await
            .unwrap();
        repo.apply_extraction(u2, &serde_json::json!({ "occupation": "工程师" }))
            .await
            .unwrap();

        let t = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let n = repo.snapshot_all_users(t).await.unwrap();
        assert_eq!(n, 2, "one row per human_insights row");

        let rows: Vec<(Uuid, serde_json::Value, DateTime<Utc>)> = sqlx::query_as(
            "SELECT user_id, snapshot, captured_at
               FROM engine.human_insights_snapshot ORDER BY user_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        for (uid, snap, ts) in &rows {
            assert_eq!(*ts, t, "every row in the same fire shares captured_at");
            assert_eq!(
                snap["user_id"],
                serde_json::json!(uid),
                "snapshot is the full row"
            );
        }
        let by_u1 = rows.iter().find(|(u, _, _)| u == &u1).unwrap();
        assert_eq!(by_u1.1["city"], "深圳");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn snapshot_all_users_with_empty_table_writes_nothing(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let t = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        assert_eq!(repo.snapshot_all_users(t).await.unwrap(), 0);
    }

    #[test]
    fn existing_as_extraction_json_emits_only_populated() {
        let row = HumanInsightsRow {
            user_id: Uuid::new_v4(),
            city: Some("深圳".into()),
            location: None,
            hometown: None,
            nationality: None,
            occupation: Some("后端工程师".into()),
            mbti_guess: None,
            love_values: None,
            emotional_needs: None,
            life_rhythm: None,
            interests: vec!["手冲咖啡".into()],
            personality_traits: vec![],
            preferred_gender: Some("female".into()),
            age_min: Some(22),
            age_max: Some(30),
            deal_breakers: vec![],
            education: None,
            family: None,
            relationship_history: None,
            social_pattern: None,
            future_plans: None,
            finance_status: None,
            updated_at: chrono::Utc::now(),
        };
        let v = existing_as_extraction_json(&row);
        let obj = v.as_object().unwrap();
        assert_eq!(obj["city"], "深圳");
        assert_eq!(obj["occupation"], "后端工程师");
        assert_eq!(obj["interests"], serde_json::json!(["手冲咖啡"]));
        // Empty arrays and NULL scalars are omitted entirely.
        assert!(!obj.contains_key("personality_traits"));
        assert!(!obj.contains_key("mbti_guess"));
        assert!(!obj.contains_key("location"));
        // user_id / updated_at are row bookkeeping, not insight fields.
        assert!(!obj.contains_key("user_id"));
        assert!(!obj.contains_key("updated_at"));
        // Matching trio re-nests into matching_preferences.
        let prefs = obj["matching_preferences"].as_object().unwrap();
        assert_eq!(prefs["preferred_gender"], "female");
        assert_eq!(prefs["age_range"], serde_json::json!([22, 30]));
        assert!(!prefs.contains_key("deal_breakers"));
    }

    #[test]
    fn existing_as_extraction_json_empty_row_is_empty_object() {
        let row = HumanInsightsRow {
            user_id: Uuid::new_v4(),
            city: None,
            location: None,
            hometown: None,
            nationality: None,
            occupation: None,
            mbti_guess: None,
            love_values: None,
            emotional_needs: None,
            life_rhythm: None,
            interests: vec![],
            personality_traits: vec![],
            preferred_gender: None,
            age_min: None,
            age_max: None,
            deal_breakers: vec![],
            education: None,
            family: None,
            relationship_history: None,
            social_pattern: None,
            future_plans: None,
            finance_status: None,
            updated_at: chrono::Utc::now(),
        };
        assert_eq!(existing_as_extraction_json(&row), serde_json::json!({}));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn apply_extraction_creates_then_merges_incrementally(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let user_id = Uuid::new_v4();

        repo.apply_extraction(
            user_id,
            &serde_json::json!({ "city": "深圳", "interests": ["手冲咖啡"] }),
        )
        .await
        .unwrap();
        let first = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(first.city.as_deref(), Some("深圳"));
        assert_eq!(first.interests, vec!["手冲咖啡"]);

        // Second extraction touches OTHER fields: previous values survive.
        repo.apply_extraction(user_id, &serde_json::json!({ "occupation": "后端工程师" }))
            .await
            .unwrap();
        let second = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(
            second.city.as_deref(),
            Some("深圳"),
            "absent scalar keeps old value"
        );
        assert_eq!(
            second.interests,
            vec!["手冲咖啡"],
            "absent array keeps old value"
        );
        assert_eq!(second.occupation.as_deref(), Some("后端工程师"));
        assert!(second.updated_at >= first.updated_at);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn apply_extraction_present_overwrites_absent_keeps(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let user_id = Uuid::new_v4();

        repo.apply_extraction(
            user_id,
            &serde_json::json!({
                "city": "深圳",
                "interests": ["a"],
                "matching_preferences": { "preferred_gender": "any", "age_range": [20, 28] }
            }),
        )
        .await
        .unwrap();

        repo.apply_extraction(
            user_id,
            &serde_json::json!({
                "city": "上海",
                "interests": ["b", "c"],
                "city_null_probe": null
            }),
        )
        .await
        .unwrap();

        let row = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(
            row.city.as_deref(),
            Some("上海"),
            "present scalar overwrites"
        );
        assert_eq!(row.interests, vec!["b", "c"], "non-empty array overwrites");
        assert_eq!(
            row.preferred_gender.as_deref(),
            Some("any"),
            "untouched nested field kept"
        );
        assert_eq!(row.age_min, Some(20));
        assert_eq!(row.age_max, Some(28));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn apply_extraction_null_and_empty_array_cannot_erase(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let user_id = Uuid::new_v4();

        repo.apply_extraction(
            user_id,
            &serde_json::json!({ "city": "深圳", "interests": ["a"] }),
        )
        .await
        .unwrap();
        // Explicit null / empty array behave like absent: no erase path.
        repo.apply_extraction(
            user_id,
            &serde_json::json!({ "city": null, "interests": [] }),
        )
        .await
        .unwrap();

        let row = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(row.city.as_deref(), Some("深圳"), "null does not erase");
        assert_eq!(row.interests, vec!["a"], "empty array does not erase");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn apply_extraction_roundtrips_through_reverse_projection(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let input = serde_json::json!({
            "city": "深圳",
            "hometown": "长沙",
            "interests": ["爬山", "手冲咖啡"],
            "education": "985 本科计算机",
            "matching_preferences": {
                "preferred_gender": "female",
                "age_range": [22, 30],
                "deal_breakers": ["抽烟"]
            }
        });
        repo.apply_extraction(user_id, &input).await.unwrap();
        let row = repo.load(user_id).await.unwrap().unwrap();
        // The reverse projection reproduces exactly what was stored.
        assert_eq!(existing_as_extraction_json(&row), input);
    }
}

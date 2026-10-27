// SPDX-License-Identifier: AGPL-3.0-only
//! Flat, typed store of the conversation-derived user profile. Replaced
//! wholesale by the profile rebuild (`replace_profile`); read by prompt
//! building, the profile route and the export endpoint.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// Accepted `education_level` codes, lowest to highest.
pub const EDUCATION_LEVELS: &[&str] = &[
    "secondary_or_below",
    "associate",
    "bachelor",
    "master",
    "doctorate",
];

/// Accepted `education_status` codes.
pub const EDUCATION_STATUSES: &[&str] = &["enrolled", "graduated"];

/// The profile row's columns, in the order every read selects them. Named
/// explicitly because the table still carries seven retired columns (city,
/// location, hometown, nationality, preferred_gender, age_min, age_max) that
/// nothing reads or writes.
const ROW_COLUMNS: &str = "user_id, occupation, mbti_guess, love_values, emotional_needs, \
     life_rhythm, interests, personality_traits, deal_breakers, education, \
     education_level, education_status, family, relationship_history, \
     social_pattern, future_plans, finance_status, kinks, ideology, \
     mental_health, updated_at";

/// True for a value that carries no information: blank, a schema-echo token
/// (`string`, `null`, `unknown`, `N/A`, `未知`, `无`), or a `<…>` / `「<…>」`
/// template slot the model left unfilled.
pub fn is_placeholder(s: &str) -> bool {
    let t = s.trim();
    t.is_empty()
        || ["string", "null", "unknown", "n/a"]
            .iter()
            .any(|p| t.eq_ignore_ascii_case(p))
        || t == "未知"
        || t == "无"
        || (t.starts_with('<') && t.ends_with('>'))
        || (t.starts_with("「<") && t.ends_with(">」"))
}

/// The typed profile columns a rebuild writes. Owned values so the caller can
/// move them straight into `.bind(...)`.
#[derive(Debug, Default, PartialEq)]
pub struct ProjectedColumns {
    pub occupation: Option<String>,
    pub mbti_guess: Option<String>,
    pub love_values: Option<String>,
    pub emotional_needs: Option<String>,
    pub life_rhythm: Option<String>,
    pub interests: Vec<String>,
    pub personality_traits: Vec<String>,
    pub deal_breakers: Vec<String>,
    pub education: Option<String>,
    pub education_level: Option<String>,
    pub education_status: Option<String>,
    pub family: Option<String>,
    pub relationship_history: Option<String>,
    pub social_pattern: Option<String>,
    pub future_plans: Option<String>,
    pub finance_status: Option<String>,
    pub kinks: Vec<String>,
    pub ideology: Option<String>,
    pub mental_health: Option<String>,
}

impl ProjectedColumns {
    /// True when no column carries a value.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A scalar field: `Some` only for a JSON string that is not a placeholder.
fn scalar(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !is_placeholder(s))
        .map(String::from)
}

/// An array field: the non-placeholder string items. A missing or non-array
/// value yields `[]`.
fn list(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .filter(|s| !is_placeholder(s))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// `education_level` is an object `{"level", "status"}`; each half is kept
/// only when it is one of the accepted codes.
fn education_code(v: &serde_json::Value, key: &str, accepted: &[&str]) -> Option<String> {
    v.get("education_level")
        .and_then(|e| e.get(key))
        .and_then(|x| x.as_str())
        .filter(|s| accepted.contains(s))
        .map(String::from)
}

/// The single definition of the profile-rebuild JSON -> `human_insights`
/// columns mapping. Pure; unit-tested without a database.
pub fn project_profile(v: &serde_json::Value) -> ProjectedColumns {
    ProjectedColumns {
        occupation: scalar(v, "occupation"),
        mbti_guess: scalar(v, "mbti_guess"),
        love_values: scalar(v, "love_values"),
        emotional_needs: scalar(v, "emotional_needs"),
        life_rhythm: scalar(v, "life_rhythm"),
        interests: list(v, "interests"),
        personality_traits: list(v, "personality_traits"),
        deal_breakers: list(v, "deal_breakers"),
        education: scalar(v, "education"),
        education_level: education_code(v, "level", EDUCATION_LEVELS),
        education_status: education_code(v, "status", EDUCATION_STATUSES),
        family: scalar(v, "family"),
        relationship_history: scalar(v, "relationship_history"),
        social_pattern: scalar(v, "social_pattern"),
        future_plans: scalar(v, "future_plans"),
        finance_status: scalar(v, "finance_status"),
        kinks: list(v, "kinks"),
        ideology: scalar(v, "ideology"),
        mental_health: scalar(v, "mental_health"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct HumanInsightsRow {
    pub user_id: Uuid,
    pub occupation: Option<String>,
    pub mbti_guess: Option<String>,
    pub love_values: Option<String>,
    pub emotional_needs: Option<String>,
    pub life_rhythm: Option<String>,
    pub interests: Vec<String>,
    pub personality_traits: Vec<String>,
    pub deal_breakers: Vec<String>,
    pub education: Option<String>,
    pub education_level: Option<String>,
    pub education_status: Option<String>,
    pub family: Option<String>,
    pub relationship_history: Option<String>,
    pub social_pattern: Option<String>,
    pub future_plans: Option<String>,
    pub finance_status: Option<String>,
    pub kinks: Vec<String>,
    pub ideology: Option<String>,
    pub mental_health: Option<String>,
    pub updated_at: DateTime<Utc>,
}

pub struct HumanInsightRepo<'a> {
    pub pool: &'a PgPool,
}

impl<'a> HumanInsightRepo<'a> {
    /// Write the whole profile in one statement: every 2.0 column takes the
    /// incoming value, so a rebuild that omits a field clears it (empty arrays
    /// included). The seven retired columns are never touched.
    pub async fn replace_profile(
        &self,
        user_id: Uuid,
        c: &ProjectedColumns,
    ) -> Result<(), sqlx::Error> {
        write_profile(self.pool, user_id, c).await
    }

    /// `replace_profile` for a writer that holds the user's rebuild claim: the
    /// profile is written only while `human_profile_state` still has a claimed
    /// row for the user, and the write runs under that row's lock. The erase
    /// deletes the state row before the profile, so with the lock it either
    /// waits for this write and then deletes the row written here, or has
    /// already removed the state row, in which case this write is refused
    /// instead of re-creating a profile the erase just removed. Returns whether
    /// the profile was written.
    pub async fn replace_profile_if_claimed(
        &self,
        user_id: Uuid,
        c: &ProjectedColumns,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let claimed: Option<Uuid> = sqlx::query_scalar(
            "SELECT user_id FROM engine.human_profile_state \
             WHERE user_id = $1 AND claimed_at IS NOT NULL FOR UPDATE",
        )
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if claimed.is_none() {
            tx.rollback().await?;
            return Ok(false);
        }
        write_profile(&mut *tx, user_id, c).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Rows changed strictly after the `(since, after_user)` keyset tuple, in
    /// `(updated_at, user_id)` order, at most `limit`. Backs the insights
    /// export endpoint; the tuple comparison (not a bare `updated_at >`) is
    /// what keeps rows sharing one timestamp from being skipped or repeated
    /// across a page boundary. Uses `idx_human_insights_updated_user` (0069).
    pub async fn list_changes(
        &self,
        since: DateTime<Utc>,
        after_user: Uuid,
        limit: i64,
    ) -> Result<Vec<HumanInsightsRow>, sqlx::Error> {
        sqlx::query_as::<_, HumanInsightsRow>(&format!(
            "SELECT {ROW_COLUMNS} FROM engine.human_insights \
             WHERE (updated_at, user_id) > ($1, $2) \
             ORDER BY updated_at, user_id \
             LIMIT $3"
        ))
        .bind(since)
        .bind(after_user)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    pub async fn load(&self, user_id: Uuid) -> Result<Option<HumanInsightsRow>, sqlx::Error> {
        sqlx::query_as::<_, HumanInsightsRow>(&format!(
            "SELECT {ROW_COLUMNS} FROM engine.human_insights WHERE user_id = $1"
        ))
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

/// The profile upsert shared by `replace_profile` and
/// `replace_profile_if_claimed`.
async fn write_profile<'e>(
    exec: impl sqlx::PgExecutor<'e>,
    user_id: Uuid,
    c: &ProjectedColumns,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO engine.human_insights \
            (user_id, occupation, mbti_guess, love_values, emotional_needs, \
             life_rhythm, interests, personality_traits, deal_breakers, education, \
             education_level, education_status, family, relationship_history, \
             social_pattern, future_plans, finance_status, kinks, ideology, \
             mental_health) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, \
                 $16, $17, $18, $19, $20) \
         ON CONFLICT (user_id) DO UPDATE SET \
             occupation           = EXCLUDED.occupation, \
             mbti_guess           = EXCLUDED.mbti_guess, \
             love_values          = EXCLUDED.love_values, \
             emotional_needs      = EXCLUDED.emotional_needs, \
             life_rhythm          = EXCLUDED.life_rhythm, \
             interests            = EXCLUDED.interests, \
             personality_traits   = EXCLUDED.personality_traits, \
             deal_breakers        = EXCLUDED.deal_breakers, \
             education            = EXCLUDED.education, \
             education_level      = EXCLUDED.education_level, \
             education_status     = EXCLUDED.education_status, \
             family               = EXCLUDED.family, \
             relationship_history = EXCLUDED.relationship_history, \
             social_pattern       = EXCLUDED.social_pattern, \
             future_plans         = EXCLUDED.future_plans, \
             finance_status       = EXCLUDED.finance_status, \
             kinks                = EXCLUDED.kinks, \
             ideology             = EXCLUDED.ideology, \
             mental_health        = EXCLUDED.mental_health, \
             updated_at           = now()",
    )
    .bind(user_id)
    .bind(&c.occupation)
    .bind(&c.mbti_guess)
    .bind(&c.love_values)
    .bind(&c.emotional_needs)
    .bind(&c.life_rhythm)
    .bind(&c.interests)
    .bind(&c.personality_traits)
    .bind(&c.deal_breakers)
    .bind(&c.education)
    .bind(&c.education_level)
    .bind(&c.education_status)
    .bind(&c.family)
    .bind(&c.relationship_history)
    .bind(&c.social_pattern)
    .bind(&c.future_plans)
    .bind(&c.finance_status)
    .bind(&c.kinks)
    .bind(&c.ideology)
    .bind(&c.mental_health)
    .execute(exec)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_profile_json() -> serde_json::Value {
        serde_json::json!({
            "occupation": "后端工程师",
            "mbti_guess": "INFP",
            "love_values": "慢热",
            "emotional_needs": "被理解",
            "life_rhythm": "夜猫子",
            "interests": ["爬山", "手冲咖啡"],
            "personality_traits": ["温柔", "内向"],
            "deal_breakers": ["抽烟"],
            "education": "985 本科计算机",
            "education_level": { "level": "bachelor", "status": "enrolled" },
            "family": "独生女，父母在杭州",
            "relationship_history": "单身两年",
            "social_pattern": "小圈子聚会",
            "future_plans": "想开工作室",
            "finance_status": "攒钱中",
            "kinks": ["束缚"],
            "ideology": "偏自由派",
            "mental_health": "长期失眠"
        })
    }

    #[test]
    fn project_profile_reads_every_2_0_field() {
        let c = project_profile(&full_profile_json());
        assert_eq!(
            c,
            ProjectedColumns {
                occupation: Some("后端工程师".into()),
                mbti_guess: Some("INFP".into()),
                love_values: Some("慢热".into()),
                emotional_needs: Some("被理解".into()),
                life_rhythm: Some("夜猫子".into()),
                interests: vec!["爬山".into(), "手冲咖啡".into()],
                personality_traits: vec!["温柔".into(), "内向".into()],
                deal_breakers: vec!["抽烟".into()],
                education: Some("985 本科计算机".into()),
                education_level: Some("bachelor".into()),
                education_status: Some("enrolled".into()),
                family: Some("独生女，父母在杭州".into()),
                relationship_history: Some("单身两年".into()),
                social_pattern: Some("小圈子聚会".into()),
                future_plans: Some("想开工作室".into()),
                finance_status: Some("攒钱中".into()),
                kinks: vec!["束缚".into()],
                ideology: Some("偏自由派".into()),
                mental_health: Some("长期失眠".into()),
            }
        );
        assert!(!c.is_empty());
    }

    #[test]
    fn project_profile_drops_placeholders() {
        for bad in [
            "string",
            "String",
            "null",
            "unknown",
            "N/A",
            "n/a",
            "未知",
            "无",
            "",
            "   ",
            "<城市>",
            "「<城市>」",
        ] {
            let c = project_profile(&serde_json::json!({ "occupation": bad, "ideology": bad }));
            assert_eq!(c.occupation, None, "occupation for {bad:?}");
            assert_eq!(c.ideology, None, "ideology for {bad:?}");
        }
        let c = project_profile(&serde_json::json!({
            "interests": ["string", "周末常去爬山", "<兴趣>", "  ", "未知"],
            "kinks": ["N/A"],
        }));
        assert_eq!(c.interests, vec!["周末常去爬山"]);
        assert!(c.kinks.is_empty());
    }

    #[test]
    fn is_placeholder_keeps_real_values() {
        for ok in [
            "设计师",
            "无业游民",
            "未知数",
            "a < b",
            "<开头",
            "结尾>",
            "「爬山」",
        ] {
            assert!(!is_placeholder(ok), "{ok:?} is a real value");
        }
    }

    #[test]
    fn project_profile_drops_wrong_types_and_unknown_keys() {
        let c = project_profile(&serde_json::json!({
            "interests": "爬山",
            "occupation": 3,
            "city": "x",
            "age_min": 20,
            "education_level": "本科",
            "kinks": [1, null, "束缚"],
        }));
        assert_eq!(
            c,
            ProjectedColumns {
                kinks: vec!["束缚".into()],
                ..Default::default()
            }
        );
        assert!(project_profile(&serde_json::json!("not an object")).is_empty());
        assert!(project_profile(&serde_json::json!(null)).is_empty());
    }

    #[test]
    fn project_profile_validates_education_enums() {
        let c = project_profile(&serde_json::json!({
            "education_level": { "level": "phd", "status": "enrolled" }
        }));
        assert_eq!(c.education_level, None, "unknown level dropped");
        assert_eq!(
            c.education_status.as_deref(),
            Some("enrolled"),
            "status is validated independently"
        );

        let c = project_profile(&serde_json::json!({
            "education_level": { "level": "master", "status": "dropped_out" }
        }));
        assert_eq!(c.education_level.as_deref(), Some("master"));
        assert_eq!(c.education_status, None);

        for level in EDUCATION_LEVELS {
            let c = project_profile(&serde_json::json!({ "education_level": { "level": level } }));
            assert_eq!(c.education_level.as_deref(), Some(*level));
        }
    }

    #[test]
    fn is_empty_true_for_default() {
        assert!(ProjectedColumns::default().is_empty());
        assert!(project_profile(&serde_json::json!({})).is_empty());
        let c = ProjectedColumns {
            kinks: vec!["x".into()],
            ..Default::default()
        };
        assert!(!c.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn replace_profile_overwrites_every_2_0_column_and_clears_omitted(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let user_id = Uuid::new_v4();

        repo.replace_profile(user_id, &project_profile(&full_profile_json()))
            .await
            .unwrap();
        let first = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(first.occupation.as_deref(), Some("后端工程师"));
        assert_eq!(first.mbti_guess.as_deref(), Some("INFP"));
        assert_eq!(first.love_values.as_deref(), Some("慢热"));
        assert_eq!(first.emotional_needs.as_deref(), Some("被理解"));
        assert_eq!(first.life_rhythm.as_deref(), Some("夜猫子"));
        assert_eq!(first.interests, vec!["爬山", "手冲咖啡"]);
        assert_eq!(first.personality_traits, vec!["温柔", "内向"]);
        assert_eq!(first.deal_breakers, vec!["抽烟"]);
        assert_eq!(first.education.as_deref(), Some("985 本科计算机"));
        assert_eq!(first.education_level.as_deref(), Some("bachelor"));
        assert_eq!(first.education_status.as_deref(), Some("enrolled"));
        assert_eq!(first.family.as_deref(), Some("独生女，父母在杭州"));
        assert_eq!(first.relationship_history.as_deref(), Some("单身两年"));
        assert_eq!(first.social_pattern.as_deref(), Some("小圈子聚会"));
        assert_eq!(first.future_plans.as_deref(), Some("想开工作室"));
        assert_eq!(first.finance_status.as_deref(), Some("攒钱中"));
        assert_eq!(first.kinks, vec!["束缚"]);
        assert_eq!(first.ideology.as_deref(), Some("偏自由派"));
        assert_eq!(first.mental_health.as_deref(), Some("长期失眠"));

        // A second rebuild that only knows the occupation clears all the rest.
        repo.replace_profile(
            user_id,
            &project_profile(&serde_json::json!({ "occupation": "设计师" })),
        )
        .await
        .unwrap();
        let second = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(second.occupation.as_deref(), Some("设计师"));
        assert_eq!(second.mbti_guess, None);
        assert_eq!(second.love_values, None);
        assert_eq!(second.emotional_needs, None);
        assert_eq!(second.life_rhythm, None);
        assert!(second.interests.is_empty());
        assert!(second.personality_traits.is_empty());
        assert!(second.deal_breakers.is_empty());
        assert_eq!(second.education, None);
        assert_eq!(second.education_level, None);
        assert_eq!(second.education_status, None);
        assert_eq!(second.family, None);
        assert_eq!(second.relationship_history, None);
        assert_eq!(second.social_pattern, None);
        assert_eq!(second.future_plans, None);
        assert_eq!(second.finance_status, None);
        assert!(second.kinks.is_empty());
        assert_eq!(second.ideology, None);
        assert_eq!(second.mental_health, None);
        assert!(second.updated_at > first.updated_at, "updated_at advances");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn replace_profile_if_claimed_refuses_once_the_claim_is_gone(pool: PgPool) {
        use crate::human_fact::{FactInput, FactSource, HumanFactRepo};
        use crate::profile_state::ProfileStateRepo;

        let repo = HumanInsightRepo { pool: &pool };
        let state = ProfileStateRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        let cols = project_profile(&serde_json::json!({ "occupation": "设计师" }));
        let stale = std::time::Duration::from_secs(600);

        HumanFactRepo { pool: &pool }
            .upsert_run(
                user_id,
                &[FactInput::from_json(&serde_json::json!({ "content": "是设计师" })).unwrap()],
                &FactSource {
                    session_id: None,
                    message_id: None,
                    run_id: None,
                },
            )
            .await
            .unwrap();
        state.bump(user_id, 1).await.unwrap();

        // A state row nobody has claimed is not a rebuild in flight.
        assert!(!repo
            .replace_profile_if_claimed(user_id, &cols)
            .await
            .unwrap());
        assert!(repo.load(user_id).await.unwrap().is_none());

        assert!(state.claim(user_id, stale).await.unwrap());
        assert!(repo
            .replace_profile_if_claimed(user_id, &cols)
            .await
            .unwrap());
        assert_eq!(
            repo.load(user_id)
                .await
                .unwrap()
                .unwrap()
                .occupation
                .as_deref(),
            Some("设计师")
        );

        // The erase removes the profile and the state row while a rebuild still
        // holds its reply: the late write must not bring the profile back.
        sqlx::query("DELETE FROM engine.human_insights WHERE user_id = $1")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM engine.human_profile_state WHERE user_id = $1")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!repo
            .replace_profile_if_claimed(user_id, &cols)
            .await
            .unwrap());
        assert!(repo.load(user_id).await.unwrap().is_none());
    }

    /// An erase that has deleted the state row but not yet committed holds its
    /// lock: the guarded write waits for it, then finds the row gone.
    #[sqlx::test(migrations = "./migrations")]
    async fn replace_profile_if_claimed_waits_for_an_erase_holding_the_state_row(pool: PgPool) {
        use crate::profile_state::ProfileStateRepo;

        let user_id = Uuid::new_v4();
        let state = ProfileStateRepo { pool: &pool };
        state.bump(user_id, 1).await.unwrap();
        assert!(state
            .claim(user_id, std::time::Duration::from_secs(600))
            .await
            .unwrap());

        let mut erase = pool.begin().await.unwrap();
        sqlx::query("DELETE FROM engine.human_profile_state WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *erase)
            .await
            .unwrap();

        let write = tokio::spawn({
            let pool = pool.clone();
            async move {
                let cols = project_profile(&serde_json::json!({ "occupation": "设计师" }));
                HumanInsightRepo { pool: &pool }
                    .replace_profile_if_claimed(user_id, &cols)
                    .await
            }
        });
        crate::testutil::wait_for_lock_wait(&pool).await;
        assert!(!write.is_finished(), "the write waits for the erase");
        erase.commit().await.unwrap();

        assert!(!write.await.unwrap().unwrap(), "the claim is gone");
        assert!(HumanInsightRepo { pool: &pool }
            .load(user_id)
            .await
            .unwrap()
            .is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn replace_profile_leaves_retired_columns_untouched(pool: PgPool) {
        let user_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.human_insights (user_id, city, age_min) VALUES ($1, 'x', 18)",
        )
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

        let repo = HumanInsightRepo { pool: &pool };
        repo.replace_profile(
            user_id,
            &project_profile(&serde_json::json!({ "occupation": "设计师" })),
        )
        .await
        .unwrap();

        let (city, age_min): (Option<String>, Option<i32>) =
            sqlx::query_as("SELECT city, age_min FROM engine.human_insights WHERE user_id = $1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(city.as_deref(), Some("x"));
        assert_eq!(age_min, Some(18));
        let row = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(row.occupation.as_deref(), Some("设计师"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn arrays_roundtrip(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let user_id = Uuid::new_v4();
        repo.replace_profile(
            user_id,
            &project_profile(&serde_json::json!({
                "interests": ["a", "b"],
                "personality_traits": ["x"],
                "deal_breakers": ["d1", "d2"],
                "kinks": ["k"]
            })),
        )
        .await
        .unwrap();
        let row = repo.load(user_id).await.unwrap().unwrap();
        assert_eq!(row.interests, vec!["a", "b"]);
        assert_eq!(row.personality_traits, vec!["x"]);
        assert_eq!(row.deal_breakers, vec!["d1", "d2"]);
        assert_eq!(row.kinks, vec!["k"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn gin_overlap_query_matches(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let want = Uuid::new_v4();
        let other = Uuid::new_v4();
        repo.replace_profile(
            want,
            &project_profile(&serde_json::json!({ "interests": ["coffee", "hiking"] })),
        )
        .await
        .unwrap();
        repo.replace_profile(
            other,
            &project_profile(&serde_json::json!({ "interests": ["gaming"] })),
        )
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
        repo.replace_profile(
            u1,
            &project_profile(&serde_json::json!({ "occupation": "设计师" })),
        )
        .await
        .unwrap();
        repo.replace_profile(
            u2,
            &project_profile(&serde_json::json!({ "occupation": "工程师" })),
        )
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
        assert_eq!(by_u1.1["occupation"], "设计师");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn snapshot_all_users_with_empty_table_writes_nothing(pool: PgPool) {
        let repo = HumanInsightRepo { pool: &pool };
        let t = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        assert_eq!(repo.snapshot_all_users(t).await.unwrap(), 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn human_insights_has_keyset_index_on_updated_at_user_id(pool: PgPool) {
        let indexdef: String = sqlx::query_scalar(
            "SELECT indexdef FROM pg_indexes \
             WHERE schemaname = 'engine' AND tablename = 'human_insights' \
               AND indexname = 'idx_human_insights_updated_user'",
        )
        .fetch_one(&pool)
        .await
        .expect("index idx_human_insights_updated_user exists");
        assert!(
            indexdef.contains("(updated_at, user_id)"),
            "index must be on (updated_at, user_id), got: {indexdef}"
        );
    }

    async fn seed_insight_at(pool: &PgPool, user_id: Uuid, at: chrono::DateTime<chrono::Utc>) {
        sqlx::query(
            "INSERT INTO engine.human_insights (user_id, interests, updated_at) \
             VALUES ($1, '{\"jazz\"}', $2)",
        )
        .bind(user_id)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_changes_orders_by_updated_at_then_user_id_and_filters_since(pool: PgPool) {
        use chrono::{Duration, TimeZone, Utc};
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        // Ids chosen so user_id order contradicts timestamp order: `ORDER BY user_id`
        // alone would yield `after` first and `before` last, failing deterministically.
        let before = Uuid::from_u128(0xffff_ffff_ffff_ffff_ffff_ffff_ffff_fff0);
        let after = Uuid::from_u128(0x1);
        let mut tied: Vec<Uuid> = [0x10u128, 0x20, 0x30].map(Uuid::from_u128).to_vec();
        seed_insight_at(&pool, before, t - Duration::seconds(1)).await;
        seed_insight_at(&pool, after, t + Duration::seconds(1)).await;
        for id in &tied {
            seed_insight_at(&pool, *id, t).await;
        }
        tied.sort(); // keyset order within one timestamp is user_id order

        let repo = HumanInsightRepo { pool: &pool };

        // Walk with limit 2 from the epoch until a short page.
        let mut walked: Vec<Uuid> = Vec::new();
        let mut cursor = (chrono::DateTime::<Utc>::UNIX_EPOCH, Uuid::nil());
        loop {
            let page = repo.list_changes(cursor.0, cursor.1, 2).await.unwrap();
            walked.extend(page.iter().map(|r| r.user_id));
            if page.len() < 2 {
                break;
            }
            let last = page.last().unwrap();
            cursor = (last.updated_at, last.user_id);
        }
        let mut expected = vec![before];
        expected.extend(tied.iter().copied());
        expected.push(after);
        assert_eq!(
            walked, expected,
            "every row exactly once, in (updated_at, user_id) order"
        );

        // `since` drops rows strictly before it and keeps the tied and later ones.
        let rows: Vec<Uuid> = repo
            .list_changes(t, Uuid::nil(), 10)
            .await
            .unwrap()
            .iter()
            .map(|r| r.user_id)
            .collect();
        assert!(!rows.contains(&before), "row before `since` is excluded");
        let mut expected = tied.clone();
        expected.push(after);
        assert_eq!(rows, expected, "tied rows and the later row are included");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_changes_since_without_after_user_includes_rows_at_since(pool: PgPool) {
        use chrono::{TimeZone, Utc};
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let id = Uuid::new_v4();
        seed_insight_at(&pool, id, t).await;

        let repo = HumanInsightRepo { pool: &pool };
        // since == the row's updated_at, after_user == nil: the row is *returned*
        // (nil sorts below every real UUID). Callers that want to skip it must
        // pass both fields of `next`. This is the contract; do not "fix" it.
        let rows = repo.list_changes(t, Uuid::nil(), 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user_id, id);

        // Passing the row's own (updated_at, user_id) skips it.
        let rows = repo.list_changes(t, id, 10).await.unwrap();
        assert!(rows.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn list_changes_returns_2_0_columns(pool: PgPool) {
        use chrono::{TimeZone, Utc};
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.human_insights \
               (user_id, occupation, interests, personality_traits, deal_breakers, \
                finance_status, education_level, education_status, kinks, ideology, \
                mental_health, updated_at) \
             VALUES ($1, 'engineer', '{\"jazz\",\"hiking\"}', '{\"calm\"}', '{\"smoking\"}', \
                     'stable', 'master', 'graduated', '{\"k1\"}', 'liberal', 'anxious', $2)",
        )
        .bind(id)
        .bind(t)
        .execute(&pool)
        .await
        .unwrap();

        let repo = HumanInsightRepo { pool: &pool };
        let rows = repo
            .list_changes(chrono::DateTime::<Utc>::UNIX_EPOCH, Uuid::nil(), 10)
            .await
            .unwrap();
        let r = &rows[0];
        assert_eq!(r.occupation.as_deref(), Some("engineer"));
        assert_eq!(r.interests, vec!["jazz", "hiking"]);
        assert_eq!(r.personality_traits, vec!["calm"]);
        assert_eq!(r.deal_breakers, vec!["smoking"]);
        assert_eq!(r.finance_status.as_deref(), Some("stable"));
        assert_eq!(r.education_level.as_deref(), Some("master"));
        assert_eq!(r.education_status.as_deref(), Some("graduated"));
        assert_eq!(r.kinks, vec!["k1"]);
        assert_eq!(r.ideology.as_deref(), Some("liberal"));
        assert_eq!(r.mental_health.as_deref(), Some("anxious"));
        assert_eq!(r.updated_at, t);
    }
}

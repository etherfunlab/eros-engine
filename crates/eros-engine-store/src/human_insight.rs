// SPDX-License-Identifier: AGPL-3.0-only
//! Flat, typed projection of the soft (conversation-derived) profile for
//! user<->user matching. The JSONB->columns mapping lives ONLY in
//! `project_columns` so the source/trigger can be repointed later without
//! touching callers. `companion_insights` remains the source of truth.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// The parsed, typed columns ready to UPSERT. Owned values so the caller can
/// move them straight into `.bind(...)`.
#[derive(Debug, Default, PartialEq)]
pub struct ProjectedColumns {
    pub city: Option<String>,
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
}

fn str_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

/// Collect a JSON array under `key` into the string items only. Missing /
/// non-array / non-string items are dropped, yielding `[]` rather than an error.
fn str_array(v: &serde_json::Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|a| a.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// Parse `matching_preferences.age_range` ([min, max]) into two i32s. Any
/// shape other than a 2-element all-integer array yields `(None, None)`.
fn parse_age_range(prefs: Option<&serde_json::Value>) -> (Option<i32>, Option<i32>) {
    prefs
        .and_then(|p| p.get("age_range"))
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            if arr.len() == 2 {
                match (arr[0].as_i64(), arr[1].as_i64()) {
                    (Some(lo), Some(hi)) => Some((Some(lo as i32), Some(hi as i32))),
                    _ => None,
                }
            } else {
                None
            }
        })
        .unwrap_or((None, None))
}

/// The single definition of the companion_insights JSONB -> human_insights
/// columns mapping. Pure; unit-tested without a database.
pub fn project_columns(insights: &serde_json::Value) -> ProjectedColumns {
    let prefs = insights.get("matching_preferences");
    let (age_min, age_max) = parse_age_range(prefs);
    ProjectedColumns {
        city: str_field(insights, "city"),
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
        deal_breakers: prefs.map(|p| str_array(p, "deal_breakers")).unwrap_or_default(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct HumanInsightsRow {
    pub user_id: Uuid,
    pub city: Option<String>,
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
    pub updated_at: DateTime<Utc>,
}

pub struct HumanInsightRepo<'a> {
    pub pool: &'a PgPool,
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
    fn project_columns_missing_fields_are_null_and_empty() {
        let c = project_columns(&serde_json::json!({}));
        assert_eq!(c.city, None);
        assert_eq!(c.preferred_gender, None);
        assert_eq!(c.age_min, None);
        assert_eq!(c.age_max, None);
        assert!(c.interests.is_empty());
        assert!(c.personality_traits.is_empty());
        assert!(c.deal_breakers.is_empty());
    }

    #[test]
    fn project_columns_malformed_age_range_is_null() {
        for bad in [
            serde_json::json!("18-30"),
            serde_json::json!([18]),
            serde_json::json!([18, 30, 40]),
            serde_json::json!(["a", "b"]),
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
}

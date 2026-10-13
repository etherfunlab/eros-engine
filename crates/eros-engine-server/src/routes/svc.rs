// SPDX-License-Identifier: AGPL-3.0-only
//! Service-to-service routes (`/svc/v1/*`).
//!
//! A third auth class next to the public and bearer-JWT sub-trees: admitted by
//! a shared secret header, never by a user token. The whole sub-tree is left
//! unmounted when the deployment did not configure the secret (see
//! `routes::router`), so an unconfigured deployment answers on `/svc/*`
//! exactly as it does for any unknown path.

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::human_insight::{HumanInsightRepo, HumanInsightsRow};

use crate::byok::constant_time_eq;
use crate::error::AppError;
use crate::state::AppState;

pub(crate) const EXPORT_SECRET_HEADER: &str = "x-insights-export-secret";
const DEFAULT_LIMIT: i64 = 500;
const MAX_LIMIT: i64 = 1000;

/// One `human_insights` row as exported. All 21 data columns: the export does
/// not guess which ones a consumer wants.
#[derive(Serialize, ToSchema)]
pub struct InsightExportRow {
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

impl From<HumanInsightsRow> for InsightExportRow {
    fn from(r: HumanInsightsRow) -> Self {
        Self {
            user_id: r.user_id,
            city: r.city,
            location: r.location,
            hometown: r.hometown,
            nationality: r.nationality,
            occupation: r.occupation,
            mbti_guess: r.mbti_guess,
            love_values: r.love_values,
            emotional_needs: r.emotional_needs,
            life_rhythm: r.life_rhythm,
            interests: r.interests,
            personality_traits: r.personality_traits,
            preferred_gender: r.preferred_gender,
            age_min: r.age_min,
            age_max: r.age_max,
            deal_breakers: r.deal_breakers,
            education: r.education,
            family: r.family,
            relationship_history: r.relationship_history,
            social_pattern: r.social_pattern,
            future_plans: r.future_plans,
            finance_status: r.finance_status,
            updated_at: r.updated_at,
        }
    }
}

/// Keyset position. Pass both fields back as `since` + `after_user`.
#[derive(Serialize, ToSchema)]
pub struct ChangesCursor {
    pub updated_at: DateTime<Utc>,
    pub user_id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct InsightChangesResponse {
    pub profiles: Vec<InsightExportRow>,
    /// Present iff the page is full; `null` means the walk is complete.
    pub next: Option<ChangesCursor>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ChangesQuery {
    /// RFC 3339. Default: Unix epoch (walk everything).
    pub since: Option<DateTime<Utc>>,
    /// Tie-break within `since`. Default: nil UUID. Pass `next.user_id`.
    pub after_user: Option<Uuid>,
    /// Default 500, clamped to 1..=1000.
    pub limit: Option<i64>,
}

/// Default 500, clamped to 1..=1000.
fn clamp_limit(requested: Option<i64>) -> i64 {
    requested.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// 403 unless the header matches the configured secret. The route is only
/// mounted when the secret is set, so `None` here is unreachable in practice;
/// it still fails closed.
fn admit(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    let Some(secret) = state.config.insights_export.secret.as_deref() else {
        return Err(AppError::Forbidden("insights export is not enabled".into()));
    };
    let sent = headers
        .get(EXPORT_SECRET_HEADER)
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    if constant_time_eq(sent, secret.as_bytes()) {
        Ok(())
    } else {
        Err(AppError::Forbidden(
            "insights export secret missing or wrong".into(),
        ))
    }
}

/// `human_insights` rows changed since a keyset cursor, for a downstream
/// service that mirrors the conversation-derived profile. Reads the live table
/// (not the append-only snapshot archive): `updated_at` is the signal that a
/// person's profile changed.
#[utoipa::path(
    get,
    path = "/svc/v1/insights/changes",
    tag = "svc",
    params(ChangesQuery),
    responses(
        (status = 200, body = InsightChangesResponse),
        (status = 403, description = "x-insights-export-secret missing or wrong")
    ),
    security(("insights_export_secret" = []))
)]
async fn insights_changes(
    State(state): State<AppState>,
    headers: HeaderMap,
    q: Result<Query<ChangesQuery>, QueryRejection>,
) -> Result<Json<InsightChangesResponse>, AppError> {
    // Extraction of `q` happens before this body runs, but a rejected query is
    // only reported after admission succeeds: a caller without the secret gets
    // 403 even when its query string is malformed, and no DB access precedes `admit`.
    admit(&state, &headers)?;
    let Query(q) = q.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let since = q.since.unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let after_user = q.after_user.unwrap_or(Uuid::nil());
    let limit = clamp_limit(q.limit);

    let rows = HumanInsightRepo { pool: &state.pool }
        .list_changes(since, after_user, limit)
        .await?;
    let next = if rows.len() as i64 == limit {
        rows.last().map(|r| ChangesCursor {
            updated_at: r.updated_at,
            user_id: r.user_id,
        })
    } else {
        None
    };
    Ok(Json(InsightChangesResponse {
        profiles: rows.into_iter().map(InsightExportRow::from).collect(),
        next,
    }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(insights_changes))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use chrono::{TimeZone, Utc};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::{clamp_limit, EXPORT_SECRET_HEADER};
    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::build_router;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn app_with_secret(pool: sqlx::PgPool, secret: Option<&str>) -> Router {
        let mut state = test_state(pool);
        state.config.insights_export.secret = secret.map(String::from);
        build_router(state)
    }

    async fn seed(pool: &sqlx::PgPool, at: chrono::DateTime<Utc>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.human_insights (user_id, interests, updated_at) \
             VALUES ($1, '{\"jazz\"}', $2)",
        )
        .bind(id)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn get(app: Router, uri: &str, secret: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::get(uri);
        if let Some(s) = secret {
            req = req.header(EXPORT_SECRET_HEADER, s);
        }
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, body)
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn unmounted_when_secret_unset_behaves_like_any_unknown_path(pool: sqlx::PgPool) {
        let (svc, _) = get(
            app_with_secret(pool.clone(), None),
            "/svc/v1/insights/changes",
            Some(SECRET),
        )
        .await;
        let (unknown, _) = get(
            app_with_secret(pool, None),
            "/svc-does-not-exist/zzz",
            Some(SECRET),
        )
        .await;
        assert_eq!(
            svc, unknown,
            "/svc must be indistinguishable from an unknown path"
        );
        assert_ne!(svc, StatusCode::FORBIDDEN);
        assert_ne!(svc, StatusCode::OK);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn missing_or_wrong_secret_is_forbidden(pool: sqlx::PgPool) {
        let (status, body) = get(
            app_with_secret(pool.clone(), Some(SECRET)),
            "/svc/v1/insights/changes",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "forbidden");

        let (status, wrong_body) = get(
            app_with_secret(pool.clone(), Some(SECRET)),
            "/svc/v1/insights/changes",
            Some("wrong"),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(wrong_body["error"], "forbidden");

        // Same length as the configured secret, last byte changed.
        let mut same_len = SECRET.as_bytes().to_vec();
        *same_len.last_mut().unwrap() ^= 0x01;
        let same_len = String::from_utf8(same_len).unwrap();
        assert_eq!(same_len.len(), SECRET.len());
        let (status, same_len_body) = get(
            app_with_secret(pool, Some(SECRET)),
            "/svc/v1/insights/changes",
            Some(&same_len),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            same_len_body, body,
            "wrong secret body equals missing-header body"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn full_page_carries_next_and_short_page_does_not(pool: sqlx::PgPool) {
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let mut ids = [
            seed(&pool, t).await,
            seed(&pool, t).await,
            seed(&pool, t).await,
        ];
        ids.sort();

        let (status, p1) = get(
            app_with_secret(pool.clone(), Some(SECRET)),
            "/svc/v1/insights/changes?limit=2",
            Some(SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(p1["profiles"].as_array().unwrap().len(), 2);
        assert!(p1["next"].is_object(), "full page must carry next");
        assert_eq!(p1["profiles"][0]["interests"], serde_json::json!(["jazz"]));

        let next_ts = p1["next"]["updated_at"].as_str().unwrap();
        let next_user = p1["next"]["user_id"].as_str().unwrap();
        let (status, p2) = get(
            app_with_secret(pool, Some(SECRET)),
            &format!("/svc/v1/insights/changes?limit=2&since={next_ts}&after_user={next_user}"),
            Some(SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(p2["profiles"].as_array().unwrap().len(), 1);
        assert!(p2["next"].is_null(), "short page must not carry next");

        let walked: Vec<String> = p1["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .chain(p2["profiles"].as_array().unwrap())
            .map(|r| r["user_id"].as_str().unwrap().to_string())
            .collect();
        let expected: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
        assert_eq!(walked, expected);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn limit_is_clamped_to_1_1000(pool: sqlx::PgPool) {
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        seed(&pool, t).await;
        seed(&pool, t).await;

        for bad in ["0", "-5"] {
            let (status, body) = get(
                app_with_secret(pool.clone(), Some(SECRET)),
                &format!("/svc/v1/insights/changes?limit={bad}"),
                Some(SECRET),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "limit={bad}");
            assert_eq!(
                body["profiles"].as_array().unwrap().len(),
                1,
                "limit={bad} clamps to 1"
            );
        }
        let (status, body) = get(
            app_with_secret(pool, Some(SECRET)),
            "/svc/v1/insights/changes?limit=5000",
            Some(SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["profiles"].as_array().unwrap().len(), 2);
        assert!(
            body["next"].is_null(),
            "2 rows < clamped limit 1000 ⇒ no next"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn admission_outranks_query_errors(pool: sqlx::PgPool) {
        for secret in [None, Some("wrong")] {
            let (status, body) = get(
                app_with_secret(pool.clone(), Some(SECRET)),
                "/svc/v1/insights/changes?limit=abc",
                secret,
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "secret={secret:?}");
            assert_eq!(body["error"], "forbidden");
        }

        let (status, body) = get(
            app_with_secret(pool, Some(SECRET)),
            "/svc/v1/insights/changes?limit=abc",
            Some(SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "bad_request");
    }

    #[test]
    fn insight_export_row_maps_fields_by_name() {
        use eros_engine_store::human_insight::HumanInsightsRow;

        use super::InsightExportRow;

        let user_id = Uuid::new_v4();
        let updated_at = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let row = HumanInsightsRow {
            user_id,
            city: Some("city".into()),
            location: Some("location".into()),
            hometown: Some("hometown".into()),
            nationality: Some("nationality".into()),
            occupation: Some("occupation".into()),
            mbti_guess: Some("mbti_guess".into()),
            love_values: Some("love_values".into()),
            emotional_needs: Some("emotional_needs".into()),
            life_rhythm: Some("life_rhythm".into()),
            interests: vec!["interests_0".into(), "interests_1".into()],
            personality_traits: vec!["personality_traits_0".into()],
            preferred_gender: Some("preferred_gender".into()),
            age_min: Some(21),
            age_max: Some(35),
            deal_breakers: vec!["deal_breakers_0".into(), "deal_breakers_1".into()],
            education: Some("education".into()),
            family: Some("family".into()),
            relationship_history: Some("relationship_history".into()),
            social_pattern: Some("social_pattern".into()),
            future_plans: Some("future_plans".into()),
            finance_status: Some("finance_status".into()),
            updated_at,
        };

        let out = InsightExportRow::from(row);
        assert_eq!(out.user_id, user_id);
        assert_eq!(out.city.as_deref(), Some("city"));
        assert_eq!(out.location.as_deref(), Some("location"));
        assert_eq!(out.hometown.as_deref(), Some("hometown"));
        assert_eq!(out.nationality.as_deref(), Some("nationality"));
        assert_eq!(out.occupation.as_deref(), Some("occupation"));
        assert_eq!(out.mbti_guess.as_deref(), Some("mbti_guess"));
        assert_eq!(out.love_values.as_deref(), Some("love_values"));
        assert_eq!(out.emotional_needs.as_deref(), Some("emotional_needs"));
        assert_eq!(out.life_rhythm.as_deref(), Some("life_rhythm"));
        assert_eq!(out.interests, vec!["interests_0", "interests_1"]);
        assert_eq!(out.personality_traits, vec!["personality_traits_0"]);
        assert_eq!(out.preferred_gender.as_deref(), Some("preferred_gender"));
        assert_eq!(out.age_min, Some(21));
        assert_eq!(out.age_max, Some(35));
        assert_eq!(
            out.deal_breakers,
            vec!["deal_breakers_0", "deal_breakers_1"]
        );
        assert_eq!(out.education.as_deref(), Some("education"));
        assert_eq!(out.family.as_deref(), Some("family"));
        assert_eq!(
            out.relationship_history.as_deref(),
            Some("relationship_history")
        );
        assert_eq!(out.social_pattern.as_deref(), Some("social_pattern"));
        assert_eq!(out.future_plans.as_deref(), Some("future_plans"));
        assert_eq!(out.finance_status.as_deref(), Some("finance_status"));
        assert_eq!(out.updated_at, updated_at);
    }

    #[test]
    fn clamp_limit_defaults_and_bounds() {
        assert_eq!(clamp_limit(None), 500);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(-5)), 1);
        assert_eq!(clamp_limit(Some(5000)), 1000);
        assert_eq!(clamp_limit(Some(7)), 7);
    }

    #[test]
    fn openapi_lists_the_export_route() {
        let api = crate::routes::openapi_for_tests();
        assert!(api["paths"]["/svc/v1/insights/changes"]["get"].is_object());
    }
}

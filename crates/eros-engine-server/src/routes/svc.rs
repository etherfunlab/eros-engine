// SPDX-License-Identifier: AGPL-3.0-only
//! Service-to-service routes (`/svc/v1/*`).
//!
//! A third auth class next to the public and bearer-JWT sub-trees: admitted by
//! a shared secret header, never by a user token. The class is split into three
//! sub-trees, each behind its own secret so a consumer holds only the
//! credential for the streams it reads:
//!   * insights: `insights/changes`, `memories/changes`, `insights/deletions`
//!     (`x-insights-export-secret`)
//!   * facts: `facts/changes`, `facts/deletions` (`x-facts-export-secret`)
//!   * user erase: `users/{user_id}/erase` (`x-user-erase-secret`)
//!
//! A sub-tree is left unmounted when the deployment did not configure its
//! secret (see `routes::router`), so an unconfigured deployment answers on
//! those paths exactly as it does for any unknown path.
//!
//! The erasure stream rides both read secrets: a mirror of profiles and
//! memories and a mirror of facts each have to learn which users to drop, and
//! neither is given the other's credential to do it.

use std::collections::BTreeMap;

use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_store::erasure::ErasureRepo;
use eros_engine_store::human_fact::{HumanFactRepo, HumanFactRow};
use eros_engine_store::human_insight::{HumanInsightRepo, HumanInsightsRow};
use eros_engine_store::memory::{MemoryRepo, ProfileMemoryChange};

use crate::byok::constant_time_eq;
use crate::error::AppError;
use crate::state::AppState;

pub(crate) const EXPORT_SECRET_HEADER: &str = "x-insights-export-secret";
pub(crate) const FACTS_SECRET_HEADER: &str = "x-facts-export-secret";
pub(crate) const ERASE_SECRET_HEADER: &str = "x-user-erase-secret";
const DEFAULT_LIMIT: i64 = 500;
const MAX_LIMIT: i64 = 1000;

/// One `human_insights` row as exported: every profile column except the
/// sensitive three (`kinks`, `ideology`, `mental_health`), which this export
/// never carries.
#[derive(Serialize, ToSchema)]
pub struct InsightExportRow {
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
    pub updated_at: DateTime<Utc>,
}

impl From<HumanInsightsRow> for InsightExportRow {
    fn from(r: HumanInsightsRow) -> Self {
        Self {
            user_id: r.user_id,
            occupation: r.occupation,
            mbti_guess: r.mbti_guess,
            love_values: r.love_values,
            emotional_needs: r.emotional_needs,
            life_rhythm: r.life_rhythm,
            interests: r.interests,
            personality_traits: r.personality_traits,
            deal_breakers: r.deal_breakers,
            education: r.education,
            education_level: r.education_level,
            education_status: r.education_status,
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

/// 403 unless `header` carries `secret`. A sub-tree is only mounted when its
/// secret is set, so `None` here is unreachable in practice; it still fails
/// closed. `what` names the sub-tree in the error text.
fn admit_with(
    headers: &HeaderMap,
    secret: Option<&str>,
    header: &str,
    what: &str,
) -> Result<(), AppError> {
    let Some(secret) = secret else {
        return Err(AppError::Forbidden(format!("{what} is not enabled")));
    };
    let sent = headers
        .get(header)
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    if constant_time_eq(sent, secret.as_bytes()) {
        Ok(())
    } else {
        Err(AppError::Forbidden(format!(
            "{what} secret missing or wrong"
        )))
    }
}

fn admit(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    admit_with(
        headers,
        state.config.insights_export.secret.as_deref(),
        EXPORT_SECRET_HEADER,
        "insights export",
    )
}

fn admit_facts(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    admit_with(
        headers,
        state.config.facts_export.secret.as_deref(),
        FACTS_SECRET_HEADER,
        "facts export",
    )
}

fn admit_erase(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    admit_with(
        headers,
        state.config.user_erase.secret.as_deref(),
        ERASE_SECRET_HEADER,
        "user erase",
    )
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

/// One extracted profile-layer memory as exported. No embedding.
#[derive(Serialize, ToSchema)]
pub struct MemoryExportRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub content: String,
    pub category: String,
    /// The extraction metadata, verbatim; `null` when the extraction wrote none.
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

impl From<ProfileMemoryChange> for MemoryExportRow {
    fn from(r: ProfileMemoryChange) -> Self {
        Self {
            id: r.id,
            user_id: r.user_id,
            content: r.content,
            category: r.category,
            metadata: r.metadata,
            created_at: r.created_at,
        }
    }
}

/// Keyset position. Pass both fields back as `since` + `after_id`.
#[derive(Serialize, ToSchema)]
pub struct MemoryChangesCursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct MemoryChangesResponse {
    pub memories: Vec<MemoryExportRow>,
    /// Present iff the page is full; `null` means the walk is complete.
    pub next: Option<MemoryChangesCursor>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MemoryChangesQuery {
    /// RFC 3339. Default: Unix epoch (walk everything).
    pub since: Option<DateTime<Utc>>,
    /// Tie-break within `since`. Default: nil UUID. Pass `next.id`.
    pub after_id: Option<Uuid>,
    /// Default 500, clamped to 1..=1000.
    pub limit: Option<i64>,
}

/// Extracted profile-layer memories created since a keyset cursor, for a
/// downstream service that builds on the facts the engine extracted about a
/// user. Raw-turn rows and relationship-layer rows are not exported. The rows
/// are insert-only; the export carries no deletions.
#[utoipa::path(
    get,
    path = "/svc/v1/memories/changes",
    tag = "svc",
    params(MemoryChangesQuery),
    responses(
        (status = 200, body = MemoryChangesResponse),
        (status = 403, description = "x-insights-export-secret missing or wrong")
    ),
    security(("insights_export_secret" = []))
)]
async fn memories_changes(
    State(state): State<AppState>,
    headers: HeaderMap,
    q: Result<Query<MemoryChangesQuery>, QueryRejection>,
) -> Result<Json<MemoryChangesResponse>, AppError> {
    // As in `insights_changes`: admission before the query error, no DB access
    // before `admit`.
    admit(&state, &headers)?;
    let Query(q) = q.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let since = q.since.unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let after_id = q.after_id.unwrap_or(Uuid::nil());
    let limit = clamp_limit(q.limit);

    let rows = MemoryRepo { pool: &state.pool }
        .list_profile_changes(since, after_id, limit)
        .await?;
    let next = if rows.len() as i64 == limit {
        rows.last().map(|r| MemoryChangesCursor {
            created_at: r.created_at,
            id: r.id,
        })
    } else {
        None
    };
    Ok(Json(MemoryChangesResponse {
        memories: rows.into_iter().map(MemoryExportRow::from).collect(),
        next,
    }))
}

/// One user's erasure tombstone as exported.
#[derive(Serialize, ToSchema)]
pub struct ErasureExportRow {
    pub user_id: Uuid,
    pub erased_at: DateTime<Utc>,
}

/// Keyset position. Pass both fields back as `since` + `after_user`.
#[derive(Serialize, ToSchema)]
pub struct ErasureCursor {
    pub erased_at: DateTime<Utc>,
    pub user_id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct ErasuresResponse {
    pub erasures: Vec<ErasureExportRow>,
    /// Present iff the page is full; `null` means the walk is complete.
    pub next: Option<ErasureCursor>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ErasuresQuery {
    /// RFC 3339. Default: Unix epoch (walk everything).
    pub since: Option<DateTime<Utc>>,
    /// Tie-break within `since`. Default: nil UUID. Pass `next.user_id`.
    pub after_user: Option<Uuid>,
    /// Default 500, clamped to 1..=1000.
    pub limit: Option<i64>,
}

/// One page of the erasure stream. Callers admit before they get here.
async fn erasures_page(
    state: &AppState,
    q: Result<Query<ErasuresQuery>, QueryRejection>,
) -> Result<Json<ErasuresResponse>, AppError> {
    let Query(q) = q.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let since = q.since.unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let after_user = q.after_user.unwrap_or(Uuid::nil());
    let limit = clamp_limit(q.limit);

    let rows = ErasureRepo { pool: &state.pool }
        .list_changes(since, after_user, limit)
        .await?;
    let next = if rows.len() as i64 == limit {
        rows.last().map(|r| ErasureCursor {
            erased_at: r.erased_at,
            user_id: r.user_id,
        })
    } else {
        None
    };
    Ok(Json(ErasuresResponse {
        erasures: rows
            .into_iter()
            .map(|r| ErasureExportRow {
                user_id: r.user_id,
                erased_at: r.erased_at,
            })
            .collect(),
        next,
    }))
}

/// Users whose data the engine erased since a keyset cursor, for the mirror of
/// profiles and memories to drop in turn. The same stream as
/// `/svc/v1/facts/deletions`, readable with the insights secret so that mirror
/// needs no second credential.
#[utoipa::path(
    get,
    path = "/svc/v1/insights/deletions",
    tag = "svc",
    params(ErasuresQuery),
    responses(
        (status = 200, body = ErasuresResponse),
        (status = 403, description = "x-insights-export-secret missing or wrong")
    ),
    security(("insights_export_secret" = []))
)]
async fn insights_deletions(
    State(state): State<AppState>,
    headers: HeaderMap,
    q: Result<Query<ErasuresQuery>, QueryRejection>,
) -> Result<Json<ErasuresResponse>, AppError> {
    // Admission before the query error, no DB access before `admit`.
    admit(&state, &headers)?;
    erasures_page(&state, q).await
}

/// One `human_facts` row as exported. Source pointers (session, message, run)
/// stay inside the engine; `recurrence` is the derived tier.
#[derive(Serialize, ToSchema)]
pub struct FactExportRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub content: String,
    pub category: String,
    pub domain: String,
    pub evidence_type: String,
    pub temporality: String,
    pub persistence: String,
    pub confidence: String,
    pub occurrences: i32,
    pub session_count: i32,
    /// `once`, `repeated_in_session` or `repeated_across_sessions`.
    pub recurrence: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

impl From<HumanFactRow> for FactExportRow {
    fn from(r: HumanFactRow) -> Self {
        let recurrence = r.recurrence().as_str().to_string();
        Self {
            id: r.id,
            user_id: r.user_id,
            content: r.content,
            category: r.category,
            domain: r.domain,
            evidence_type: r.evidence_type,
            temporality: r.temporality,
            persistence: r.persistence,
            confidence: r.confidence,
            occurrences: r.occurrences,
            session_count: r.session_count,
            recurrence,
            first_seen_at: r.first_seen_at,
            last_seen_at: r.last_seen_at,
        }
    }
}

/// Keyset position. Pass both fields back as `since` + `after_id`.
#[derive(Serialize, ToSchema)]
pub struct FactChangesCursor {
    pub last_seen_at: DateTime<Utc>,
    pub id: Uuid,
}

#[derive(Serialize, ToSchema)]
pub struct FactChangesResponse {
    pub facts: Vec<FactExportRow>,
    /// Present iff the page is full; `null` means the walk is complete.
    pub next: Option<FactChangesCursor>,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FactChangesQuery {
    /// RFC 3339. Default: Unix epoch (walk everything).
    pub since: Option<DateTime<Utc>>,
    /// Tie-break within `since`. Default: nil UUID. Pass `next.id`.
    pub after_id: Option<Uuid>,
    /// Default 500, clamped to 1..=1000.
    pub limit: Option<i64>,
}

/// Ledger facts first seen or seen again since a keyset cursor, for a
/// downstream service that mirrors what was mined about each user. A repeat
/// moves `last_seen_at`, so a row reappears in the stream each time its
/// counters change. Rows leave the ledger only through erasure; see
/// `/svc/v1/facts/deletions`.
#[utoipa::path(
    get,
    path = "/svc/v1/facts/changes",
    tag = "svc",
    params(FactChangesQuery),
    responses(
        (status = 200, body = FactChangesResponse),
        (status = 403, description = "x-facts-export-secret missing or wrong")
    ),
    security(("facts_export_secret" = []))
)]
async fn facts_changes(
    State(state): State<AppState>,
    headers: HeaderMap,
    q: Result<Query<FactChangesQuery>, QueryRejection>,
) -> Result<Json<FactChangesResponse>, AppError> {
    // As in `insights_changes`: admission before the query error, no DB access
    // before `admit_facts`.
    admit_facts(&state, &headers)?;
    let Query(q) = q.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let since = q.since.unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
    let after_id = q.after_id.unwrap_or(Uuid::nil());
    let limit = clamp_limit(q.limit);

    let rows = HumanFactRepo { pool: &state.pool }
        .list_changes(since, after_id, limit)
        .await?;
    let next = if rows.len() as i64 == limit {
        rows.last().map(|r| FactChangesCursor {
            last_seen_at: r.last_seen_at,
            id: r.id,
        })
    } else {
        None
    };
    Ok(Json(FactChangesResponse {
        facts: rows.into_iter().map(FactExportRow::from).collect(),
        next,
    }))
}

/// Users whose data the engine erased since a keyset cursor, for the facts
/// mirror to drop in turn. The same stream as `/svc/v1/insights/deletions`,
/// readable with the facts secret so that mirror needs no second credential.
#[utoipa::path(
    get,
    path = "/svc/v1/facts/deletions",
    tag = "svc",
    params(ErasuresQuery),
    responses(
        (status = 200, body = ErasuresResponse),
        (status = 403, description = "x-facts-export-secret missing or wrong")
    ),
    security(("facts_export_secret" = []))
)]
async fn facts_deletions(
    State(state): State<AppState>,
    headers: HeaderMap,
    q: Result<Query<ErasuresQuery>, QueryRejection>,
) -> Result<Json<ErasuresResponse>, AppError> {
    // Admission before the query error, no DB access before `admit_facts`.
    admit_facts(&state, &headers)?;
    erasures_page(&state, q).await
}

/// What one erase call removed, by table. Every covered table is listed, with 0
/// when it had nothing to do.
#[derive(Serialize, ToSchema)]
pub struct EraseResponse {
    pub user_id: Uuid,
    /// The tombstone's timestamp. A repeat call moves it forward.
    pub erased_at: DateTime<Utc>,
    /// Rows removed, by table.
    pub deleted: BTreeMap<String, i64>,
    /// Rows kept with their content cleared, by table. Counts only rows that
    /// still carried content, so a repeat reports 0.
    pub blanked: BTreeMap<String, i64>,
}

/// Erase everything the engine derived about a user, and list the user on the
/// deletion streams.
///
/// For a deployment to call when the user deletes their account. Derived rows
/// (facts, profile, memories, world and story content, and both sides'
/// per-relationship insights) are deleted. Audit and event rows keep their ids
/// and timestamps but lose their payloads and free text; relationship state
/// keeps its numbers and loses its generated summary. Chat sessions and
/// messages belong to the caller and are not touched.
///
/// Idempotent: a repeat finds nothing left to erase, reports 0 everywhere and
/// re-stamps the tombstone.
#[utoipa::path(
    post,
    path = "/svc/v1/users/{user_id}/erase",
    tag = "svc",
    params(("user_id" = Uuid, Path, description = "The user to erase")),
    responses(
        (status = 200, body = EraseResponse),
        (status = 403, description = "x-user-erase-secret missing or wrong")
    ),
    security(("user_erase_secret" = []))
)]
async fn erase_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    user_id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<EraseResponse>, AppError> {
    // As in the read routes: admission before the path error, no DB access
    // before `admit_erase`.
    admit_erase(&state, &headers)?;
    let Path(user_id) = user_id.map_err(|e| AppError::BadRequest(e.body_text()))?;

    let report = ErasureRepo { pool: &state.pool }
        .erase_user(user_id)
        .await?;
    let named = |m: BTreeMap<&'static str, i64>| -> BTreeMap<String, i64> {
        m.into_iter().map(|(t, n)| (t.to_string(), n)).collect()
    };
    Ok(Json(EraseResponse {
        user_id,
        erased_at: report.erased_at,
        deleted: named(report.deleted),
        blanked: named(report.blanked),
    }))
}

/// Insights sub-tree: mounted under `INSIGHTS_EXPORT_SECRET`.
pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(insights_changes))
        .routes(routes!(memories_changes))
        .routes(routes!(insights_deletions))
}

/// Facts sub-tree: mounted under `FACTS_EXPORT_SECRET`.
pub fn facts_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(facts_changes))
        .routes(routes!(facts_deletions))
}

/// User-erase sub-tree: mounted under `USER_ERASE_SECRET`.
pub fn erase_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(erase_user))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use chrono::{TimeZone, Utc};
    use tower::ServiceExt;
    use uuid::Uuid;

    use eros_engine_store::human_fact::{FactInput, FactSource, HumanFactRepo};

    use super::{clamp_limit, ERASE_SECRET_HEADER, EXPORT_SECRET_HEADER, FACTS_SECRET_HEADER};
    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::build_router;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";
    const FACTS_SECRET: &str = "fedcba9876543210fedcba9876543210";
    const ERASE_SECRET: &str = "abcdef0123456789abcdef0123456789";

    fn app_with_secret(pool: sqlx::PgPool, secret: Option<&str>) -> Router {
        app_with_secrets(pool, secret, None, None)
    }

    /// The three sub-trees are configured independently.
    fn app_with_secrets(
        pool: sqlx::PgPool,
        insights: Option<&str>,
        facts: Option<&str>,
        erase: Option<&str>,
    ) -> Router {
        let mut state = test_state(pool);
        state.config.insights_export.secret = insights.map(String::from);
        state.config.facts_export.secret = facts.map(String::from);
        state.config.user_erase.secret = erase.map(String::from);
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
        get_with(app, uri, EXPORT_SECRET_HEADER, secret).await
    }

    async fn get_with(
        app: Router,
        uri: &str,
        header: &str,
        secret: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        send_with(app, Request::get(uri), header, secret).await
    }

    async fn post_with(
        app: Router,
        uri: &str,
        header: &str,
        secret: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        send_with(app, Request::post(uri), header, secret).await
    }

    async fn send_with(
        app: Router,
        mut req: axum::http::request::Builder,
        header: &str,
        secret: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        if let Some(s) = secret {
            req = req.header(header, s);
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
            occupation: Some("occupation".into()),
            mbti_guess: Some("mbti_guess".into()),
            love_values: Some("love_values".into()),
            emotional_needs: Some("emotional_needs".into()),
            life_rhythm: Some("life_rhythm".into()),
            interests: vec!["interests_0".into(), "interests_1".into()],
            personality_traits: vec!["personality_traits_0".into()],
            deal_breakers: vec!["deal_breakers_0".into(), "deal_breakers_1".into()],
            education: Some("education".into()),
            education_level: Some("education_level".into()),
            education_status: Some("education_status".into()),
            family: Some("family".into()),
            relationship_history: Some("relationship_history".into()),
            social_pattern: Some("social_pattern".into()),
            future_plans: Some("future_plans".into()),
            finance_status: Some("finance_status".into()),
            kinks: vec!["kinks_0".into()],
            ideology: Some("ideology".into()),
            mental_health: Some("mental_health".into()),
            updated_at,
        };

        let out = InsightExportRow::from(row);
        assert_eq!(out.user_id, user_id);
        assert_eq!(out.occupation.as_deref(), Some("occupation"));
        assert_eq!(out.mbti_guess.as_deref(), Some("mbti_guess"));
        assert_eq!(out.love_values.as_deref(), Some("love_values"));
        assert_eq!(out.emotional_needs.as_deref(), Some("emotional_needs"));
        assert_eq!(out.life_rhythm.as_deref(), Some("life_rhythm"));
        assert_eq!(out.interests, vec!["interests_0", "interests_1"]);
        assert_eq!(out.personality_traits, vec!["personality_traits_0"]);
        assert_eq!(
            out.deal_breakers,
            vec!["deal_breakers_0", "deal_breakers_1"]
        );
        assert_eq!(out.education.as_deref(), Some("education"));
        assert_eq!(out.education_level.as_deref(), Some("education_level"));
        assert_eq!(out.education_status.as_deref(), Some("education_status"));
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

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn exported_row_has_education_level_but_not_sensitive_or_retired_columns(
        pool: sqlx::PgPool,
    ) {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO engine.human_insights \
               (user_id, occupation, education_level, education_status, \
                kinks, ideology, mental_health, city, age_min) \
             VALUES ($1, '设计师', 'bachelor', 'enrolled', '{\"k\"}', 'liberal', 'anxious', \
                     '上海', 20)",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) = get(
            app_with_secret(pool, Some(SECRET)),
            "/svc/v1/insights/changes",
            Some(SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let row = body["profiles"][0].as_object().expect("one exported row");
        assert_eq!(row["education_level"], "bachelor");
        assert_eq!(row["education_status"], "enrolled");
        assert_eq!(row["occupation"], "设计师");
        for key in [
            "kinks",
            "ideology",
            "mental_health",
            "city",
            "location",
            "hometown",
            "nationality",
            "preferred_gender",
            "age_min",
            "age_max",
        ] {
            assert!(!row.contains_key(key), "{key} must not be exported");
        }
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

    async fn seed_memory(
        pool: &sqlx::PgPool,
        user: Uuid,
        instance: Option<Uuid>,
        category: Option<&str>,
        at: chrono::DateTime<Utc>,
    ) -> Uuid {
        let session = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user)
        .bind(instance)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query_scalar(
            "INSERT INTO engine.companion_memories \
               (session_id, user_id, instance_id, content, embedding, category, metadata, created_at) \
             VALUES ($1, $2, $3, '用户喜欢爵士乐', array_fill(0::real, ARRAY[512])::vector, \
                     $4, $5, $6) \
             RETURNING id",
        )
        .bind(session)
        .bind(user)
        .bind(instance)
        .bind(category)
        .bind(category.map(|_| serde_json::json!({"confidence": "high"})))
        .bind(at)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn memories_export_only_extracted_profile_rows(pool: sqlx::PgPool) {
        use crate::routes::companion::testutil::seed_persona_instance;

        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let user = Uuid::new_v4();
        let instance = seed_persona_instance(&pool, user).await;
        let extracted = seed_memory(&pool, user, None, Some("preference"), t).await;
        seed_memory(&pool, user, None, None, t).await; // a raw turn
        seed_memory(&pool, user, Some(instance), Some("preference"), t).await; // relationship

        let (status, body) = get(
            app_with_secret(pool, Some(SECRET)),
            "/svc/v1/memories/changes",
            Some(SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            serde_json::json!({
                "memories": [{
                    "id": extracted,
                    "user_id": user,
                    "content": "用户喜欢爵士乐",
                    "category": "preference",
                    "metadata": {"confidence": "high"},
                    "created_at": "2026-10-13T12:00:00Z",
                }],
                "next": null,
            })
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn memories_full_page_carries_next_and_short_page_does_not(pool: sqlx::PgPool) {
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let user = Uuid::new_v4();
        let mut ids = [
            seed_memory(&pool, user, None, Some("fact"), t).await,
            seed_memory(&pool, user, None, Some("fact"), t).await,
            seed_memory(&pool, user, None, Some("fact"), t).await,
        ];
        ids.sort();

        let (status, p1) = get(
            app_with_secret(pool.clone(), Some(SECRET)),
            "/svc/v1/memories/changes?limit=2",
            Some(SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(p1["memories"].as_array().unwrap().len(), 2);
        assert!(p1["next"].is_object(), "full page must carry next");

        let next_ts = p1["next"]["created_at"].as_str().unwrap();
        let next_id = p1["next"]["id"].as_str().unwrap();
        let (status, p2) = get(
            app_with_secret(pool, Some(SECRET)),
            &format!("/svc/v1/memories/changes?limit=2&since={next_ts}&after_id={next_id}"),
            Some(SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(p2["memories"].as_array().unwrap().len(), 1);
        assert!(p2["next"].is_null(), "short page must not carry next");

        let walked: Vec<String> = p1["memories"]
            .as_array()
            .unwrap()
            .iter()
            .chain(p2["memories"].as_array().unwrap())
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        let expected: Vec<String> = ids.iter().map(|u| u.to_string()).collect();
        assert_eq!(walked, expected);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn memories_export_admits_like_the_insights_export(pool: sqlx::PgPool) {
        for secret in [None, Some("wrong")] {
            let (status, body) = get(
                app_with_secret(pool.clone(), Some(SECRET)),
                "/svc/v1/memories/changes?limit=abc",
                secret,
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "secret={secret:?}");
            assert_eq!(body["error"], "forbidden");
        }
        let (status, body) = get(
            app_with_secret(pool.clone(), Some(SECRET)),
            "/svc/v1/memories/changes?limit=abc",
            Some(SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "bad_request");

        let (unmounted, _) = get(
            app_with_secret(pool.clone(), None),
            "/svc/v1/memories/changes",
            Some(SECRET),
        )
        .await;
        let (unknown, _) = get(
            app_with_secret(pool, None),
            "/svc-does-not-exist/zzz",
            Some(SECRET),
        )
        .await;
        assert_eq!(unmounted, unknown);
    }

    #[test]
    fn openapi_lists_the_memories_export_route() {
        let api = crate::routes::openapi_for_tests();
        assert!(api["paths"]["/svc/v1/memories/changes"]["get"].is_object());
    }

    fn fact(content: &str) -> FactInput {
        FactInput {
            content: content.into(),
            category: "preference".into(),
            domain: "hobby".into(),
            evidence_type: "explicit_statement".into(),
            temporality: "current".into(),
            persistence: "stable".into(),
            confidence: "high".into(),
        }
    }

    fn seen() -> FactSource {
        FactSource {
            session_id: None,
            message_id: None,
            run_id: None,
        }
    }

    /// Ledger stamps are the database clock at the write; the export tests need
    /// known instants, so they set the stamps by hand after seeding.
    async fn stamp_seen(
        pool: &sqlx::PgPool,
        user: Uuid,
        content: &str,
        first: chrono::DateTime<Utc>,
        last: chrono::DateTime<Utc>,
    ) {
        sqlx::query(
            "UPDATE engine.human_facts SET first_seen_at = $3, last_seen_at = $4 \
             WHERE user_id = $1 AND content = $2",
        )
        .bind(user)
        .bind(content)
        .bind(first)
        .bind(last)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn seed_erasure(pool: &sqlx::PgPool, at: chrono::DateTime<Utc>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO engine.user_erasures (user_id, erased_at) VALUES ($1, $2)")
            .bind(id)
            .bind(at)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn facts_subtree_unmounted_without_its_secret_even_when_insights_secret_set(
        pool: sqlx::PgPool,
    ) {
        let app = || app_with_secrets(pool.clone(), Some(SECRET), None, None);
        let (unknown, _) = get_with(
            app(),
            "/svc-does-not-exist/zzz",
            FACTS_SECRET_HEADER,
            Some(FACTS_SECRET),
        )
        .await;
        for uri in ["/svc/v1/facts/changes", "/svc/v1/facts/deletions"] {
            for (header, secret) in [
                (FACTS_SECRET_HEADER, FACTS_SECRET),
                (EXPORT_SECRET_HEADER, SECRET),
            ] {
                let (status, _) = get_with(app(), uri, header, Some(secret)).await;
                assert_eq!(status, unknown, "{uri} must look like an unknown path");
            }
        }
        let (status, _) = get_with(
            app(),
            "/svc/v1/insights/changes",
            EXPORT_SECRET_HEADER,
            Some(SECRET),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the insights sub-tree stays mounted"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn insights_secret_does_not_admit_facts_routes(pool: sqlx::PgPool) {
        let app = || app_with_secrets(pool.clone(), Some(SECRET), Some(FACTS_SECRET), None);
        for uri in ["/svc/v1/facts/changes", "/svc/v1/facts/deletions"] {
            // The insights secret under either header name.
            for header in [EXPORT_SECRET_HEADER, FACTS_SECRET_HEADER] {
                let (status, body) = get_with(app(), uri, header, Some(SECRET)).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{uri} via {header}");
                assert_eq!(body["error"], "forbidden");
            }
            let (status, _) = get_with(app(), uri, FACTS_SECRET_HEADER, None).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri} without a header");
            let (status, _) = get_with(app(), uri, FACTS_SECRET_HEADER, Some(FACTS_SECRET)).await;
            assert_eq!(status, StatusCode::OK, "{uri} with its own secret");
        }
        // And the other way round: the facts secret opens nothing in the
        // insights sub-tree.
        for uri in [
            "/svc/v1/insights/changes",
            "/svc/v1/memories/changes",
            "/svc/v1/insights/deletions",
        ] {
            let (status, _) = get_with(app(), uri, EXPORT_SECRET_HEADER, Some(FACTS_SECRET)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn erase_secret_mounts_nothing_in_the_other_sub_trees(pool: sqlx::PgPool) {
        let app = || app_with_secrets(pool.clone(), None, None, Some(ERASE_SECRET));
        let (unknown, _) = get_with(
            app(),
            "/svc-does-not-exist/zzz",
            ERASE_SECRET_HEADER,
            Some(ERASE_SECRET),
        )
        .await;
        for uri in [
            "/svc/v1/insights/changes",
            "/svc/v1/insights/deletions",
            "/svc/v1/facts/changes",
            "/svc/v1/facts/deletions",
        ] {
            let (status, _) = get_with(app(), uri, ERASE_SECRET_HEADER, Some(ERASE_SECRET)).await;
            assert_eq!(status, unknown, "{uri}");
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn facts_changes_pages_by_last_seen_then_id_and_exposes_recurrence(pool: sqlx::PgPool) {
        let t0 = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap();
        let user = Uuid::new_v4();
        let repo = HumanFactRepo { pool: &pool };
        // Three rows tie on t0; the third is seen again at t1 and sorts last.
        repo.upsert_run(
            user,
            &[
                fact("用户喜欢爬山"),
                fact("用户在深圳工作"),
                fact("用户有一个妹妹"),
            ],
            &seen(),
        )
        .await
        .unwrap();
        for content in ["用户喜欢爬山", "用户在深圳工作", "用户有一个妹妹"] {
            stamp_seen(&pool, user, content, t0, t0).await;
        }
        repo.upsert_run(user, &[fact("用户有一个妹妹")], &seen())
            .await
            .unwrap();
        stamp_seen(&pool, user, "用户有一个妹妹", t0, t1).await;
        let mut tied: Vec<Uuid> = repo
            .list_for_user(user)
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.last_seen_at == t0)
            .map(|r| r.id)
            .collect();
        tied.sort();
        assert_eq!(tied.len(), 2);

        let app = || app_with_secrets(pool.clone(), None, Some(FACTS_SECRET), None);
        let (status, p1) = get_with(
            app(),
            "/svc/v1/facts/changes?limit=2",
            FACTS_SECRET_HEADER,
            Some(FACTS_SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rows = p1["facts"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            tied.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
            "ties on last_seen_at break by id"
        );
        assert!(p1["next"].is_object(), "full page must carry next");
        assert_eq!(p1["next"]["id"], rows[1]["id"]);
        assert_eq!(p1["next"]["last_seen_at"], rows[1]["last_seen_at"]);

        let next_ts = p1["next"]["last_seen_at"].as_str().unwrap();
        let next_id = p1["next"]["id"].as_str().unwrap();
        let (status, p2) = get_with(
            app(),
            &format!("/svc/v1/facts/changes?limit=2&since={next_ts}&after_id={next_id}"),
            FACTS_SECRET_HEADER,
            Some(FACTS_SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rows2 = p2["facts"].as_array().unwrap();
        assert_eq!(rows2.len(), 1);
        assert!(p2["next"].is_null(), "short page must not carry next");
        assert_eq!(rows2[0]["content"], "用户有一个妹妹");
        assert_eq!(rows2[0]["recurrence"], "repeated_in_session");
        assert_eq!(rows2[0]["occurrences"], 2);
        assert_eq!(rows2[0]["session_count"], 1);

        // A once-seen row, and the exact wire shape: no session / message /
        // run pointers.
        let once = &rows[0];
        assert_eq!(once["recurrence"], "once");
        assert_eq!(once["user_id"], user.to_string());
        assert_eq!(once["category"], "preference");
        assert_eq!(once["domain"], "hobby");
        assert_eq!(once["evidence_type"], "explicit_statement");
        assert_eq!(once["temporality"], "current");
        assert_eq!(once["persistence"], "stable");
        assert_eq!(once["confidence"], "high");
        assert_eq!(once["first_seen_at"], "2026-10-13T12:00:00Z");
        assert_eq!(once["last_seen_at"], "2026-10-13T12:00:00Z");
        let mut keys: Vec<&str> = once
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "category",
                "confidence",
                "content",
                "domain",
                "evidence_type",
                "first_seen_at",
                "id",
                "last_seen_at",
                "occurrences",
                "persistence",
                "recurrence",
                "session_count",
                "temporality",
                "user_id",
            ]
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn insights_deletions_and_facts_deletions_page_user_erasures(pool: sqlx::PgPool) {
        let t0 = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 10, 14, 12, 0, 0).unwrap();
        let first = seed_erasure(&pool, t0).await;
        let second = seed_erasure(&pool, t1).await;

        let app = || app_with_secrets(pool.clone(), Some(SECRET), Some(FACTS_SECRET), None);
        for (uri, header, secret) in [
            ("/svc/v1/insights/deletions", EXPORT_SECRET_HEADER, SECRET),
            ("/svc/v1/facts/deletions", FACTS_SECRET_HEADER, FACTS_SECRET),
        ] {
            let (status, body) = get_with(app(), uri, header, Some(secret)).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(
                body,
                serde_json::json!({
                    "erasures": [
                        {"user_id": first, "erased_at": "2026-10-13T12:00:00Z"},
                        {"user_id": second, "erased_at": "2026-10-14T12:00:00Z"},
                    ],
                    "next": null,
                }),
                "{uri}"
            );

            let (_, p1) = get_with(app(), &format!("{uri}?limit=1"), header, Some(secret)).await;
            assert_eq!(p1["erasures"].as_array().unwrap().len(), 1);
            assert_eq!(
                p1["next"],
                serde_json::json!({"erased_at": "2026-10-13T12:00:00Z", "user_id": first})
            );
            let next_ts = p1["next"]["erased_at"].as_str().unwrap();
            let next_user = p1["next"]["user_id"].as_str().unwrap();
            let (_, p2) = get_with(
                app(),
                &format!("{uri}?limit=1&since={next_ts}&after_user={next_user}"),
                header,
                Some(secret),
            )
            .await;
            assert_eq!(
                p2["erasures"],
                serde_json::json!([{"user_id": second, "erased_at": "2026-10-14T12:00:00Z"}])
            );
            // limit == remaining rows: a full page still carries next.
            assert!(p2["next"].is_object());
        }

        // Each stream is closed to the other's secret.
        let (status, _) = get_with(
            app(),
            "/svc/v1/insights/deletions",
            EXPORT_SECRET_HEADER,
            Some(FACTS_SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = get_with(
            app(),
            "/svc/v1/facts/deletions",
            FACTS_SECRET_HEADER,
            Some(SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn facts_and_deletions_routes_admit_before_query_errors(pool: sqlx::PgPool) {
        let app = || app_with_secrets(pool.clone(), Some(SECRET), Some(FACTS_SECRET), None);
        for (uri, header, secret) in [
            (
                "/svc/v1/facts/changes?limit=abc",
                FACTS_SECRET_HEADER,
                FACTS_SECRET,
            ),
            (
                "/svc/v1/facts/deletions?limit=abc",
                FACTS_SECRET_HEADER,
                FACTS_SECRET,
            ),
            (
                "/svc/v1/insights/deletions?limit=abc",
                EXPORT_SECRET_HEADER,
                SECRET,
            ),
        ] {
            for sent in [None, Some("wrong")] {
                let (status, body) = get_with(app(), uri, header, sent).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{uri} sent={sent:?}");
                assert_eq!(body["error"], "forbidden");
            }
            let (status, body) = get_with(app(), uri, header, Some(secret)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(body["error"], "bad_request");
        }
    }

    async fn tombstone_count(pool: &sqlx::PgPool, user: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM engine.user_erasures WHERE user_id = $1")
            .bind(user)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn erase_requires_its_own_secret_and_is_absent_without_it(pool: sqlx::PgPool) {
        let user = Uuid::new_v4();
        let uri = format!("/svc/v1/users/{user}/erase");

        // Unmounted without USER_ERASE_SECRET, whatever the other secrets are.
        let app = || app_with_secrets(pool.clone(), Some(SECRET), Some(FACTS_SECRET), None);
        let (unknown, _) = post_with(
            app(),
            "/svc-does-not-exist/zzz",
            ERASE_SECRET_HEADER,
            Some(ERASE_SECRET),
        )
        .await;
        for (header, secret) in [
            (ERASE_SECRET_HEADER, ERASE_SECRET),
            (ERASE_SECRET_HEADER, SECRET),
            (EXPORT_SECRET_HEADER, SECRET),
            (FACTS_SECRET_HEADER, FACTS_SECRET),
        ] {
            let (status, _) = post_with(app(), &uri, header, Some(secret)).await;
            assert_eq!(status, unknown, "{header} must look like an unknown path");
        }

        // Mounted: only its own secret, only under its own header.
        let app = || {
            app_with_secrets(
                pool.clone(),
                Some(SECRET),
                Some(FACTS_SECRET),
                Some(ERASE_SECRET),
            )
        };
        for (header, sent) in [
            (ERASE_SECRET_HEADER, None),
            (ERASE_SECRET_HEADER, Some("wrong")),
            (ERASE_SECRET_HEADER, Some(SECRET)),
            (ERASE_SECRET_HEADER, Some(FACTS_SECRET)),
            (EXPORT_SECRET_HEADER, Some(ERASE_SECRET)),
            (FACTS_SECRET_HEADER, Some(ERASE_SECRET)),
        ] {
            let (status, body) = post_with(app(), &uri, header, sent).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{header} sent={sent:?}");
            assert_eq!(body["error"], "forbidden");
        }
        assert_eq!(
            tombstone_count(&pool, user).await,
            0,
            "a refused call erases nothing"
        );
        // The erase secret opens nothing in the other sub-trees.
        let (status, _) = get_with(
            app(),
            "/svc/v1/insights/changes",
            EXPORT_SECRET_HEADER,
            Some(ERASE_SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Right secret: the user's rows go and the report says how many.
        HumanFactRepo { pool: &pool }
            .upsert_run(user, &[fact("用户喜欢爬山")], &seen())
            .await
            .unwrap();
        let (status, body) = post_with(app(), &uri, ERASE_SECRET_HEADER, Some(ERASE_SECRET)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user_id"], user.to_string());
        assert_eq!(body["deleted"]["human_facts"], 1);
        assert_eq!(body["deleted"]["companion_memories"], 0);
        assert_eq!(body["blanked"]["companion_insights_events"], 0);
        let erased_at = body["erased_at"].as_str().unwrap().to_string();
        assert!(HumanFactRepo { pool: &pool }
            .list_for_user(user)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(tombstone_count(&pool, user).await, 1);

        // A repeat is a 200 with nothing left to count, and moves the tombstone.
        let (status, again) = post_with(app(), &uri, ERASE_SECRET_HEADER, Some(ERASE_SECRET)).await;
        assert_eq!(status, StatusCode::OK);
        for map in ["deleted", "blanked"] {
            assert!(
                again[map]
                    .as_object()
                    .unwrap()
                    .values()
                    .all(|n| n.as_i64() == Some(0)),
                "{map}: {again}"
            );
        }
        assert_eq!(again["deleted"].as_object().unwrap().len(), 17);
        assert_eq!(again["blanked"].as_object().unwrap().len(), 8);
        let parse = |v: &str| chrono::DateTime::parse_from_rfc3339(v).unwrap();
        assert!(
            parse(again["erased_at"].as_str().unwrap()) > parse(&erased_at),
            "a repeat moves the tombstone forward"
        );
        assert_eq!(tombstone_count(&pool, user).await, 1);

        // A user the engine never saw is still a 200 with a tombstone.
        let stranger = Uuid::new_v4();
        let (status, body) = post_with(
            app(),
            &format!("/svc/v1/users/{stranger}/erase"),
            ERASE_SECRET_HEADER,
            Some(ERASE_SECRET),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user_id"], stranger.to_string());
        assert!(body["deleted"]
            .as_object()
            .unwrap()
            .values()
            .all(|n| n.as_i64() == Some(0)));
        assert_eq!(tombstone_count(&pool, stranger).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn erase_with_a_malformed_user_id_is_refused_before_it_is_rejected(pool: sqlx::PgPool) {
        let app = || app_with_secrets(pool.clone(), None, None, Some(ERASE_SECRET));
        let uri = "/svc/v1/users/not-a-uuid/erase";
        let (status, body) = post_with(app(), uri, ERASE_SECRET_HEADER, Some("wrong")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "forbidden");
        let (status, body) = post_with(app(), uri, ERASE_SECRET_HEADER, Some(ERASE_SECRET)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "bad_request");
        let tombstones: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.user_erasures")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tombstones, 0);
    }

    #[test]
    fn openapi_lists_new_svc_paths_and_schemes() {
        let api = crate::routes::openapi_for_tests();
        for (path, scheme) in [
            ("/svc/v1/facts/changes", "facts_export_secret"),
            ("/svc/v1/facts/deletions", "facts_export_secret"),
            ("/svc/v1/insights/deletions", "insights_export_secret"),
        ] {
            let op = &api["paths"][path]["get"];
            assert!(op.is_object(), "{path}");
            assert!(op["security"][0][scheme].is_array(), "{path} uses {scheme}");
        }
        let op = &api["paths"]["/svc/v1/users/{user_id}/erase"]["post"];
        assert!(op.is_object(), "the erase path is documented");
        assert!(op["security"][0]["user_erase_secret"].is_array());
        // The schemes live on the full document (`ApiDoc` + routes), as
        // `print-openapi` builds it.
        let (_router, full) =
            utoipa_axum::router::OpenApiRouter::<crate::state::AppState>::with_openapi(
                <crate::openapi::ApiDoc as utoipa::OpenApi>::openapi(),
            )
            .merge(crate::routes::router_for_openapi())
            .split_for_parts();
        let full = serde_json::to_value(full).unwrap();
        let schemes = &full["components"]["securitySchemes"];
        assert_eq!(
            schemes["facts_export_secret"]["name"],
            "x-facts-export-secret"
        );
        assert_eq!(schemes["facts_export_secret"]["in"], "header");
        assert_eq!(schemes["user_erase_secret"]["name"], "x-user-erase-secret");
        assert_eq!(schemes["user_erase_secret"]["in"], "header");
    }
}

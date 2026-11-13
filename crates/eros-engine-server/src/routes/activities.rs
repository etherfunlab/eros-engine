// SPDX-License-Identifier: AGPL-3.0-only
//! POST /v2/comp/session/{session_id}/activities/{activity_id}/end — the user
//! ends a date, or collects the result of one whose clock ran out (spec
//! docs/superpowers/specs/2026-11-12-activity-sub-session-design.md §5.5,
//! §6.1, §6.4).

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::affinity::Affinity;
use eros_engine_store::activity::{ActivityRepo, ActivityStatus};

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, StreamPreError};
use crate::pipeline::activity::settle;
use crate::routes::companion::AffinityDeltasDto;
use crate::routes::companion_stream::{resolve_text_session, StreamPreErrorBody};
use crate::routes::dto::{ActivityView, AffinitySnapshot};
use crate::state::AppState;

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct ActivityEndResponse {
    /// The date, `settled`; `ends_at` is when it ended.
    pub activity: ActivityView,
    /// The relationship after the date.
    pub affinity: AffinitySnapshot,
    /// What the date moved, as its settlement's affinity event committed it.
    /// `null` when the settlement wrote no event: no turn of the date was
    /// answered, or the relationship was archived meanwhile.
    pub effective_deltas: Option<AffinityDeltasDto>,
}

fn not_found() -> AppError {
    AppError::StreamPre(StreamPreError {
        status: StatusCode::NOT_FOUND,
        code: "activity_not_found",
        message: "activity not found in this session".into(),
        user_message: "找不到这次约会".into(),
        original_user_message_id: None,
    })
}

/// End a date and settle it.
///
/// `session_id` is the session the invitation was sent in. A date still under
/// way ends now; one whose clock ran out keeps the end it had. The date is
/// then settled in this call: the activity judge reads it once, its verdict
/// moves the relationship (each line by at most one tier), and the date goes
/// into the relationship's memory. A date already settled answers with the
/// result it stored, so a repeat call changes nothing.
///
/// `404 activity_not_found` when the activity is not this session's. `409
/// activity_settling` while another request settles the date; nothing waits.
/// `409 wrong_channel` for a date's own session id.
#[utoipa::path(
    post,
    path = "/v2/comp/session/{session_id}/activities/{activity_id}/end",
    tag = "companion",
    params(
        ("session_id" = Uuid, Path, description = "The session the invitation was sent in"),
        ("activity_id" = Uuid, Path, description = "The activity's id")
    ),
    responses(
        (status = 200, body = ActivityEndResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = StreamPreErrorBody),
        (status = 404, body = StreamPreErrorBody),
        (status = 409, body = StreamPreErrorBody)
    ),
    security(("bearer" = []))
)]
async fn end_activity(
    State(state): State<AppState>,
    Path((session_id, activity_id)): Path<(Uuid, Uuid)>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<ActivityEndResponse>, AppError> {
    Ok(Json(
        end_at(&state, session_id, activity_id, user_id, Utc::now()).await?,
    ))
}

/// The handler with its clock injected, for tests.
pub(crate) async fn end_at(
    state: &AppState,
    session_id: Uuid,
    activity_id: Uuid,
    user_id: Uuid,
    now: DateTime<Utc>,
) -> Result<ActivityEndResponse, AppError> {
    resolve_text_session(state, session_id, user_id).await?;
    let activities = ActivityRepo { pool: &state.pool };
    let activity = activities
        .get(activity_id)
        .await?
        .filter(|a| a.session_id == session_id)
        .ok_or_else(not_found)?;
    if activity.status_at(now) == ActivityStatus::Ongoing {
        activities.end_early(activity.id, now).await?;
    }
    let settled = settle(state, activity.id, now).await?;
    Ok(ActivityEndResponse {
        activity: ActivityView::from_record(&settled.activity, now),
        affinity: AffinitySnapshot::from(settled.affinity.unwrap_or_else(Affinity::unrecorded)),
        effective_deltas: settled
            .effective_deltas
            .as_ref()
            .map(AffinityDeltasDto::from),
    })
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(end_activity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request};
    use serde_json::{json, Value};
    use sqlx::PgPool;

    use crate::pipeline::activity::tests::{judge_mock, judged, scene, with_judge, JUDGE_REPLY};
    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{build_router, mint_test_jwt, send_request};

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn req(method: &str, uri: String, user_id: Uuid, body: Option<Value>) -> Request<Body> {
        let b = Request::builder().method(method).uri(uri).header(
            header::AUTHORIZATION,
            format!("Bearer {}", mint_test_jwt(user_id)),
        );
        match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string())),
            None => b.body(Body::empty()),
        }
        .unwrap()
    }

    fn end_uri(session_id: Uuid, activity_id: Uuid) -> String {
        format!("/v2/comp/session/{session_id}/activities/{activity_id}/end")
    }

    async fn count(pool: &PgPool, sql: &str, id: Uuid) -> i64 {
        sqlx::query_scalar(sql)
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// The parent's `activity` affinity events.
    async fn activity_events(pool: &PgPool, session_id: Uuid) -> i64 {
        count(
            pool,
            "SELECT count(*) FROM engine.companion_affinity_events e \
               JOIN engine.companion_affinity a ON a.id = e.affinity_id \
              WHERE a.session_id = $1 AND e.event_type = 'activity'",
            session_id,
        )
        .await
    }

    async fn settled_at(pool: &PgPool, activity_id: Uuid) -> Option<DateTime<Utc>> {
        sqlx::query_scalar("SELECT settled_at FROM engine.activities WHERE id = $1")
            .bind(activity_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn instance_of(pool: &PgPool, session_id: Uuid) -> Uuid {
        sqlx::query_scalar("SELECT instance_id FROM engine.chat_sessions WHERE id = $1")
            .bind(session_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn same_deltas(a: &AffinityDeltasDto, b: &AffinityDeltasDto) -> bool {
        [
            (a.warmth, b.warmth),
            (a.trust, b.trust),
            (a.intrigue, b.intrigue),
            (a.intimacy, b.intimacy),
            (a.patience, b.patience),
            (a.tension, b.tension),
        ]
        .iter()
        .all(|(x, y)| (x - y).abs() < 1e-12)
    }

    /// A date under way, ended by the user at 17:50: its event ends then,
    /// and the judge settles it in the same call. A repeat call returns what
    /// was stored and writes nothing.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn end_early_settles_and_returns_the_result(pool: PgPool) {
        let mock = judge_mock(judged(JUDGE_REPLY), 1).await;
        let s = scene(with_judge(test_state(pool.clone()), &mock), 2).await;
        let now = t("2026-11-12T17:50:00Z");

        let out = end_at(&s.state, s.session_id, s.activity.id, s.user_id, now)
            .await
            .unwrap();
        assert_eq!(out.activity.id, s.activity.id);
        assert_eq!(out.activity.status, "settled");
        assert_eq!(out.activity.ends_at, Some(now));
        let ends_at: DateTime<Utc> =
            sqlx::query_scalar("SELECT ends_at FROM engine.persona_events WHERE id = $1")
                .bind(s.activity.event_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(ends_at, now, "the event ends at the request");
        let first = out.effective_deltas.expect("the date's deltas");
        assert!(first.trust > 0.0, "{first:?}");
        assert!(out.affinity.trust > 0.0, "{:?}", out.affinity);
        assert_eq!(settled_at(&pool, s.activity.id).await, Some(now));

        let again = end_at(
            &s.state,
            s.session_id,
            s.activity.id,
            s.user_id,
            t("2026-11-12T17:55:00Z"),
        )
        .await
        .unwrap();
        assert_eq!(again.activity.status, "settled");
        assert_eq!(again.activity.ends_at, Some(now));
        let stored = again.effective_deltas.expect("the stored deltas");
        assert!(same_deltas(&stored, &first), "{stored:?} vs {first:?}");
        assert_eq!(activity_events(&pool, s.session_id).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn end_after_the_clock_settles(pool: PgPool) {
        let mock = judge_mock(judged(JUDGE_REPLY), 1).await;
        let s = scene(with_judge(test_state(pool.clone()), &mock), 2).await;
        let mut app = build_router(s.state.clone());

        let (status, body) = send_request(
            &mut app,
            req(
                "POST",
                end_uri(s.session_id, s.activity.id),
                s.user_id,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["activity"]["status"], json!("settled"), "{body}");
        assert_eq!(
            body["activity"]["sub_session_id"],
            json!(s.activity.sub_session_id)
        );
        let ends_at: DateTime<Utc> =
            serde_json::from_value(body["activity"]["ends_at"].clone()).unwrap();
        assert_eq!(ends_at, t("2026-11-12T18:30:00Z"), "the clock's end");
        assert!(body["effective_deltas"].is_object(), "{body}");
        assert!(body["affinity"]["bond_tier"].is_number(), "{body}");
        assert!(settled_at(&pool, s.activity.id).await.is_some());
        assert_eq!(activity_events(&pool, s.session_id).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn end_of_another_sessions_activity_is_404(pool: PgPool) {
        let mock = judge_mock(judged(JUDGE_REPLY), 0).await;
        let s = scene(with_judge(test_state(pool.clone()), &mock), 2).await;
        let instance_id = instance_of(&pool, s.session_id).await;
        let other =
            crate::routes::companion::testutil::seed_session(&pool, s.user_id, instance_id).await;
        let mut app = build_router(s.state.clone());

        for (session, activity) in [(other, s.activity.id), (s.session_id, Uuid::new_v4())] {
            let (status, body) = send_request(
                &mut app,
                req("POST", end_uri(session, activity), s.user_id, None),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
            assert_eq!(body["code"], json!("activity_not_found"));
        }
        assert_eq!(settled_at(&pool, s.activity.id).await, None);
        mock.verify().await;
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn end_on_a_sub_session_id_is_wrong_channel(pool: PgPool) {
        let mock = judge_mock(judged(JUDGE_REPLY), 0).await;
        let s = scene(with_judge(test_state(pool.clone()), &mock), 2).await;
        let mut app = build_router(s.state.clone());

        let (status, body) = send_request(
            &mut app,
            req(
                "POST",
                end_uri(s.activity.sub_session_id, s.activity.id),
                s.user_id,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], json!("wrong_channel"));
        assert_eq!(settled_at(&pool, s.activity.id).await, None);
        mock.verify().await;
    }

    /// The `activity` of `messages` whose id is `id`; `Value::Null` when the
    /// row has no such key.
    fn receipt(messages: &Value, id: Uuid) -> Value {
        messages
            .as_array()
            .expect("messages")
            .iter()
            .find(|m| m["id"] == json!(id))
            .unwrap_or_else(|| panic!("row {id} in {messages}"))
            .get("activity")
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Both history routes and the BFF start put the date's receipt on the
    /// accepted invitation and on no other row; the date's own rows are
    /// served under the sub-session's id.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn history_receipts_mark_the_accepted_invitation(pool: PgPool) {
        let s = scene(test_state(pool.clone()), 2).await;
        let invitation = s.activity.message_id.expect("the invitation");
        let refused: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, metadata, sent_at) \
             VALUES ($1, 'user', '再去一次吧', $2, '2026-11-12T19:00:00Z') RETURNING id",
        )
        .bind(s.session_id)
        .bind(json!({"action": {
            "type": "date", "duration": "short", "place": "山上", "content": "看星星",
            "response": "refuse"
        }}))
        .fetch_one(&pool)
        .await
        .unwrap();
        let instance_id = instance_of(&pool, s.session_id).await;
        let mut app = build_router(s.state.clone());

        let check = |messages: &Value| {
            let r = receipt(messages, invitation);
            assert_eq!(r["id"], json!(s.activity.id), "{messages}");
            assert_eq!(r["sub_session_id"], json!(s.activity.sub_session_id));
            assert_eq!(r["status"], json!("ended"));
            assert_eq!(r["place"], json!("海边"));
            assert_eq!(receipt(messages, refused), Value::Null);
            let with_receipt = messages
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m.get("activity").is_some())
                .count();
            assert_eq!(with_receipt, 1, "{messages}");
        };

        let (status, body) = send_request(
            &mut app,
            req(
                "GET",
                format!("/comp/chat/{}/history?limit=50", s.session_id),
                s.user_id,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        check(&body["messages"]);

        let (status, body) = send_request(
            &mut app,
            req(
                "GET",
                format!("/bff/v1/comp/chat/{}/history", s.session_id),
                s.user_id,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        check(&body["messages"]);

        let (status, body) = send_request(
            &mut app,
            req(
                "POST",
                "/bff/v1/comp/chat/start".into(),
                s.user_id,
                Some(json!({"instance_id": instance_id})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["session_id"], json!(s.session_id), "{body}");
        check(&body["history"]);

        let (status, body) = send_request(
            &mut app,
            req(
                "GET",
                format!("/comp/chat/{}/history?limit=50", s.activity.sub_session_id),
                s.user_id,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let contents: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert_eq!(contents, ["第0句", "回你第0句", "第1句", "回你第1句"]);
    }

    /// A date turn's event lives on the parent's affinity; asked by the
    /// sub-session's id, the delta route answers with it.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_delta_route_answers_for_a_sub_session(pool: PgPool) {
        let s = scene(test_state(pool.clone()), 1).await;
        let sub = s.activity.sub_session_id;
        let date_turn: Uuid = sqlx::query_scalar(
            "SELECT id FROM engine.chat_messages WHERE session_id = $1 AND role = 'user'",
        )
        .bind(sub)
        .fetch_one(&pool)
        .await
        .unwrap();
        let zero = json!({"warmth": 0.0, "trust": 0.0, "intrigue": 0.0, "intimacy": 0.0,
                          "patience": 0.0, "tension": 0.0});
        let event_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.companion_affinity_events \
               (affinity_id, event_type, deltas, effective_deltas, context, user_message_id) \
             SELECT id, 'message', $2, $2, '{\"eval_skip_reason\": \"activity\"}', $3 \
               FROM engine.companion_affinity WHERE session_id = $1 RETURNING id",
        )
        .bind(s.session_id)
        .bind(&zero)
        .bind(date_turn)
        .fetch_one(&pool)
        .await
        .unwrap();
        let mut app = build_router(s.state.clone());

        let (status, body) = send_request(
            &mut app,
            req(
                "GET",
                format!("/bff/v1/comp/affinity/{sub}/event"),
                s.user_id,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["session_id"], json!(sub));
        assert_eq!(body["event"]["event_id"], json!(event_id), "{body}");
        assert_eq!(body["event"]["user_message_id"], json!(date_turn));
        assert_eq!(body["event"]["effective_deltas"], zero);
    }

    /// The session routes refuse a date's own session (spec §5.3, §10); the
    /// listing omits it; archiving the relationship archives it, and the date
    /// can no longer be settled into it.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn sub_sessions_are_refused_by_the_session_routes(pool: PgPool) {
        let mock = judge_mock(judged(JUDGE_REPLY), 0).await;
        let s = scene(with_judge(test_state(pool.clone()), &mock), 2).await;
        let sub = s.activity.sub_session_id;
        let instance_id = instance_of(&pool, s.session_id).await;
        let mut app = build_router(s.state.clone());

        for (method, uri, body) in [
            (
                "POST",
                format!("/v2/comp/session/{sub}/open"),
                Some(json!({})),
            ),
            (
                "POST",
                format!("/v2/comp/session/{sub}/events"),
                Some(json!({"event_key": "k1"})),
            ),
            ("GET", format!("/v2/comp/session/{sub}/effects"), None),
            (
                "POST",
                format!("/v2/comp/session/{sub}/life-choices/{}", Uuid::new_v4()),
                Some(json!({"response": "accept"})),
            ),
        ] {
            let (status, resp) =
                send_request(&mut app, req(method, uri.clone(), s.user_id, body)).await;
            assert_eq!(status, StatusCode::CONFLICT, "{uri}: {resp}");
            assert_eq!(resp["code"], json!("wrong_channel"), "{uri}");
        }

        let (status, body) = send_request(
            &mut app,
            req(
                "GET",
                format!("/comp/chat/{}/sessions", s.user_id),
                s.user_id,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let listed: Vec<&Value> = body["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| &e["session_id"])
            .collect();
        assert_eq!(listed, [&json!(s.session_id)], "{body}");

        let (status, body) = send_request(
            &mut app,
            req(
                "DELETE",
                format!("/comp/instance/{instance_id}/sessions"),
                s.user_id,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["archived_sessions"], json!(2));
        let archived: bool =
            sqlx::query_scalar("SELECT archived FROM engine.chat_sessions WHERE id = $1")
                .bind(sub)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(archived);

        let (status, body) = send_request(
            &mut app,
            req(
                "POST",
                end_uri(s.session_id, s.activity.id),
                s.user_id,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM engine.companion_affinity WHERE instance_id = $1",
                instance_id
            )
            .await,
            0
        );
        assert_eq!(settled_at(&pool, s.activity.id).await, None);
        mock.verify().await;
    }
}

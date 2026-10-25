// SPDX-License-Identifier: AGPL-3.0-only
//! `GET /v2/comp/session/{session_id}/effects` — what accepted gives are still
//! doing to the persona (spec
//! docs/superpowers/specs/2026-10-12-given-item-lingering-effects-design.md §9.2).
//!
//! The state a text turn sent now would carry. The `body_state` stream frame
//! reports a turn's own state; this route reports the persona as she is now,
//! for a page that opens on a conversation and for async callers.

use axum::extract::{Extension, Path, State};
use axum::Json;
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use eros_engine_core::effects::{ActiveEffect, EffectPhase};

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::routes::companion_stream::{resolve_text_session, GivenItemDto, StreamPreErrorBody};
use crate::state::AppState;

/// Where an effect is on its curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EffectPhaseDto {
    Rising,
    Peak,
    Fading,
}

/// One active effect. Also the element shape of the `body_state` stream frame.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct EffectView {
    pub item: GivenItemDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub quantity: u8,
    /// 1–4; 4 only for a quantity above 5.
    pub level: u8,
    pub phase: EffectPhaseDto,
    /// This turn's step, 1-based.
    pub step: u8,
    /// How many turns the effect lasts: `min(quantity, 5)`.
    pub steps: u8,
}

impl From<&ActiveEffect> for EffectView {
    fn from(e: &ActiveEffect) -> Self {
        Self {
            item: e.item.into(),
            name: e.name.clone(),
            quantity: e.quantity,
            level: e.level,
            phase: match e.phase {
                EffectPhase::Rising => EffectPhaseDto::Rising,
                EffectPhase::Peak => EffectPhaseDto::Peak,
                EffectPhase::Fading => EffectPhaseDto::Fading,
            },
            step: e.step,
            steps: e.steps,
        }
    }
}

/// One need's level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NeedLevelDto {
    Low,
    Mid,
    High,
}

impl From<eros_engine_core::needs::NeedLevel> for NeedLevelDto {
    fn from(l: eros_engine_core::needs::NeedLevel) -> Self {
        use eros_engine_core::needs::NeedLevel;
        match l {
            NeedLevel::Low => NeedLevelDto::Low,
            NeedLevel::Mid => NeedLevelDto::Mid,
            NeedLevel::High => NeedLevelDto::High,
        }
    }
}

/// The persona's four needs (spec 2026-10-13-persona-needs-design.md §3).
/// Also the `needs` shape of the `body_state` stream frame.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct NeedsView {
    pub hunger: NeedLevelDto,
    pub social: NeedLevelDto,
    pub stamina: NeedLevelDto,
    pub emotional: NeedLevelDto,
}

impl From<&eros_engine_core::needs::Needs> for NeedsView {
    fn from(n: &eros_engine_core::needs::Needs) -> Self {
        Self {
            hunger: n.hunger.into(),
            social: n.social.into(),
            stamina: n.stamina.into(),
            emotional: n.emotional.into(),
        }
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct EffectsResponse {
    /// Newest give first; empty when nothing is at work.
    pub effects: Vec<EffectView>,
    /// The persona's needs carried to now; `null` with no rating, or when the
    /// deployment runs no LLM judge.
    pub needs: Option<NeedsView>,
    /// The persona's foreground event under way now, or `null`.
    pub event: Option<crate::routes::session_events::EventView>,
    /// The persona's background under way now (a trip, an illness), or
    /// `null`.
    pub background_event: Option<crate::routes::session_events::EventView>,
    /// The server time this response was computed for: the instant the
    /// carried needs and `changes_at` derive from. A client schedules its
    /// re-read by `changes_at - as_of`, counted from the moment it received
    /// the response, so the device clock does not matter.
    pub as_of: chrono::DateTime<chrono::Utc>,
    /// The earliest time at which this response changes with no new turn and
    /// no new event: the next drift step of a need, or the end of `event` or
    /// `background_event`. `null` when nothing changes on its own.
    pub changes_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The persona's lingering state from accepted gives, her needs and the
/// incident and the background under way, as a text turn sent now would carry
/// them.
#[utoipa::path(
    get,
    path = "/v2/comp/session/{session_id}/effects",
    tag = "companion",
    params(("session_id" = Uuid, Path, description = "Chat session id")),
    responses(
        (status = 200, body = EffectsResponse),
        (status = 401, description = "missing or invalid bearer"),
        (status = 403, body = StreamPreErrorBody),
        (status = 404, body = StreamPreErrorBody),
        (status = 409, body = StreamPreErrorBody),
    ),
    security(("bearer" = []))
)]
async fn get_effects(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Extension(AuthUser(user_id)): Extension<AuthUser>,
) -> Result<Json<EffectsResponse>, AppError> {
    let (_session, instance_id) = resolve_text_session(&state, session_id, user_id).await?;
    let now = chrono::Utc::now();
    let effects = crate::pipeline::handlers::load_effects(&state.pool, session_id, None).await?;
    let rating = if state.model_config.pde_enabled() {
        eros_engine_store::chat::ChatRepo { pool: &state.pool }
            .latest_needs(instance_id, None)
            .await?
    } else {
        None
    };
    // As `load_needs` carries them, plus when the carried levels next step.
    let (needs, needs_change) = match &rating {
        Some(r) => {
            let hours = (now - r.rated_at).num_milliseconds() as f64 / 3_600_000.0;
            let next = eros_engine_core::needs::next_change(r.needs, hours).map(|h| {
                r.rated_at + chrono::Duration::milliseconds((h * 3_600_000.0).round() as i64)
            });
            (Some(eros_engine_core::needs::carried(r.needs, hours)), next)
        }
        None => (None, None),
    };
    let current = eros_engine_store::events::EventRepo { pool: &state.pool }
        .current(instance_id, None, now)
        .await?;
    // Whether an event keeps her offline can depend on the persona. A BYOC
    // instance has no genome, no events and no traits.
    let traits = if current.foreground.is_some() || current.background.is_some() {
        eros_engine_store::persona::PersonaRepo { pool: &state.pool }
            .load_companion(instance_id)
            .await?
            .as_ref()
            .map(crate::prompt::persona_traits)
            .unwrap_or_default()
    } else {
        Default::default()
    };
    let view = |e: &Option<eros_engine_store::events::EventRecord>| {
        e.as_ref()
            .map(|e| crate::routes::session_events::EventView::new(e, &traits))
    };
    let changes_at = [
        needs_change,
        current.foreground.as_ref().map(|e| e.ends_at),
        current.background.as_ref().map(|e| e.ends_at),
    ]
    .into_iter()
    .flatten()
    .min();
    Ok(Json(EffectsResponse {
        effects: effects.iter().map(EffectView::from).collect(),
        needs: needs.as_ref().map(NeedsView::from),
        event: view(&current.foreground),
        background_event: view(&current.background),
        as_of: now,
        changes_at,
    }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_effects))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
    use eros_engine_core::types::GivenItem;
    use serde_json::json;
    use tower::ServiceExt;

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        build_router, mint_test_jwt, seed_byoc_session, seed_persona_instance, seed_session,
    };

    async fn get(
        pool: sqlx::PgPool,
        session_id: Uuid,
        user: Uuid,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method("GET")
            .uri(format!("/v2/comp/session/{session_id}/effects"))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", mint_test_jwt(user)),
            )
            .body(Body::empty())
            .unwrap();
        let res = build_router(test_state(pool)).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 65536).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn user_row(
        pool: &sqlx::PgPool,
        session_id: Uuid,
        mins_ago: i32,
        metadata: serde_json::Value,
    ) -> chrono::DateTime<chrono::Utc> {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, metadata, sent_at) \
             VALUES ($1, 'user', '', $2, now() - make_interval(mins => $3)) RETURNING sent_at",
        )
        .bind(session_id)
        .bind(metadata)
        .bind(mins_ago)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn effects_409_when_session_is_a_group(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let group = crate::routes::companion::testutil::seed_group_session(&pool, owner).await;
        let (status, _) = get(pool, group, owner).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn effects_serve_a_byoc_session(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let (_, session_id) = seed_byoc_session(&pool, owner).await;
        let (s, body) = get(pool, session_id, owner).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(
            without_as_of(body),
            json!({"effects": [], "needs": null, "event": null, "background_event": null, "changes_at": null})
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_newest_turn_counts_as_step_one(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        user_row(&pool, session_id, 3,
            json!({"action": {"type": "give", "item": "alcohol", "name": "威士忌", "quantity": 3, "response": "accept"}})).await;
        user_row(&pool, session_id, 2, json!({})).await;
        user_row(&pool, session_id, 1,
            json!({"action": {"type": "give", "item": "cigarette", "quantity": 1, "response": "accept"}})).await;

        let (s, body) = get(pool, session_id, owner).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            without_as_of(body),
            json!({"effects": [
                {"item": "cigarette", "quantity": 1, "level": 3, "phase": "peak", "step": 1, "steps": 1},
                {"item": "alcohol", "name": "威士忌", "quantity": 3, "level": 3, "phase": "peak", "step": 3, "steps": 3},
            ], "needs": null, "event": null, "background_event": null, "changes_at": null})
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn nothing_at_work_is_an_empty_list(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        let (s, body) = get(pool, session_id, owner).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            without_as_of(body),
            json!({"effects": [], "needs": null, "event": null, "background_event": null, "changes_at": null})
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_event_under_way_is_listed(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        let started = chrono::Utc::now() - chrono::Duration::minutes(30);
        let ends = started + chrono::Duration::hours(4);
        let written = eros_engine_store::events::EventRepo { pool: &pool }
            .insert(
                &eros_engine_store::events::NewEvent {
                    instance_id,
                    session_id,
                    event_key: "k1",
                    kind: eros_engine_core::events::EventKind::TempWork,
                    started_at: started,
                    ends_at: ends,
                },
                Some(&eros_engine_store::chat::ProactiveInsert {
                    id: Uuid::new_v4(),
                    content: "临时被叫去加班".into(),
                    generation_id: None,
                    metadata: json!({"proactive": "event"}),
                    llm_attempts: None,
                    gateway_errors: None,
                }),
            )
            .await
            .unwrap()
            .unwrap();
        let (s, body) = get(pool, session_id, owner).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["event"]["kind"], json!("temp_work"));
        assert_eq!(
            body["event"]["message_id"],
            json!(written.message.unwrap().id)
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_background_and_the_foreground_are_listed(pool: sqlx::PgPool) {
        use eros_engine_core::events::EventKind;
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        let now = chrono::Utc::now();
        let repo = eros_engine_store::events::EventRepo { pool: &pool };
        for (key, kind, from, to) in [
            (
                "trip",
                EventKind::BusinessTrip,
                now - chrono::Duration::hours(2),
                now + chrono::Duration::hours(22),
            ),
            (
                "away",
                EventKind::AwayFromPhone,
                now - chrono::Duration::minutes(5),
                now + chrono::Duration::hours(1),
            ),
        ] {
            repo.insert(
                &eros_engine_store::events::NewEvent {
                    instance_id,
                    session_id,
                    event_key: key,
                    kind,
                    started_at: from,
                    ends_at: to,
                },
                None,
            )
            .await
            .unwrap()
            .unwrap();
        }
        let (s, body) = get(pool, session_id, owner).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["event"]["kind"], json!("away_from_phone"));
        assert_eq!(body["event"]["category"], json!("mid"));
        assert_eq!(body["event"]["message_id"], json!(null));
        assert_eq!(body["background_event"]["kind"], json!("business_trip"));
        assert_eq!(body["background_event"]["category"], json!("trip"));
        assert_eq!(body["event"]["offline"], json!(true));
        assert_eq!(body["background_event"]["offline"], json!(false));
        assert_eq!(
            body["changes_at"], body["event"]["ends_at"],
            "the foreground ends first"
        );
    }

    /// Whether she is offline in `out_shopping` depends on the persona: only
    /// a female one goes dark, so the wire flag follows the persona.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn out_shopping_is_offline_only_for_a_female_persona(pool: sqlx::PgPool) {
        use eros_engine_core::events::EventKind;
        for (art, offline) in [
            (json!({"gender": "female"}), true),
            (json!({"gender": "male"}), false),
            (json!({}), false),
        ] {
            let owner = Uuid::new_v4();
            let instance_id = seed_persona_instance(&pool, owner).await;
            sqlx::query(
                "UPDATE engine.persona_genomes SET art_metadata = $2 \
                 WHERE id = (SELECT genome_id FROM engine.persona_instances WHERE id = $1)",
            )
            .bind(instance_id)
            .bind(&art)
            .execute(&pool)
            .await
            .unwrap();
            let session_id = seed_session(&pool, owner, instance_id).await;
            let now = chrono::Utc::now();
            eros_engine_store::events::EventRepo { pool: &pool }
                .insert(
                    &eros_engine_store::events::NewEvent {
                        instance_id,
                        session_id,
                        event_key: "shopping",
                        kind: EventKind::OutShopping,
                        started_at: now - chrono::Duration::minutes(5),
                        ends_at: now + chrono::Duration::hours(2),
                    },
                    None,
                )
                .await
                .unwrap()
                .unwrap();
            let (s, body) = get(pool.clone(), session_id, owner).await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(body["event"]["kind"], json!("out_shopping"), "{art}");
            assert_eq!(body["event"]["offline"], json!(offline), "{art}");
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_gate_rejects_strangers_unknown_and_voice_sessions(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        assert_eq!(
            get(pool.clone(), session_id, Uuid::new_v4()).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get(pool.clone(), Uuid::new_v4(), owner).await.0,
            StatusCode::NOT_FOUND
        );
        let voice: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id, channel) VALUES ($1, $2, 'voice') RETURNING id",
        )
        .bind(owner)
        .bind(instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let (s, body) = get(pool, voice, owner).await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(body["code"], "wrong_channel");
    }

    async fn get_judged(
        pool: sqlx::PgPool,
        session_id: Uuid,
        user: Uuid,
    ) -> (StatusCode, serde_json::Value) {
        let mut state = test_state(pool);
        state.model_config = std::sync::Arc::new(
            eros_engine_llm::model_config::ModelConfig::from_toml_str(
                "[tasks.pde_decision]\nmodel=\"j\"\nfilter_prompt=\"Decide.\"\n",
            )
            .unwrap(),
        );
        let req = Request::builder()
            .method("GET")
            .uri(format!("/v2/comp/session/{session_id}/effects"))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", mint_test_jwt(user)),
            )
            .body(Body::empty())
            .unwrap();
        let res = build_router(state).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 65536).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn needs_are_carried_to_now(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        // Rated 5 hours ago: hunger low → mid (4h period); the rest unchanged.
        let rated_at = user_row(&pool, session_id, 300,
            json!({"needs": {"hunger": "low", "social": "mid", "stamina": "high", "emotional": "mid"}})).await;
        let (s, body) = get_judged(pool, session_id, owner).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            body["needs"],
            json!({"hunger": "mid", "social": "mid", "stamina": "high", "emotional": "mid"})
        );
        // Hunger reaches high and stamina mid 8 hours after the rating.
        assert_eq!(
            body["changes_at"],
            json!(rated_at + chrono::Duration::hours(8))
        );
    }

    /// `as_of` is the handler's instant and differs per run: check it parses,
    /// then compare the rest.
    fn without_as_of(mut body: serde_json::Value) -> serde_json::Value {
        let as_of = body
            .as_object_mut()
            .unwrap()
            .remove("as_of")
            .expect("as_of present");
        serde_json::from_value::<chrono::DateTime<chrono::Utc>>(as_of).expect("as_of is RFC 3339");
        body
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn as_of_is_the_instant_changes_at_counts_from(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        // Hunger steps 4 hours after the rating, 10 minutes from now.
        user_row(&pool, session_id, 230,
            json!({"needs": {"hunger": "low", "social": "high", "stamina": "mid", "emotional": "mid"}})).await;
        let (s, body) = get_judged(pool, session_id, owner).await;
        assert_eq!(s, StatusCode::OK);
        let as_of: chrono::DateTime<chrono::Utc> =
            serde_json::from_value(body["as_of"].clone()).unwrap();
        let changes_at: chrono::DateTime<chrono::Utc> =
            serde_json::from_value(body["changes_at"].clone()).unwrap();
        let gap = changes_at - as_of;
        assert!(
            gap > chrono::Duration::minutes(8) && gap <= chrono::Duration::minutes(10),
            "{gap}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_need_stepping_before_the_event_ends_sets_changes_at(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        // Hunger steps 4 hours after the rating, 10 minutes from now.
        let rated_at = user_row(&pool, session_id, 230,
            json!({"needs": {"hunger": "low", "social": "high", "stamina": "mid", "emotional": "mid"}})).await;
        let now = chrono::Utc::now();
        eros_engine_store::events::EventRepo { pool: &pool }
            .insert(
                &eros_engine_store::events::NewEvent {
                    instance_id,
                    session_id,
                    event_key: "k1",
                    kind: eros_engine_core::events::EventKind::Reading,
                    started_at: now - chrono::Duration::minutes(5),
                    ends_at: now + chrono::Duration::hours(2),
                },
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let (s, body) = get_judged(pool, session_id, owner).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["event"]["kind"], json!("reading"));
        assert_eq!(
            body["changes_at"],
            json!(rated_at + chrono::Duration::hours(4))
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn needs_at_rest_and_no_event_change_nothing(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        user_row(&pool, session_id, 10,
            json!({"needs": {"hunger": "high", "social": "high", "stamina": "mid", "emotional": "mid"}})).await;
        let (s, body) = get_judged(pool, session_id, owner).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body["needs"].is_object(), "{body}");
        assert_eq!(
            body.get("changes_at"),
            Some(&serde_json::Value::Null),
            "{body}"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn needs_are_null_without_a_rating_or_without_the_judge(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        let (s, body) = get_judged(pool.clone(), session_id, owner).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body.get("needs"), Some(&serde_json::Value::Null), "{body}");
        user_row(&pool, session_id, 10,
            json!({"needs": {"hunger": "high", "social": "mid", "stamina": "mid", "emotional": "mid"}})).await;
        let (s, body) = get(pool, session_id, owner).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            body.get("needs"),
            Some(&serde_json::Value::Null),
            "no judge configured: {body}"
        );
    }

    #[test]
    fn needs_view_matches_the_frame_shape() {
        use eros_engine_core::needs::{NeedLevel, Needs};
        let n = Needs {
            hunger: NeedLevel::High,
            social: NeedLevel::Low,
            stamina: NeedLevel::Mid,
            emotional: NeedLevel::Mid,
        };
        assert_eq!(
            serde_json::to_value(NeedsView::from(&n)).unwrap(),
            serde_json::to_value(n).unwrap()
        );
    }

    /// The route's element and the frame's element serialize identically.
    #[test]
    fn effect_view_matches_the_frame_shape() {
        let e = ActiveEffect {
            item: GivenItem::Medicine,
            name: Some("布洛芬".into()),
            quantity: 9,
            level: 4,
            phase: EffectPhase::Peak,
            step: 3,
            steps: 5,
        };
        assert_eq!(
            serde_json::to_value(EffectView::from(&e)).unwrap(),
            serde_json::to_value(&e).unwrap()
        );
    }
}

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
use eros_engine_core::types::GivenItem;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::routes::companion_stream::{resolve_text_turn, GivenItemDto, StreamPreErrorBody};
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
            item: match e.item {
                GivenItem::Cigarette => GivenItemDto::Cigarette,
                GivenItem::Alcohol => GivenItemDto::Alcohol,
                GivenItem::Medicine => GivenItemDto::Medicine,
            },
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

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct EffectsResponse {
    /// Newest give first; empty when nothing is at work.
    pub effects: Vec<EffectView>,
}

/// The persona's lingering state from accepted gives, as a text turn sent now
/// would carry it.
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
    resolve_text_turn(&state, session_id, user_id).await?;
    let effects = crate::pipeline::handlers::load_effects(&state.pool, session_id, None).await?;
    Ok(Json(EffectsResponse {
        effects: effects.iter().map(EffectView::from).collect(),
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
    use serde_json::json;
    use tower::ServiceExt;

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{
        build_router, mint_test_jwt, seed_persona_instance, seed_session,
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
    ) {
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, metadata, sent_at) \
             VALUES ($1, 'user', '', $2, now() - make_interval(mins => $3))",
        )
        .bind(session_id)
        .bind(metadata)
        .bind(mins_ago)
        .execute(pool)
        .await
        .unwrap();
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
            body,
            json!({"effects": [
                {"item": "cigarette", "quantity": 1, "level": 3, "phase": "peak", "step": 1, "steps": 1},
                {"item": "alcohol", "name": "威士忌", "quantity": 3, "level": 3, "phase": "peak", "step": 3, "steps": 3},
            ]})
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn nothing_at_work_is_an_empty_list(pool: sqlx::PgPool) {
        let owner = Uuid::new_v4();
        let instance_id = seed_persona_instance(&pool, owner).await;
        let session_id = seed_session(&pool, owner, instance_id).await;
        let (s, body) = get(pool, session_id, owner).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body, json!({"effects": []}));
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

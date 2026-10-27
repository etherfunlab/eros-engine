// SPDX-License-Identifier: AGPL-3.0-only
//! Top-level route composition.
//!
//! The HTTP surface is split into three independently-authed sub-trees:
//!   * Public:        `/healthz` — no auth
//!   * Bearer JWT:    `/comp/*`, `/bff/v1/*`, `/world/*`, `/persona/*` — Supabase JWT (see auth::middleware)
//!   * Service secrets: `/svc/v1/*` — three sub-trees, each behind its own secret header
//!     (`x-insights-export-secret`, `x-facts-export-secret`, `x-user-erase-secret`); a sub-tree is
//!     mounted only when its secret is configured
//!   * Caller secret: when `CALLER_SECRET` is set, the Bearer-JWT tree also needs
//!     `x-caller-secret`, checked before the JWT
//!
//! The auth layer is applied to the `/comp` + `/bff` merge, NOT the
//! top-level merge, so the public `/healthz` route stays unauthenticated
//! after the outer merge.

use axum::middleware::from_fn_with_state;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::middleware::{require_auth, require_caller_secret};
use crate::state::AppState;

pub mod bff;
pub mod byoc;
pub mod companion;
pub mod companion_async;
pub mod companion_stream;
pub mod dto;
pub mod group;
pub mod health;
pub mod image_edit;
pub mod insight;
pub mod life_choices;
pub mod persona;
pub mod persona_items;
pub mod reaction;
pub mod session_effects;
pub mod session_events;
pub mod session_open;
pub mod svc;
pub mod voice;
pub mod world_town;

/// Compose the full app router with auth layers attached.
///
/// The `#[utoipa::path]` annotations on companion handlers
/// already include the full route prefix, so we MERGE rather than NEST:
/// nesting would double the prefix to `/comp/comp/...`. Each auth layer
/// attaches only to its merged sub-router, so the public `/healthz`
/// route stays unauthenticated even after the outer merge.
pub fn router(state: AppState) -> OpenApiRouter<AppState> {
    let comp = OpenApiRouter::new()
        .merge(companion::router())
        .merge(companion_async::router())
        .merge(companion_stream::router())
        .merge(voice::router())
        .merge(bff::router())
        .merge(byoc::router())
        .merge(world_town::router())
        .merge(persona::router())
        .merge(persona_items::router())
        .merge(insight::router())
        .merge(image_edit::router())
        .merge(session_effects::router())
        .merge(session_open::router())
        .merge(session_events::router())
        .merge(life_choices::router())
        .merge(reaction::router())
        .merge(group::router())
        .layer(from_fn_with_state(state.clone(), require_auth))
        .layer(from_fn_with_state(state.clone(), require_caller_secret));

    let mut app = OpenApiRouter::new().merge(health::router()).merge(comp);
    // Service sub-trees: secret-admitted, each mounted only when the
    // deployment set its own secret (INSIGHTS_EXPORT_SECRET,
    // FACTS_EXPORT_SECRET, USER_ERASE_SECRET). Separate secrets keep a
    // consumer of one stream from reaching another's routes. An unconfigured
    // sub-tree answers exactly as any unknown path does, so it does not
    // confirm the feature exists.
    if state.config.insights_export.secret.is_some() {
        app = app.merge(svc::router());
    }
    if state.config.facts_export.secret.is_some() {
        app = app.merge(svc::facts_router());
    }
    if state.config.user_erase.secret.is_some() {
        app = app.merge(svc::erase_router());
    }
    app
}

/// Same shape as [`router`] for OpenAPI extraction purposes, minus the auth
/// middleware (which doesn't affect the spec) and minus any need for a real
/// `AppState` (which would otherwise require a live DB pool). Used by the
/// `print-openapi` subcommand and the CI drift check.
pub fn router_for_openapi() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(health::router())
        .merge(companion::router())
        .merge(companion_async::router())
        .merge(companion_stream::router())
        .merge(voice::router())
        .merge(bff::router())
        .merge(byoc::router())
        .merge(world_town::router())
        .merge(persona::router())
        .merge(persona_items::router())
        .merge(insight::router())
        .merge(image_edit::router())
        .merge(session_effects::router())
        .merge(session_open::router())
        .merge(session_events::router())
        .merge(life_choices::router())
        .merge(reaction::router())
        .merge(group::router())
        .merge(svc::router())
        .merge(svc::facts_router())
        .merge(svc::erase_router())
}

/// The generated OpenAPI document as JSON, for tests that assert on the wire
/// contract itself (path presence, `deprecated` flags) rather than on handler
/// behaviour.
#[cfg(test)]
pub(crate) fn openapi_for_tests() -> serde_json::Value {
    let (_router, api) = router_for_openapi().split_for_parts();
    serde_json::to_value(api).expect("openapi serialises")
}

#[cfg(test)]
mod caller_gate_tests {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::Router;
    use sqlx::PgPool;
    use uuid::Uuid;

    use crate::routes::companion::test_state;
    use crate::routes::companion::testutil::{build_router, mint_test_jwt, send_request};

    const KEY_A: &str = "0123456789abcdef0123456789abcdef";
    const KEY_B: &str = "fedcba9876543210fedcba9876543210";

    fn app(pool: PgPool, keys: &[&str], byoc: Option<&str>, insights: Option<&str>) -> Router {
        let mut state = test_state(pool);
        state.config.caller_secret.secrets = keys.iter().map(|k| k.to_string()).collect();
        state.config.byoc.caller_secret = byoc.map(String::from);
        state.config.insights_export.secret = insights.map(String::from);
        build_router(state)
    }

    /// `method uri`, with a bearer for a fresh user when `bearer`, and the
    /// given `(header, value)` pairs.
    fn req(method: &str, uri: &str, bearer: bool, headers: &[(&str, &str)]) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if bearer {
            b = b.header(
                header::AUTHORIZATION,
                format!("Bearer {}", mint_test_jwt(Uuid::new_v4())),
            );
        }
        for (name, value) in headers {
            b = b.header(*name, *value);
        }
        b.body(Body::empty()).unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn gate_off_leaves_the_tree_as_today(pool: PgPool) {
        let mut app = app(pool, &[], None, None);
        let (status, body) = send_request(&mut app, req("GET", "/v2/comp/groups", true, &[])).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn gate_refuses_a_missing_header(pool: PgPool) {
        let mut app = app(pool, &[KEY_A], None, None);
        let (status, body) = send_request(&mut app, req("GET", "/v2/comp/groups", true, &[])).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "caller_forbidden");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn gate_refuses_a_wrong_value_without_echoing_it(pool: PgPool) {
        let mut app = app(pool, &[KEY_A], None, None);
        let wrong = [("x-caller-secret", KEY_B)];
        let (status, body) =
            send_request(&mut app, req("GET", "/v2/comp/groups", true, &wrong)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "caller_forbidden");
        let shown = body.to_string();
        assert!(!shown.contains(KEY_A) && !shown.contains(KEY_B), "{shown}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn gate_runs_before_jwt_auth(pool: PgPool) {
        let mut app = app(pool, &[KEY_A], None, None);
        let (status, body) =
            send_request(&mut app, req("GET", "/v2/comp/groups", false, &[])).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "no caller secret outranks no bearer"
        );
        assert_eq!(body["code"], "caller_forbidden");
        let right = [("x-caller-secret", KEY_A)];
        let (status, _) =
            send_request(&mut app, req("GET", "/v2/comp/groups", false, &right)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "admitted, then JWT auth");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn gate_admits_every_rotation_key(pool: PgPool) {
        let mut app = app(pool, &[KEY_A, KEY_B], None, None);
        for key in [KEY_A, KEY_B] {
            let h = [("x-caller-secret", key)];
            let (status, body) =
                send_request(&mut app, req("GET", "/v2/comp/groups", true, &h)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn gate_leaves_healthz_and_svc_open(pool: PgPool) {
        let mut app = app(pool, &[KEY_A], None, Some(KEY_B));
        let (status, _) = send_request(&mut app, req("GET", "/healthz", false, &[])).await;
        assert_eq!(status, StatusCode::OK);
        let svc = [("x-insights-export-secret", KEY_B)];
        let (status, body) = send_request(
            &mut app,
            req("GET", "/svc/v1/insights/changes", false, &svc),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn byoc_needs_the_caller_secret_too(pool: PgPool) {
        let mut app = app(pool, &[KEY_A], Some(KEY_B), None);
        let byoc_only = [("x-byoc-caller-secret", KEY_B)];
        let (status, body) = send_request(
            &mut app,
            req("POST", "/v2/comp/byoc/init", true, &byoc_only),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["code"], "caller_forbidden");
        let both = [("x-byoc-caller-secret", KEY_B), ("x-caller-secret", KEY_A)];
        let (status, body) =
            send_request(&mut app, req("POST", "/v2/comp/byoc/init", true, &both)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

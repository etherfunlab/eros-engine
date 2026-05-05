// SPDX-License-Identifier: AGPL-3.0-only
// TODO(T12): AppState is constructed in T12 once DATABASE_URL + Supabase JWT
// secret env wiring lands. ServerConfig fields are read by routes in T11.
#![allow(dead_code)]

use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub auth: Arc<dyn crate::auth::AuthValidator>,
    pub config: ServerConfig,
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub expose_affinity_debug: bool,
    pub ema_inertia: f64,
    pub bind_addr: String,
}

impl ServerConfig {
    pub fn from_env() -> Self {
        Self {
            expose_affinity_debug: std::env::var("EXPOSE_AFFINITY_DEBUG")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            ema_inertia: std::env::var("EMA_INERTIA")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.8),
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
        }
    }
}

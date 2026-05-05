// SPDX-License-Identifier: AGPL-3.0-only
use sqlx::PgPool;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
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

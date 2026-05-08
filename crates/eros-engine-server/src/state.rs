// SPDX-License-Identifier: AGPL-3.0-only

use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub auth: Arc<dyn crate::auth::AuthValidator>,
    pub config: ServerConfig,
    pub openrouter: Arc<eros_engine_llm::openrouter::OpenRouterClient>,
    pub voyage: Arc<eros_engine_llm::voyage::VoyageClient>,
    pub model_config: Arc<eros_engine_llm::model_config::ModelConfig>,
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub expose_affinity_debug: bool,
    pub ema_inertia: f64,
    /// Override inertia for sessions opened with `is_demo: true`. Smaller =
    /// each turn's delta is blended more aggressively, so the meters move
    /// visibly across an 8-turn demo. Falls back to `ema_inertia` if unset.
    pub demo_ema_inertia: f64,
    pub bind_addr: String,
}

impl ServerConfig {
    pub fn from_env() -> Self {
        let ema_inertia = std::env::var("EMA_INERTIA")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.8);
        Self {
            expose_affinity_debug: std::env::var("EXPOSE_AFFINITY_DEBUG")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false),
            ema_inertia,
            demo_ema_inertia: std::env::var("DEMO_EMA_INERTIA")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.3),
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
        }
    }
}

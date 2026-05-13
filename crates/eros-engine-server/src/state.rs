// SPDX-License-Identifier: AGPL-3.0-only

use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub auth: Arc<dyn crate::auth::AuthValidator>,
    pub config: ServerConfig,
    pub openrouter: Arc<eros_engine_llm::openrouter::OpenRouterClient>,
    pub voyage: Arc<eros_engine_llm::voyage::VoyageClient>,
    pub model_config: Arc<eros_engine_llm::model_config::ModelConfig>,
    /// Base URL of the marketplace service used by the self-heal /since
    /// puller. `None` runs the engine in OSS-only mode (no outbound pull
    /// loop spawned). Populated from `MARKETPLACE_SVC_URL` in main.rs.
    /// Read by the puller task — added in a follow-up task; the field is
    /// wired here so the env / boot-validation layer can land first.
    #[allow(dead_code)]
    pub marketplace_svc_url: Option<String>,
    /// Active HMAC secret used by `auth::s2s` middleware to verify (and
    /// sign outbound) /s2s/* requests. Populated from
    /// `MARKETPLACE_SVC_S2S_SECRET` in main.rs.
    pub marketplace_s2s_secret: Option<String>,
    /// Previous HMAC secret retained during rotation. Verify-only — never
    /// used to sign outbound. Populated from
    /// `MARKETPLACE_SVC_S2S_SECRET_PREVIOUS` in main.rs.
    pub marketplace_s2s_secret_previous: Option<String>,
    /// Shared reqwest client used by the self-heal /since puller for
    /// outbound calls to the marketplace service. Cheaply cloneable
    /// (internally Arc'd); construct once at boot with a sensible
    /// per-request timeout. Consumer task lands in a follow-up.
    #[allow(dead_code)]
    pub http_client: reqwest::Client,
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
    /// How often the dreaming-lite sweeper wakes up to look for idle
    /// sessions. Set to `Duration::ZERO` (env `DREAMING_DISABLED=1`) to
    /// skip spawning the sweeper entirely — useful for unit-test runs.
    pub dreaming_tick: Duration,
    /// Minimum idle time on `chat_sessions.last_active_at` before a
    /// session becomes eligible for classification.
    pub dreaming_idle_threshold: Duration,
    /// How long a `classification_claimed_at` claim is considered fresh.
    /// Older than this and the picker treats it as a crashed worker and
    /// re-claims the row. Should comfortably exceed the worst-case
    /// processing time (one LLM call + N voyage embeddings).
    pub dreaming_claim_stale_threshold: Duration,
}

impl ServerConfig {
    pub fn from_env() -> Self {
        let ema_inertia = std::env::var("EMA_INERTIA")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.8);
        let dreaming_disabled = std::env::var("DREAMING_DISABLED")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);
        let dreaming_tick = if dreaming_disabled {
            Duration::ZERO
        } else {
            Duration::from_secs(
                std::env::var("DREAMING_TICK_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(300),
            )
        };
        let dreaming_idle_threshold = Duration::from_secs(
            std::env::var("DREAMING_IDLE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1800),
        );
        let dreaming_claim_stale_threshold = Duration::from_secs(
            std::env::var("DREAMING_CLAIM_STALE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
        );
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
            dreaming_tick,
            dreaming_idle_threshold,
            dreaming_claim_stale_threshold,
        }
    }
}

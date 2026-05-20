// SPDX-License-Identifier: AGPL-3.0-only

use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub auth: Arc<dyn crate::auth::AuthValidator>,
    pub config: ServerConfig,
    pub openrouter: Arc<eros_engine_llm::openrouter::OpenRouterClient>,
    pub voyage: Arc<eros_engine_llm::voyage::VoyageClient>,
    pub model_config: Arc<eros_engine_llm::model_config::ModelConfig>,
    pub stream_slots: Arc<StreamSlots>,
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

/// Parse `OPENROUTER_USAGE_HIDDEN_KEYS` into a `HashSet<String>`.
/// Comma-separated; whitespace trimmed around each entry; empty
/// entries skipped. `None` or blank input → empty set (pass-through).
/// Extracted as a free function so tests don't have to mutate process
/// env to exercise edge cases.
pub(crate) fn parse_usage_hidden_keys(raw: Option<&str>) -> HashSet<String> {
    raw.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Per-user in-flight SSE stream counter. Used by the
/// `send_message_stream` handler to enforce spec §1.9 (≤3 concurrent
/// active streams per user, returning HTTP 429 over the cap).
///
/// Lock contention is negligible: each handler hits the map twice — once
/// to acquire, once to release — and the map is small (one entry per
/// active user). A Mutex<HashMap> beats taking on a `dashmap` dependency
/// for a hot path that runs at chat cadence.
#[derive(Debug, Default)]
pub struct StreamSlots {
    inner: Mutex<HashMap<Uuid, u32>>,
}

impl StreamSlots {
    /// Attempt to acquire a stream slot for `user_id`.
    ///
    /// Returns `Some(StreamSlotGuard)` if the current count is below `cap`,
    /// or `None` if the cap is already reached. The guard is `'static` (it
    /// holds an `Arc<StreamSlots>`) so it can be moved into SSE stream bodies
    /// without lifetime trouble.
    pub fn try_acquire(self: &Arc<Self>, user_id: Uuid, cap: u32) -> Option<StreamSlotGuard> {
        let mut guard = self.inner.lock().expect("StreamSlots mutex poisoned");
        let entry = guard.entry(user_id).or_insert(0);
        if *entry >= cap {
            return None;
        }
        *entry += 1;
        Some(StreamSlotGuard {
            slots: Arc::clone(self),
            user_id,
        })
    }
}

/// An RAII guard that decrements the per-user stream count when dropped.
///
/// Holds an `Arc<StreamSlots>` so it is `'static` and can be moved into
/// long-lived futures / SSE stream bodies.
pub struct StreamSlotGuard {
    slots: Arc<StreamSlots>,
    user_id: Uuid,
}

impl Drop for StreamSlotGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.slots.inner.lock() {
            if let Some(count) = guard.get_mut(&self.user_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    guard.remove(&self.user_id);
                }
            }
        }
    }
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
    /// Top-level keys removed from the `usage` object before it leaves the
    /// engine — both `CompanionReplyResponse.usage` (sync) and the SSE
    /// streaming `done` frame. Empty = pass-through. The DB persists the full
    /// unfiltered usage and tracing is unaffected, so operator observability
    /// stays intact. Populated from `OPENROUTER_USAGE_HIDDEN_KEYS`
    /// (comma-separated).
    pub openrouter_usage_hidden_keys: HashSet<String>,
}

impl ServerConfig {
    pub fn from_env() -> Self {
        let ema_inertia = std::env::var("EMA_INERTIA")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.5);
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
            openrouter_usage_hidden_keys: parse_usage_hidden_keys(
                std::env::var("OPENROUTER_USAGE_HIDDEN_KEYS")
                    .ok()
                    .as_deref(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_hidden_keys_from_env_parses_comma_separated() {
        let out = parse_usage_hidden_keys(Some("cost,cost_details"));
        assert!(out.contains("cost"));
        assert!(out.contains("cost_details"));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn usage_hidden_keys_from_env_trims_whitespace() {
        let out = parse_usage_hidden_keys(Some(" cost , cost_details "));
        assert!(out.contains("cost"));
        assert!(out.contains("cost_details"));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn usage_hidden_keys_from_env_skips_empty_entries() {
        let out = parse_usage_hidden_keys(Some("cost,,cost_details,"));
        assert!(out.contains("cost"));
        assert!(out.contains("cost_details"));
        assert_eq!(
            out.len(),
            2,
            "empty entries from extra commas must be skipped"
        );
    }

    #[test]
    fn usage_hidden_keys_from_env_empty_when_unset() {
        let out = parse_usage_hidden_keys(None);
        assert!(out.is_empty());
    }

    #[test]
    fn usage_hidden_keys_from_env_empty_when_blank() {
        let out = parse_usage_hidden_keys(Some(""));
        assert!(out.is_empty());
    }

    #[test]
    fn stream_slots_acquire_until_cap_then_blocks() {
        let slots = Arc::new(StreamSlots::default());
        let uid = Uuid::new_v4();

        let g1 = slots.try_acquire(uid, 2).expect("1st acquire under cap");
        let g2 = slots.try_acquire(uid, 2).expect("2nd acquire at cap-1");
        assert!(slots.try_acquire(uid, 2).is_none(), "3rd at cap rejected");
        drop(g1);
        let _g3 = slots.try_acquire(uid, 2).expect("acquire after drop ok");
        drop(g2);
    }
}

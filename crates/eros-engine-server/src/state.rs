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
    /// Compiled `[tasks.chat_companion].output_regex` rules, built once at boot
    /// (fail-fast). Empty when none configured. Read by `drive_chat_burst`.
    pub output_regex: Arc<Vec<eros_engine_llm::model_config::CompiledRegexRule>>,
    pub stream_slots: Arc<StreamSlots>,
    /// Whether `[tasks.world_director]` resolves to `Some` (a usable
    /// filter_prompt is present), computed once at boot. Gates
    /// `fetch_world_context` so a deployment with the section absent never
    /// pays the `world_states`/`world_enrollments` JOIN on the reply path —
    /// distinct from `config.world.disabled` / `prompt_disabled`, which are
    /// operator env-var kill switches for a subsystem that IS configured.
    pub world_configured: bool,
    /// Whether `[tasks.world_stories_director]` resolves to `Some` (a usable
    /// filter_prompt is present), computed once at boot. Mirrors
    /// `world_configured` but for the world-stories subsystem — distinct from
    /// `config.world.stories_disabled` / `stories_prompt_disabled`, which are
    /// operator env-var kill switches for a subsystem that IS configured.
    /// Read by `fetch_stories_context`'s gating.
    pub stories_configured: bool,
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

/// Knobs for the companion_insights_snapshot sweeper. Defaults: daily
/// 23:00 SGT, enabled. The cron string is stored raw and validated by
/// the sweeper at task start (so an invalid expression fails the sweeper
/// task only, not the whole server boot).
#[derive(Clone, Debug)]
pub struct SnapshotConfig {
    pub disabled: bool,
    pub cron: String,
    pub tz: chrono_tz::Tz,
}

/// Pure parser for the three env vars. Mirrors `parse_usage_hidden_keys`
/// in that tests can exercise edge cases without touching process env.
///
/// - `SNAPSHOT_DISABLED=1` → disabled
/// - `SNAPSHOT_CRON` raw 6-field cron string (default `"0 0 23 * * *"`)
/// - `SNAPSHOT_TZ` IANA zone (default `"Asia/Singapore"`; falls back on parse failure)
pub(crate) fn parse_snapshot_config(
    disabled_raw: Option<&str>,
    cron_raw: Option<&str>,
    tz_raw: Option<&str>,
) -> SnapshotConfig {
    let disabled = disabled_raw.map(|v| v == "1").unwrap_or(false);
    let cron = cron_raw
        .map(str::to_owned)
        .unwrap_or_else(|| "0 0 23 * * *".to_string());
    let tz = tz_raw
        .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
        .unwrap_or(chrono_tz::Asia::Singapore);
    SnapshotConfig { disabled, cron, tz }
}

/// Parse `PROMPT_LOG_DIR`. Empty or unset ⇒ `None` (logging disabled).
/// Any non-empty value is the destination directory for raw prompt logs.
pub(crate) fn parse_prompt_log_dir(raw: Option<&str>) -> Option<std::path::PathBuf> {
    raw.filter(|s| !s.is_empty()).map(std::path::PathBuf::from)
}

/// Knobs for the world-memories subsystem. Defaults: disabled off, prompt
/// injection off, town disabled off, stories disabled off, stories-prompt
/// injection off, 300-second sweep cadence.
#[derive(Clone, Debug)]
pub struct WorldConfig {
    pub disabled: bool,         // WORLD_DISABLED — master switch
    pub prompt_disabled: bool,  // WORLD_PROMPT_DISABLED — injection-only valve
    pub town_disabled: bool,    // WORLD_TOWN_DISABLED — town sweeper switch
    pub stories_disabled: bool, // WORLD_STORIES_DISABLED — stories rounds + injection off
    // Read by `fetch_stories_context`'s prompt-injection gating.
    pub stories_prompt_disabled: bool, // WORLD_STORIES_PROMPT_DISABLED — injection-only valve
    pub tick: Duration,                // WORLD_TICK_SECS, default 300
}

/// Pure parser for the six world-memories env vars (spec §3.1).
/// Booleans accept "1"/"true" — the DREAMING_DISABLED convention.
pub(crate) fn parse_world_config(
    disabled_raw: Option<&str>,
    prompt_disabled_raw: Option<&str>,
    town_disabled_raw: Option<&str>,
    stories_disabled_raw: Option<&str>,
    stories_prompt_disabled_raw: Option<&str>,
    tick_raw: Option<&str>,
) -> WorldConfig {
    let flag = |raw: Option<&str>| raw.map(|v| v == "1" || v == "true").unwrap_or(false);
    WorldConfig {
        disabled: flag(disabled_raw),
        prompt_disabled: flag(prompt_disabled_raw),
        town_disabled: flag(town_disabled_raw),
        stories_disabled: flag(stories_disabled_raw),
        stories_prompt_disabled: flag(stories_prompt_disabled_raw),
        tick: Duration::from_secs(tick_raw.and_then(|v| v.parse().ok()).unwrap_or(300)),
    }
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
    /// Cron-scheduled companion_insights_snapshot sweeper config. See
    /// `pipeline::snapshot` for the sweep loop.
    pub snapshot: SnapshotConfig,
    /// Destination directory for raw assembled main-reply prompts. `None`
    /// (env `PROMPT_LOG_DIR` unset or empty) disables prompt logging. When
    /// `Some`, each reply turn writes one human-readable file here. Contains
    /// raw chat content — operator-only; point it at a volume you control.
    pub prompt_log_dir: Option<std::path::PathBuf>,
    /// World memories subsystem configuration.
    pub world: WorldConfig,
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
        let snapshot = parse_snapshot_config(
            std::env::var("SNAPSHOT_DISABLED").ok().as_deref(),
            std::env::var("SNAPSHOT_CRON").ok().as_deref(),
            std::env::var("SNAPSHOT_TZ").ok().as_deref(),
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
            snapshot,
            prompt_log_dir: parse_prompt_log_dir(std::env::var("PROMPT_LOG_DIR").ok().as_deref()),
            world: parse_world_config(
                std::env::var("WORLD_DISABLED").ok().as_deref(),
                std::env::var("WORLD_PROMPT_DISABLED").ok().as_deref(),
                std::env::var("WORLD_TOWN_DISABLED").ok().as_deref(),
                std::env::var("WORLD_STORIES_DISABLED").ok().as_deref(),
                std::env::var("WORLD_STORIES_PROMPT_DISABLED")
                    .ok()
                    .as_deref(),
                std::env::var("WORLD_TICK_SECS").ok().as_deref(),
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

    #[test]
    fn snapshot_config_defaults_when_env_unset() {
        let cfg = parse_snapshot_config(None, None, None);
        assert!(!cfg.disabled);
        assert_eq!(cfg.cron, "0 0 23 * * *");
        assert_eq!(cfg.tz, chrono_tz::Asia::Singapore);
    }

    #[test]
    fn snapshot_config_disabled_when_env_says_one() {
        let cfg = parse_snapshot_config(Some("1"), None, None);
        assert!(cfg.disabled);
        let cfg = parse_snapshot_config(Some("0"), None, None);
        assert!(!cfg.disabled, "any value other than 1 leaves it enabled");
    }

    #[test]
    fn snapshot_config_honours_env_overrides() {
        let cfg = parse_snapshot_config(None, Some("0 */5 * * * *"), Some("UTC"));
        assert_eq!(cfg.cron, "0 */5 * * * *");
        assert_eq!(cfg.tz, chrono_tz::UTC);
    }

    #[test]
    fn snapshot_config_falls_back_on_bad_tz() {
        // Misspelled tz → default + (caller will warn-log; we just verify fallback)
        let cfg = parse_snapshot_config(None, None, Some("Not/A_Real_Zone"));
        assert_eq!(cfg.tz, chrono_tz::Asia::Singapore);
    }

    #[test]
    fn prompt_log_dir_unset_or_empty_is_none() {
        assert_eq!(parse_prompt_log_dir(None), None);
        assert_eq!(parse_prompt_log_dir(Some("")), None);
    }

    #[test]
    fn prompt_log_dir_set_is_some_path() {
        assert_eq!(
            parse_prompt_log_dir(Some("/data/prompt-logs")),
            Some(std::path::PathBuf::from("/data/prompt-logs")),
        );
    }

    #[test]
    fn world_config_defaults_when_env_unset() {
        let cfg = parse_world_config(None, None, None, None, None, None);
        assert!(!cfg.disabled);
        assert!(!cfg.prompt_disabled);
        assert!(!cfg.town_disabled);
        assert_eq!(cfg.tick, Duration::from_secs(300));
    }

    #[test]
    fn world_config_accepts_true_and_one() {
        for v in ["1", "true"] {
            let cfg = parse_world_config(Some(v), Some(v), None, None, None, None);
            assert!(cfg.disabled, "{v} must disable");
            assert!(cfg.prompt_disabled, "{v} must disable injection");
        }
        let cfg = parse_world_config(Some("false"), Some("0"), None, None, None, None);
        assert!(!cfg.disabled);
        assert!(!cfg.prompt_disabled);
    }

    #[test]
    fn world_config_parses_tick_and_falls_back_on_garbage() {
        assert_eq!(
            parse_world_config(None, None, None, None, None, Some("60")).tick,
            Duration::from_secs(60)
        );
        assert_eq!(
            parse_world_config(None, None, None, None, None, Some("not-a-number")).tick,
            Duration::from_secs(300)
        );
        // "0" parses fine here — Duration::ZERO is a legitimate value from the
        // parser's point of view. It's the sweeper (pipeline::world::sweeper)
        // that treats a zero tick as "disabled" and returns before building a
        // tokio::time::interval (which would panic on Duration::ZERO).
        assert_eq!(
            parse_world_config(None, None, None, None, None, Some("0")).tick,
            Duration::from_secs(0)
        );
    }

    #[test]
    fn parse_world_config_town_disabled_flag() {
        let c = parse_world_config(None, None, None, None, None, None);
        assert!(!c.town_disabled, "default off");
        let c = parse_world_config(None, None, Some("true"), None, None, None);
        assert!(c.town_disabled);
        let c = parse_world_config(None, None, Some("1"), None, None, None);
        assert!(c.town_disabled);
        let c = parse_world_config(None, None, Some("0"), None, None, None);
        assert!(!c.town_disabled);
    }

    #[test]
    fn parse_world_config_stories_flags() {
        let c = parse_world_config(None, None, None, None, None, None);
        assert!(!c.stories_disabled, "default off");
        assert!(!c.stories_prompt_disabled, "default: inject");
        let c = parse_world_config(None, None, None, Some("1"), Some("true"), None);
        assert!(c.stories_disabled);
        assert!(c.stories_prompt_disabled);
        let c = parse_world_config(None, None, None, Some("yes"), Some("0"), None);
        assert!(!c.stories_disabled, "only 1/true count");
        assert!(!c.stories_prompt_disabled);
    }
}

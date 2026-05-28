// SPDX-License-Identifier: AGPL-3.0-only
//! companion_insights_snapshot sweeper.
//!
//! On a cron schedule (default 23:00 SGT daily), inserts one row per
//! user with a companion_insights record into
//! engine.companion_insights_snapshot, preserving the JSONB and
//! training_level at that instant for downstream time-series consumers.
//! No LLM, no dedupe, no transformation.

use std::str::FromStr;

use chrono::Utc;
use cron::Schedule;

use eros_engine_store::insight::InsightRepo;

use crate::state::AppState;

/// Run forever. Spawn once at server startup. Returns immediately if
/// `SNAPSHOT_DISABLED=1` or if the cron expression fails to parse — the
/// sweeper failure does not affect the chat path.
pub async fn sweeper(state: AppState) {
    let cfg = &state.config.snapshot;
    if cfg.disabled {
        tracing::info!("snapshot sweeper disabled (SNAPSHOT_DISABLED=1)");
        return;
    }
    let schedule = match Schedule::from_str(&cfg.cron) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(cron = %cfg.cron, error = %e,
                "snapshot: invalid SNAPSHOT_CRON; sweeper disabled");
            return;
        }
    };
    tracing::info!(cron = %cfg.cron, tz = %cfg.tz, "snapshot sweeper starting");

    loop {
        let next = match schedule.upcoming(cfg.tz).next() {
            Some(n) => n.with_timezone(&Utc),
            None => {
                tracing::error!("snapshot cron yielded no upcoming fire; exiting");
                return;
            }
        };
        let delay = (next - Utc::now())
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(1));
        tokio::time::sleep(delay).await;

        let fire_at = Utc::now();
        let repo = InsightRepo { pool: &state.pool };
        match repo.snapshot_all_users(fire_at).await {
            Ok(n) => tracing::info!(written = n, %fire_at, "snapshot: fire complete"),
            Err(e) => {
                tracing::warn!(error = %e, "snapshot: fire failed; retrying next tick")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;
    use chrono_tz::Asia::Singapore;

    #[test]
    fn default_cron_parses() {
        Schedule::from_str("0 0 23 * * *").expect("default cron must parse");
    }

    #[test]
    fn default_cron_next_fire_in_sgt_is_23h_local() {
        let sched = Schedule::from_str("0 0 23 * * *").unwrap();
        let next = sched
            .upcoming(Singapore)
            .next()
            .expect("at least one upcoming fire");
        assert_eq!(next.hour(), 23, "next fire is at 23:00 SGT local");
        assert_eq!(next.minute(), 0);
        assert_eq!(next.second(), 0);
    }

    #[test]
    fn malformed_cron_returns_err() {
        assert!(Schedule::from_str("not a cron").is_err());
    }
}

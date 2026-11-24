// SPDX-License-Identifier: AGPL-3.0-only
//! Her 28-day cycle, derived from the event rows an instance already holds
//! (spec 2026-11-24-period-cycle-design.md §5).

use chrono::{DateTime, Duration, Utc};

use crate::events::{EventKind, Health, PastEvent};

/// Days from one due moment to the next.
pub const CYCLE_DAYS: i64 = 28;
/// Days before a due moment in which a `period_scare` can start.
pub const SCARE_WINDOW_DAYS: i64 = 3;
/// The most whole days a `period_scare` ends after the due moment.
pub const SCARE_MAX_LATE_DAYS: i64 = 3;

const CYCLE_MICROS: i64 = CYCLE_DAYS * 24 * 3600 * 1_000_000;

/// How many days a period lasts for a body of this health.
pub fn period_days(health: Health) -> i64 {
    match health {
        Health::Strong => 1,
        Health::Normal => 3,
        Health::Weak => 5,
    }
}

/// The two rows the derivation reads: the newest `period` row (the anchor)
/// and the newest `period_scare` row, both started at or before the moment
/// being read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CycleRows {
    pub period: Option<PastEvent>,
    pub scare: Option<PastEvent>,
}

/// Whole cycles from the anchor to `at`, rounded down: negative before the
/// anchor.
fn cycles_since(anchor: DateTime<Utc>, at: DateTime<Utc>) -> i64 {
    (at.timestamp_micros() - anchor.timestamp_micros()).div_euclid(CYCLE_MICROS)
}

/// `due(n)`: the anchor plus `n` cycles.
fn due(anchor: DateTime<Utc>, n: i64) -> DateTime<Utc> {
    anchor + Duration::days(CYCLE_DAYS * n)
}

/// The smallest `anchor + n × 28 days` (n ≥ 1) strictly after `at`.
pub fn next_due(anchor_started_at: DateTime<Utc>, at: DateTime<Utc>) -> DateTime<Utc> {
    due(
        anchor_started_at,
        cycles_since(anchor_started_at, at).max(0) + 1,
    )
}

/// When period `n` (n ≥ 1) starts: its due moment, or the end of the scare
/// that started in the window before it, whichever is later.
fn start_of(rows: &CycleRows, anchor: DateTime<Utc>, n: i64) -> DateTime<Utc> {
    let due_n = due(anchor, n);
    let window = Duration::days(SCARE_WINDOW_DAYS);
    match rows.scare {
        Some(s) if s.started_at >= due_n - window && s.started_at < due_n => due_n.max(s.ends_at),
        _ => due_n,
    }
}

/// Period `n` (n ≥ 1) as a span.
fn period_n(rows: &CycleRows, anchor: DateTime<Utc>, health: Health, n: i64) -> PastEvent {
    let started_at = start_of(rows, anchor, n);
    PastEvent {
        kind: EventKind::Period,
        started_at,
        ends_at: started_at + Duration::days(period_days(health)),
    }
}

/// The cycle state at `at` (spec §5): the anchor row under way, a
/// `period_scare` under way, or a derived period under way, as a
/// `Period` or `PeriodScare` with its span. `None` without an anchor.
pub fn state(rows: &CycleRows, health: Health, at: DateTime<Utc>) -> Option<PastEvent> {
    let anchor = rows.period?;
    let under_way = |e: &PastEvent| e.started_at <= at && at < e.ends_at;
    if under_way(&anchor) {
        return Some(anchor);
    }
    if let Some(scare) = rows.scare.filter(under_way) {
        return Some(scare);
    }
    let n = cycles_since(anchor.started_at, at);
    (n >= 1)
        .then(|| period_n(rows, anchor.started_at, health, n))
        .filter(under_way)
}

/// The period that started most recently at or before `at`, ended or not:
/// the anchor row before the first due moment, period `n` once it has
/// started; `None` between a due moment and a delayed period.
pub fn latest_period(rows: &CycleRows, health: Health, at: DateTime<Utc>) -> Option<PastEvent> {
    let anchor = rows.period?;
    match cycles_since(anchor.started_at, at) {
        ..0 => None,
        0 => Some(anchor),
        n => Some(period_n(rows, anchor.started_at, health, n)).filter(|p| p.started_at <= at),
    }
}

/// When her next period starts, counting a scare's delay; `None` without an
/// anchor. The first period that begins strictly after `at`; a start does not
/// depend on `health`.
pub fn next_period_start(
    rows: &CycleRows,
    _health: Health,
    at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let anchor = rows.period?.started_at;
    let n = cycles_since(anchor, at).max(1);
    let start = start_of(rows, anchor, n);
    Some(if start > at {
        start
    } else {
        start_of(rows, anchor, n + 1)
    })
}

/// `due` plus 1 to 3 whole days, uniform over `roll` in [0, 1); 1.0 lands on 3.
pub fn scare_ends(due: DateTime<Utc>, roll: f64) -> DateTime<Utc> {
    let days = (roll * SCARE_MAX_LATE_DAYS as f64) as i64 + 1;
    due + Duration::days(days.clamp(1, SCARE_MAX_LATE_DAYS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, m, d, h, 0, 0).unwrap()
    }

    fn period(a: DateTime<Utc>, b: DateTime<Utc>) -> PastEvent {
        PastEvent {
            kind: EventKind::Period,
            started_at: a,
            ends_at: b,
        }
    }

    fn scare(a: DateTime<Utc>, b: DateTime<Utc>) -> PastEvent {
        PastEvent {
            kind: EventKind::PeriodScare,
            started_at: a,
            ends_at: b,
        }
    }

    /// The anchor row starts 2026-01-01T08:00Z and lasts three days, so
    /// `due(1)` is 2026-01-29T08:00Z and `due(2)` 2026-02-26T08:00Z.
    fn rows(scare: Option<PastEvent>) -> CycleRows {
        CycleRows {
            period: Some(period(t(1, 1, 8), t(1, 4, 8))),
            scare,
        }
    }

    #[test]
    fn period_days_by_health() {
        assert_eq!(period_days(Health::Strong), 1);
        assert_eq!(period_days(Health::Normal), 3);
        assert_eq!(period_days(Health::Weak), 5);
    }

    #[test]
    fn no_anchor_no_cycle() {
        let only_scare = CycleRows {
            period: None,
            scare: Some(scare(t(1, 27, 10), t(1, 31, 8))),
        };
        assert_eq!(state(&only_scare, Health::Normal, t(1, 28, 0)), None);
        assert_eq!(
            next_period_start(&only_scare, Health::Normal, t(1, 28, 0)),
            None
        );
        assert_eq!(
            latest_period(&only_scare, Health::Normal, t(1, 28, 0)),
            None
        );
    }

    #[test]
    fn the_anchor_row_keeps_its_own_span() {
        // An old seven-day row, read as `strong`: still the row's span.
        let long = CycleRows {
            period: Some(period(t(1, 1, 8), t(1, 8, 8))),
            scare: None,
        };
        assert_eq!(
            state(&long, Health::Strong, t(1, 5, 0)),
            Some(period(t(1, 1, 8), t(1, 8, 8)))
        );
    }

    #[test]
    fn a_period_comes_on_every_due_moment() {
        let r = rows(None);
        let us = Duration::microseconds(1);
        assert_eq!(state(&r, Health::Normal, t(1, 10, 0)), None);
        assert_eq!(state(&r, Health::Normal, t(1, 29, 8) - us), None);
        assert_eq!(
            state(&r, Health::Normal, t(1, 29, 8)),
            Some(period(t(1, 29, 8), t(2, 1, 8)))
        );
        assert_eq!(
            state(&r, Health::Normal, t(2, 1, 8) - us),
            Some(period(t(1, 29, 8), t(2, 1, 8)))
        );
        assert_eq!(state(&r, Health::Normal, t(2, 1, 8)), None);
        assert_eq!(
            state(&r, Health::Normal, t(2, 26, 8)),
            Some(period(t(2, 26, 8), t(3, 1, 8)))
        );
    }

    #[test]
    fn health_is_read_when_derived() {
        let r = rows(None);
        assert_eq!(
            state(&r, Health::Strong, t(1, 29, 8)).unwrap().ends_at,
            t(1, 30, 8)
        );
        assert_eq!(
            state(&r, Health::Weak, t(1, 29, 8)).unwrap().ends_at,
            t(2, 3, 8)
        );
    }

    #[test]
    fn a_scare_in_the_window_delays_that_period_only() {
        let s = scare(t(1, 27, 10), t(1, 31, 8)); // ends due + 2 days
        let r = rows(Some(s));
        assert_eq!(state(&r, Health::Normal, t(1, 28, 0)), Some(s));
        assert_eq!(state(&r, Health::Normal, t(1, 30, 0)), Some(s));
        assert_eq!(
            state(&r, Health::Normal, t(1, 31, 8)),
            Some(period(t(1, 31, 8), t(2, 3, 8)))
        );
        assert_eq!(state(&r, Health::Normal, t(2, 3, 8)), None);
        // The grid did not move.
        assert_eq!(
            state(&r, Health::Normal, t(2, 26, 8)),
            Some(period(t(2, 26, 8), t(3, 1, 8)))
        );
    }

    #[test]
    fn a_scare_that_ends_before_the_due_moment_delays_nothing() {
        // Old one-hour rows.
        for s in [
            scare(t(1, 20, 10), t(1, 20, 11)),
            scare(t(1, 28, 10), t(1, 28, 11)),
        ] {
            assert_eq!(
                state(&rows(Some(s)), Health::Normal, t(1, 29, 8)),
                Some(period(t(1, 29, 8), t(2, 1, 8)))
            );
        }
    }

    #[test]
    fn next_due_is_strictly_after() {
        assert_eq!(next_due(t(1, 1, 8), t(1, 1, 8)), t(1, 29, 8));
        assert_eq!(next_due(t(1, 1, 8), t(1, 29, 8)), t(2, 26, 8));
    }

    #[test]
    fn the_next_period_start() {
        assert_eq!(
            next_period_start(&rows(None), Health::Normal, t(1, 10, 0)),
            Some(t(1, 29, 8))
        );
        let late = rows(Some(scare(t(1, 27, 10), t(1, 31, 8))));
        assert_eq!(
            next_period_start(&late, Health::Normal, t(1, 28, 0)),
            Some(t(1, 31, 8))
        );
        assert_eq!(
            next_period_start(&rows(None), Health::Normal, t(1, 30, 0)),
            Some(t(2, 26, 8))
        );
    }

    #[test]
    fn the_latest_period() {
        let r = rows(None);
        // The anchor row.
        assert_eq!(
            latest_period(&r, Health::Normal, t(1, 20, 0)),
            Some(period(t(1, 1, 8), t(1, 4, 8)))
        );
        // Ended a day ago.
        assert_eq!(
            latest_period(&r, Health::Normal, t(2, 2, 8)),
            Some(period(t(1, 29, 8), t(2, 1, 8)))
        );
        // Due has passed, the period has not started.
        let late = rows(Some(scare(t(1, 27, 10), t(1, 31, 8))));
        assert_eq!(latest_period(&late, Health::Normal, t(1, 30, 0)), None);
    }

    #[test]
    fn a_scare_ends_one_to_three_days_late() {
        for (roll, days) in [(0.0, 1), (0.5, 2), (0.99, 3), (1.0, 3)] {
            assert_eq!(
                scare_ends(t(1, 29, 8), roll),
                t(1, 29, 8) + Duration::days(days)
            );
        }
    }
}

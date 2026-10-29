// SPDX-License-Identifier: AGPL-3.0-only
//! The anniversary lines under `[now]` (spec 2026-10-28 memory-2-dreaming-plus
//! §5.4): the relationship's recurring journal rows that fall on the user's
//! local day. States facts only; one line each, emitted after the birthday
//! line.

use chrono::{Datelike, Days, Months, NaiveDate};
use eros_engine_store::journal::JournalRow;

use crate::holiday::LOOKAHEAD_DAYS;

/// Lines one turn may carry.
pub(crate) const ANNIVERSARY_MAX_LINES: usize = 2;

/// The lines for `today`, the user's local date (`None` without a request
/// timezone: no lines, like the holiday line). `rows` are the relationship's
/// real-world recurring rows. The first meeting's lines come first, then the
/// rest by date, at most [`ANNIVERSARY_MAX_LINES`].
pub(crate) fn anniversary_lines(rows: &[JournalRow], today: Option<NaiveDate>) -> Vec<String> {
    let Some(today) = today else {
        return Vec::new();
    };
    // `(not the first meeting, date, line)`; the sort is stable.
    let mut hits: Vec<(bool, NaiveDate, String)> = rows
        .iter()
        .filter(|r| r.realm == "real")
        .filter_map(|r| {
            if r.kind == "first_meet" {
                first_meet_line(r.occurred_on, today).map(|(date, line)| (false, date, line))
            } else if r.recurrence == "on_due" && r.due_on == Some(today) {
                let summary = r.summary.trim().trim_end_matches('。');
                Some((true, today, format!("今天是你们约好的日子：{summary}。")))
            } else {
                None
            }
        })
        .collect();
    hits.sort_by_key(|(rest, date, _)| (*rest, *date));
    hits.into_iter()
        .take(ANNIVERSARY_MAX_LINES)
        .map(|(_, _, line)| line)
        .collect()
}

/// The first meeting's line for `today`, with the date it falls on. Months
/// 1..=11 render on the day only; from 12 months on the yearly date renders
/// from `LOOKAHEAD_DAYS` ahead. A day past a month's end clamps to its last
/// day (`01-31` + 1 month is `02-28`, `02-29` + 1 year is `02-28`).
fn first_meet_line(met_on: NaiveDate, today: NaiveDate) -> Option<(NaiveDate, String)> {
    let after = |months: u32| met_on.checked_add_months(Months::new(months));
    if let Some(n) = (1..=11).find(|&n| after(n) == Some(today)) {
        return Some((today, format!("今天是你们认识满 {n} 个月的日子。")));
    }
    (0..=u64::from(LOOKAHEAD_DAYS)).find_map(|ahead| {
        let date = today.checked_add_days(Days::new(ahead))?;
        let years = u32::try_from(date.year() - met_on.year())
            .ok()
            .filter(|&y| y >= 1)?;
        if after(12 * years) != Some(date) {
            return None;
        }
        let when = match ahead {
            0 => "今天".to_string(),
            1 => "明天".to_string(),
            n => format!("{n} 天后"),
        };
        Some((date, format!("{when}是你们认识满 {years} 年的日子。")))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn row(
        kind: &str,
        realm: &str,
        recurrence: &str,
        occurred_on: NaiveDate,
        due_on: Option<NaiveDate>,
        summary: &str,
    ) -> JournalRow {
        JournalRow {
            id: Uuid::new_v4(),
            instance_id: Uuid::nil(),
            occurred_on,
            kind: kind.to_string(),
            realm: realm.to_string(),
            summary: summary.to_string(),
            source: "engine".to_string(),
            message_id: None,
            affinity_event_id: None,
            dedup_key: None,
            due_on,
            recurrence: recurrence.to_string(),
            created_at: Utc::now(),
        }
    }

    fn first_meet(occurred_on: NaiveDate) -> JournalRow {
        row(
            "first_meet",
            "real",
            "monthly_then_yearly",
            occurred_on,
            None,
            "你们第一次聊天",
        )
    }

    fn due(on: NaiveDate, summary: &str) -> JournalRow {
        row(
            "promise",
            "real",
            "on_due",
            d(2026, 1, 1),
            Some(on),
            summary,
        )
    }

    #[test]
    fn monthly_renders_on_the_day_only() {
        let rows = [first_meet(d(2026, 1, 31))];
        // 01-31 plus one month clamps to the last day of February.
        assert_eq!(
            anniversary_lines(&rows, Some(d(2026, 2, 28))),
            ["今天是你们认识满 1 个月的日子。"]
        );
        assert!(anniversary_lines(&rows, Some(d(2026, 3, 2))).is_empty());
        assert!(anniversary_lines(&rows, Some(d(2026, 2, 27))).is_empty());
        // Month two is the 31st again.
        assert_eq!(
            anniversary_lines(&rows, Some(d(2026, 3, 31))),
            ["今天是你们认识满 2 个月的日子。"]
        );
    }

    #[test]
    fn yearly_uses_six_day_window() {
        let rows = [first_meet(d(2025, 3, 10))];
        let on = |y, m, day| anniversary_lines(&rows, Some(d(y, m, day)));
        assert_eq!(on(2026, 3, 7), ["3 天后是你们认识满 1 年的日子。"]);
        assert_eq!(on(2026, 3, 4), ["6 天后是你们认识满 1 年的日子。"]);
        assert_eq!(on(2026, 3, 9), ["明天是你们认识满 1 年的日子。"]);
        assert!(on(2026, 3, 3).is_empty(), "seven days ahead is outside");
        assert!(on(2026, 3, 11).is_empty(), "the day is past");
        // The window crosses a year boundary.
        let new_year = [first_meet(d(2024, 1, 2))];
        assert_eq!(
            anniversary_lines(&new_year, Some(d(2025, 12, 30))),
            ["3 天后是你们认识满 2 年的日子。"]
        );
    }

    #[test]
    fn month_twelve_switches_to_yearly() {
        let rows = [first_meet(d(2025, 3, 10))];
        let lines = anniversary_lines(&rows, Some(d(2026, 3, 10)));
        assert_eq!(lines, ["今天是你们认识满 1 年的日子。"]);
        assert!(!lines[0].contains("12 个月"));
        // Month eleven is still monthly.
        assert_eq!(
            anniversary_lines(&rows, Some(d(2026, 2, 10))),
            ["今天是你们认识满 11 个月的日子。"]
        );
        // After the first year only the yearly dates render.
        assert!(anniversary_lines(&rows, Some(d(2026, 4, 10))).is_empty());
        assert_eq!(
            anniversary_lines(&rows, Some(d(2027, 3, 10))),
            ["今天是你们认识满 2 年的日子。"]
        );
    }

    #[test]
    fn leap_day_yearly_clamps_to_feb_28() {
        let rows = [first_meet(d(2024, 2, 29))];
        let on = |y, m, day| anniversary_lines(&rows, Some(d(y, m, day)));
        assert_eq!(on(2025, 2, 28), ["今天是你们认识满 1 年的日子。"]);
        assert_eq!(on(2027, 2, 28), ["今天是你们认识满 3 年的日子。"]);
        assert_eq!(on(2028, 2, 29), ["今天是你们认识满 4 年的日子。"]);
        assert_eq!(on(2028, 2, 28), ["明天是你们认识满 4 年的日子。"]);
        // Month one of a leap-day meeting is the 29th of March.
        assert_eq!(on(2024, 3, 29), ["今天是你们认识满 1 个月的日子。"]);
    }

    #[test]
    fn due_renders_on_the_day() {
        let today = d(2026, 9, 14);
        assert_eq!(
            anniversary_lines(&[due(today, "去海边")], Some(today)),
            ["今天是你们约好的日子：去海边。"]
        );
        // A summary that already ends in 。 does not double it.
        assert_eq!(
            anniversary_lines(&[due(today, "去海边。")], Some(today)),
            ["今天是你们约好的日子：去海边。"]
        );
        assert!(anniversary_lines(&[due(d(2026, 9, 15), "去海边")], Some(today)).is_empty());
        assert!(anniversary_lines(&[due(d(2026, 9, 13), "去海边")], Some(today)).is_empty());
    }

    #[test]
    fn capped_at_two_lines_first_meet_first() {
        let today = d(2026, 2, 28);
        // The due rows come first in the input; the first meeting still leads.
        let rows = [
            due(today, "去海边"),
            due(today, "看电影"),
            first_meet(d(2026, 1, 31)),
        ];
        assert_eq!(ANNIVERSARY_MAX_LINES, 2);
        assert_eq!(
            anniversary_lines(&rows, Some(today)),
            [
                "今天是你们认识满 1 个月的日子。".to_string(),
                "今天是你们约好的日子：去海边。".to_string(),
            ]
        );
    }

    #[test]
    fn anniversary_needs_request_timezone() {
        let rows = [first_meet(d(2026, 1, 31)), due(d(2026, 2, 28), "去海边")];
        assert!(anniversary_lines(&rows, None).is_empty());
    }

    #[test]
    fn roleplay_rows_never_render() {
        let today = d(2026, 2, 28);
        let rows = [
            row(
                "first_meet",
                "roleplay",
                "monthly_then_yearly",
                d(2026, 1, 31),
                None,
                "扮演",
            ),
            row(
                "promise",
                "roleplay",
                "on_due",
                d(2026, 1, 1),
                Some(today),
                "扮演约会",
            ),
        ];
        assert!(anniversary_lines(&rows, Some(today)).is_empty());
    }
}

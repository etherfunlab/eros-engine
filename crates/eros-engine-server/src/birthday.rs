// SPDX-License-Identifier: AGPL-3.0-only
//! The persona's birthday: `art_metadata.birthday` as `"MM-DD"`, no year.
//! Feeds the `[now]` birthday line and the engine-decided birthday greeting.
//!
//! Spec: docs/superpowers/specs/2026-10-10-persona-origin-and-birthday-design.md §3, §4.4

use chrono::{Datelike, Days, NaiveDate};

/// A month-day. `02-29` is kept as written; [`Birthday::falls_on`] folds it
/// to `02-28` in a common year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Birthday {
    month: u32,
    day: u32,
}

impl Birthday {
    /// `"MM-DD"`, zero-padded, a real day in a leap year; surrounding
    /// whitespace ignored. Anything else is no birthday.
    pub fn parse(s: &str) -> Option<Self> {
        let (m, d) = s.trim().split_once('-')?;
        let two_digits = |p: &str| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit());
        if !two_digits(m) || !two_digits(d) {
            return None;
        }
        let (month, day) = (m.parse().ok()?, d.parse().ok()?);
        // 2000 is a leap year, so 02-29 is accepted.
        NaiveDate::from_ymd_opt(2000, month, day)?;
        Some(Self { month, day })
    }

    /// Whether `date` is this birthday. `02-29` falls on `02-28` in a common year.
    pub fn falls_on(self, date: NaiveDate) -> bool {
        if (date.month(), date.day()) == (self.month, self.day) {
            return true;
        }
        (self.month, self.day) == (2, 29)
            && (date.month(), date.day()) == (2, 28)
            && NaiveDate::from_ymd_opt(date.year(), 2, 29).is_none()
    }

    /// Days from `today` to the next birthday, when it falls within
    /// `0..=window`.
    pub fn days_until(self, today: NaiveDate, window: u8) -> Option<u8> {
        (0..=window).find(|&ahead| {
            today
                .checked_add_days(Days::new(u64::from(ahead)))
                .is_some_and(|date| self.falls_on(date))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn parses_zero_padded_real_month_days_only() {
        assert_eq!(
            Birthday::parse("09-25"),
            Some(Birthday { month: 9, day: 25 })
        );
        assert_eq!(
            Birthday::parse(" 02-29 "),
            Some(Birthday { month: 2, day: 29 })
        );
        for bad in [
            "",
            "  ",
            "9-25",
            "09-5",
            "13-01",
            "00-10",
            "09-00",
            "02-30",
            "04-31",
            "09/25",
            "0925",
            "2026-09-25",
            "+9-25",
            "ab-cd",
            "09-25-",
        ] {
            assert_eq!(Birthday::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn falls_on_its_own_day_and_feb_29_folds_to_feb_28_in_common_years() {
        let b = Birthday::parse("09-25").unwrap();
        assert!(b.falls_on(d(2026, 9, 25)));
        assert!(!b.falls_on(d(2026, 9, 24)));

        let leap = Birthday::parse("02-29").unwrap();
        assert!(leap.falls_on(d(2028, 2, 29)));
        assert!(
            !leap.falls_on(d(2028, 2, 28)),
            "a leap year has the real day"
        );
        assert!(leap.falls_on(d(2027, 2, 28)));
        assert!(!leap.falls_on(d(2027, 3, 1)));
    }

    #[test]
    fn days_until_counts_within_the_window_and_across_new_year() {
        let b = Birthday::parse("01-02").unwrap();
        assert_eq!(b.days_until(d(2026, 1, 2), 6), Some(0));
        assert_eq!(b.days_until(d(2026, 1, 1), 6), Some(1));
        assert_eq!(b.days_until(d(2025, 12, 30), 6), Some(3));
        assert_eq!(b.days_until(d(2025, 12, 27), 6), Some(6));
        assert_eq!(b.days_until(d(2025, 12, 26), 6), None, "seven days out");
        assert_eq!(b.days_until(d(2026, 1, 3), 6), None, "just passed");

        let leap = Birthday::parse("02-29").unwrap();
        assert_eq!(
            leap.days_until(d(2027, 2, 26), 6),
            Some(2),
            "folds to 02-28"
        );
    }
}

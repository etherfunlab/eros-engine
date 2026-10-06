// SPDX-License-Identifier: AGPL-3.0-only
//! The user's holidays: lunar festivals (`tyme4rs`), public holidays
//! (`py-holidays-rs`) without political days, and a four-row table of
//! fixed-date observances neither crate carries. Feeds the `[now]` holiday
//! line and the holiday greeting.
//!
//! Spec: docs/superpowers/specs/2026-09-26-user-locale-and-holiday-greeting-design.md §3

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Datelike, Days, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use py_holidays_rs::{CountryCode, SubDivision};
use tyme4rs::tyme::solar::SolarDay;
use tyme4rs::tyme::Culture;

/// Days after today the window covers (spec Decision 6).
pub const LOOKAHEAD_DAYS: u8 = 6;

enum Who {
    Everyone,
    /// Only when no name already on that date contains this text.
    Unless(&'static str),
}

/// Fixed-date observances neither crate carries (spec §3.1): month, day,
/// name, and when it applies. The `Unless` rows fill in where the country's
/// calendar has no New Year or Christmas of its own.
const FIXED: &[(u32, u32, &str, Who)] = &[
    (1, 1, "新年快乐", Who::Unless("New Year")),
    (2, 14, "情人节", Who::Everyone),
    (12, 24, "平安夜", Who::Everyone),
    (12, 25, "Season's Greetings", Who::Unless("Christmas")),
];

/// Label fragments that mark a python-holidays day as political: national,
/// founding, war, revolution and remembrance days, elections. A persona's
/// holidays are everyday ones only, and a country guessed from IP or timezone
/// must not hand the user another nation's day.
const POLITICAL: &[&str] = &[
    "National Day",    // CN, TW, HK, SG, FR, …; "National Day of Mourning" (IE)
    "Independence",    // US, IN, MX, …; "Independence Movement Day" (KR)
    "Republic",        // "Republic Day" (IN, TR); "Founding Day of the Republic of China" (TW)
    "Founding",        // "State Founding Day" (BR)
    "Foundation Day",  // KR, JP, IN states
    "Constitution",    // TW 12-25, JP, NO, UA, …
    "Unity",           // "German Unity Day" (DE), "Unity Day" (RU)
    "Unification",     // BG, RO
    "Statehood",       // HR, SI, RS, CZ, …
    "Revolution",      // MX, EG, BY, IR, …
    "Liberation",      // KR 08-15, NL, VN, …
    "Victory",         // RU, CZ, …; the War of Resistance victory day (CN, HK)
    "Restoration",     // "Taiwan Restoration and Guningtou Victory Memorial Day" (TW)
    "Memorial",        // "Peace Memorial Day" (TW 02-28), "Memorial Day" (US, KR)
    "Remembrance",     // CA, HR, PG, …
    "Mourning",        // BD, PA, TH, …
    "Armistice",       // FR, BE, RS
    "Martyrs",         // TN, BF, PA, …
    "Heroes",          // PH, JM, ZW, …
    "Defender",        // RU, KZ, UA, KG
    "Armed Forces",    // KR, EG, AZ, …
    "Proclamation",    // AU, BR, LV
    "Establishment",   // "Hong Kong S.A.R. Establishment Day" (HK)
    "Sovereignty",     // RU, AR, TR
    "Election",        // KR, ID, US, TH, …
    "Flag",            // AZ, SZ, PE, …
    "Canada Day",      // CA
    "Australia Day",   // AU
    "Russia Day",      // RU
    "Waitangi Day",    // NZ
    "Malaysia Day",    // MY
    "Day of Portugal", // PT
];

/// The user's locale as one request carried it, parsed (spec §3.2).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UserLocale {
    pub timezone: Option<Tz>,
    pub country: Option<CountryCode>,
    pub region: Option<SubDivision>,
}

impl UserLocale {
    /// An unparseable timezone is treated as absent (one WARN). A country the
    /// client named is never overridden by the timezone; only an absent one
    /// falls back to it. A region counts only beside a named country. Blank
    /// strings are absent.
    pub fn resolve(timezone: Option<&str>, country: Option<&str>, region: Option<&str>) -> Self {
        let timezone = timezone
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| match s.parse::<Tz>() {
                Ok(tz) => Some(tz),
                Err(_) => {
                    let shown: String = s.chars().take(MAX_RAW_LEN).collect();
                    tracing::warn!(
                        user_timezone = shown.as_str(),
                        "unparseable user_timezone; treated as absent"
                    );
                    None
                }
            });
        let (country, region) = match country.map(str::trim).filter(|s| !s.is_empty()) {
            Some(code) => {
                let country = parse_code::<CountryCode>(code);
                (country, country.and(region).and_then(parse_region))
            }
            None => (timezone.and_then(country_of), None),
        };
        Self {
            timezone,
            country,
            region,
        }
    }

    /// The user's calendar date at `now`; `None` without a timezone.
    pub fn local_date(&self, now: DateTime<Utc>) -> Option<NaiveDate> {
        self.timezone.map(|tz| now.with_timezone(&tz).date_naive())
    }
}

/// The crate enums deserialize from their ISO codes.
fn parse_code<T: serde::de::DeserializeOwned>(code: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(code.to_string())).ok()
}

/// `SubDivision` prefixes digit-leading codes with `_` (`01` → `_01`).
fn parse_region(code: &str) -> Option<SubDivision> {
    let code = code.trim();
    if code.starts_with(|c: char| c.is_ascii_digit()) {
        parse_code(&format!("_{code}"))
    } else {
        parse_code(code)
    }
}

/// `iso-rs` lists each IANA zone under exactly one country. Legacy aliases
/// (`Asia/Calcutta`) are not listed and yield `None`.
fn country_of(tz: Tz) -> Option<CountryCode> {
    static BY_ZONE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    let by_zone = BY_ZONE.get_or_init(|| {
        iso_rs::NAMES
            .values()
            .flat_map(|c| {
                c.timezones
                    .iter()
                    .map(move |t| (t.iana_identifier, c.alpha_2))
            })
            .collect()
    });
    by_zone.get(tz.name()).and_then(|alpha2| parse_code(alpha2))
}

type CountryMap = BTreeMap<SubDivision, BTreeMap<NaiveDate, String>>;

/// Loads each country once. The crate's own accessor clones the whole
/// per-country map on every call (spec §3.3).
fn country_map(country: CountryCode) -> Option<Arc<CountryMap>> {
    static CACHE: OnceLock<Mutex<HashMap<CountryCode, Arc<CountryMap>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(m) = cache.lock().unwrap().get(&country) {
        return Some(m.clone());
    }
    let loaded = match py_holidays_rs::get_holidays_by_country(country) {
        Ok(m) => Arc::new(m),
        Err(e) => {
            tracing::warn!(?country, error = %e, "holiday data load failed");
            return None;
        }
    };
    cache.lock().unwrap().insert(country, loaded.clone());
    Some(loaded)
}

/// Label fragments python-holidays uses for an in-lieu day off: a day that
/// stands in for a holiday or bridges to one, while the holiday itself keeps
/// its own label (spec §3.1). `(estimated)` is not one of them.
const IN_LIEU: &[&str] = &[
    "(observed",               // "X (observed)": moved off a weekend (US, AU, GB, …)
    "(in lieu)",               // "X (in lieu)" (TH, LA)
    "In Lieu",                 // "Special In Lieu Holiday" (TH)
    "Substitute Holiday",      // JP
    "Alternative holiday for", // "Alternative holiday for X" (KR)
    "Replacement Holiday",     // "Khmer New Year's Replacement Holiday" (KH)
    "Bridge Public Holiday",   // AR, TH
    "Day off",                 // "Day off (substituted from …)" (CN, RU, …); "Day off for X" (AO)
    "day off",                 // "Additional day off by Presidential decree" (UZ)
];

fn is_in_lieu(name: &str) -> bool {
    IN_LIEU.iter().any(|label| name.contains(label))
}

fn is_political(name: &str) -> bool {
    POLITICAL.iter().any(|label| name.contains(label))
}

/// Holiday names on `date`, in spec §3.1 order: lunar festival, public
/// holidays, fixed-date observances. Exact duplicates removed.
pub fn holidays_on(
    date: NaiveDate,
    country: Option<CountryCode>,
    region: Option<SubDivision>,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let push = |names: &mut Vec<String>, n: &str| {
        if !names.iter().any(|x| x == n) {
            names.push(n.to_string());
        }
    };
    let solar = SolarDay::from_ymd(
        date.year() as isize,
        date.month() as usize,
        date.day() as usize,
    );
    if let Some(festival) = solar.get_lunar_day().get_festival() {
        push(&mut names, &festival.get_name());
    }
    if let Some(map) = country.and_then(country_map) {
        let by_date = region
            .and_then(|r| map.get(&r))
            .or_else(|| map.get(&SubDivision::National));
        if let Some(entry) = by_date.and_then(|m| m.get(&date)) {
            // python-holidays joins same-date holidays with "; ".
            for part in entry
                .split("; ")
                .filter(|p| !is_in_lieu(p) && !is_political(p))
            {
                push(&mut names, part);
            }
        }
    }
    for (month, day, name, who) in FIXED {
        let applies = match who {
            Who::Everyone => true,
            Who::Unless(text) => !names.iter().any(|n| n.contains(*text)),
        };
        if applies && date.month() == *month && date.day() == *day {
            push(&mut names, name);
        }
    }
    names
}

/// Holidays on each of the user's local days `today ..= today + LOOKAHEAD_DAYS`,
/// as `(days_ahead, names)`, skipping days with none. Empty without a timezone.
pub fn upcoming(locale: &UserLocale, now: DateTime<Utc>) -> Vec<(u8, Vec<String>)> {
    let Some(today) = locale.local_date(now) else {
        return Vec::new();
    };
    (0..=LOOKAHEAD_DAYS)
        .filter_map(|ahead| {
            let date = today.checked_add_days(Days::new(u64::from(ahead)))?;
            let names = holidays_on(date, locale.country, locale.region);
            (!names.is_empty()).then_some((ahead, names))
        })
        .collect()
}

/// UTC instant of the first valid local hour on `date` in `tz` — midnight,
/// or the first hour after a DST gap that swallows it.
pub fn local_midnight_utc(tz: Tz, date: NaiveDate) -> DateTime<Utc> {
    (0..24)
        .find_map(|h| {
            tz.from_local_datetime(&date.and_hms_opt(h, 0, 0)?)
                .earliest()
        })
        .expect("every local day has a valid hour")
        .with_timezone(&Utc)
}

/// Longest raw locale value copied into row metadata. Anything longer is not
/// a timezone or ISO code, so it stays out of the audit copy; the unparseable
/// timezone WARN logs at most this many characters of it.
const MAX_RAW_LEN: usize = 64;

/// Trim and bound-check a raw locale value the same way `record_raw` does:
/// `None` when absent, blank, or longer than `MAX_RAW_LEN`. Used wherever a
/// raw locale string is persisted outside a row's metadata — e.g. the chat
/// queue's `QueuedTurnParams` snapshot — so an oversized value never reaches
/// storage there either; "invalid locale ⇒ absent" applies in one place.
pub fn bounded_raw(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.len() <= MAX_RAW_LEN)
        .map(str::to_string)
}

/// Copy a request's raw locale fields into a row's metadata (spec §4.1) so a
/// replay can rebuild the prompt. Absent, blank and overlong values are skipped.
pub fn record_raw(
    meta: &mut serde_json::Map<String, serde_json::Value>,
    timezone: Option<&str>,
    country: Option<&str>,
    region: Option<&str>,
) {
    for (key, value) in [
        ("user_timezone", timezone),
        ("user_country", country),
        ("user_region", region),
    ] {
        if let Some(v) = bounded_raw(value) {
            meta.insert(key.into(), serde_json::json!(v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_raw_copies_present_values_and_skips_blank_or_overlong_ones() {
        let mut m = serde_json::Map::new();
        record_raw(&mut m, Some("Asia/Taipei"), Some(""), Some(&"x".repeat(65)));
        assert_eq!(
            m.get("user_timezone"),
            Some(&serde_json::json!("Asia/Taipei"))
        );
        assert!(!m.contains_key("user_country"));
        assert!(!m.contains_key("user_region"));
        let mut empty = serde_json::Map::new();
        record_raw(&mut empty, None, None, None);
        assert!(empty.is_empty());
    }

    #[test]
    fn bounded_raw_applies_the_same_rule_as_record_raw() {
        assert_eq!(bounded_raw(None), None);
        assert_eq!(bounded_raw(Some("")), None);
        assert_eq!(bounded_raw(Some("   ")), None, "blank");
        assert_eq!(bounded_raw(Some(&"x".repeat(65))), None, "overlong (65)");
        let sixty_four = "x".repeat(64);
        assert_eq!(
            bounded_raw(Some(&sixty_four)),
            Some(sixty_four),
            "64 chars kept"
        );
        assert_eq!(
            bounded_raw(Some("  Asia/Taipei  ")),
            Some("Asia/Taipei".to_string()),
            "trimmed"
        );
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn cc(code: &str) -> Option<CountryCode> {
        parse_code(code)
    }

    #[test]
    fn mid_autumn_2026_reaches_every_country_and_none() {
        assert_eq!(holidays_on(d(2026, 9, 25), None, None), vec!["中秋节"]);
        assert_eq!(holidays_on(d(2026, 9, 25), cc("US"), None), vec!["中秋节"]);
        assert_eq!(
            holidays_on(d(2026, 9, 25), cc("CN"), None),
            vec!["中秋节", "Mid-Autumn Festival"]
        );
    }

    #[test]
    fn lunar_festivals_come_from_tyme4rs() {
        assert_eq!(holidays_on(d(2026, 8, 19), None, None), vec!["七夕节"]);
        assert_eq!(holidays_on(d(2027, 2, 5), None, None), vec!["除夕"]);
        assert_eq!(holidays_on(d(2027, 2, 6), None, None), vec!["春节"]);
    }

    #[test]
    fn subdivision_map_carries_national_holidays() {
        assert_eq!(
            holidays_on(d(2026, 11, 26), cc("US"), parse_region("CA")),
            vec!["Thanksgiving Day"]
        );
    }

    #[test]
    fn a_region_the_country_lacks_falls_back_to_national() {
        assert!(parse_region("ENG").is_some());
        assert_eq!(
            holidays_on(d(2026, 11, 26), cc("US"), parse_region("ENG")),
            vec!["Thanksgiving Day"]
        );
    }

    #[test]
    fn digit_leading_regions_parse_through_the_underscore_prefix() {
        assert_eq!(parse_region("1"), Some(SubDivision::_1));
    }

    #[test]
    fn in_lieu_days_are_dropped() {
        // US: "Independence Day (observed)"; CN: "Day off (substituted from 01/04/2026)".
        assert!(holidays_on(d(2026, 7, 3), cc("US"), None).is_empty());
        assert!(holidays_on(d(2026, 1, 2), cc("CN"), None).is_empty());
    }

    #[test]
    fn substitute_alternative_and_in_lieu_days_are_dropped() {
        let none: Vec<String> = Vec::new();
        // JP "Substitute Holiday".
        assert_eq!(holidays_on(d(2026, 5, 6), cc("JP"), None), none);
        // KR "Alternative holiday for Independence Movement Day".
        assert_eq!(holidays_on(d(2026, 3, 2), cc("KR"), None), none);
        // TH "Visakha Bucha (in lieu)".
        assert_eq!(holidays_on(d(2026, 6, 1), cc("TH"), None), none);
    }

    #[test]
    fn every_other_in_lieu_label_family_is_dropped() {
        let none: Vec<String> = Vec::new();
        for (code, date, label) in [
            (
                "TH",
                d(2026, 12, 7),
                "three `(in lieu)` parts joined with `; `",
            ),
            ("TH", d(2000, 1, 3), "Special In Lieu Holiday"),
            ("TH", d(2026, 1, 2), "Bridge Public Holiday"),
            ("AR", d(2025, 11, 21), "Bridge Public Holiday"),
            ("AO", d(2026, 1, 2), "Day off for New Year's Day"),
            (
                "UZ",
                d(2024, 12, 31),
                "Additional day off by Presidential decree",
            ),
            ("KH", d(2020, 8, 18), "Khmer New Year's Replacement Holiday"),
        ] {
            assert_eq!(
                holidays_on(date, cc(code), None),
                none,
                "{code} {date}: {label}"
            );
        }
    }

    #[test]
    fn a_joined_entry_keeps_the_real_holiday_and_drops_the_in_lieu_part() {
        // AU-NT 2033-12-26: "Boxing Day; Christmas Day (observed)".
        assert_eq!(
            holidays_on(d(2033, 12, 26), cc("AU"), parse_region("NT")),
            vec!["Boxing Day"]
        );
    }

    #[test]
    fn fixed_observances_reach_every_country() {
        let has = |date: NaiveDate, c: Option<CountryCode>, name: &str| {
            holidays_on(date, c, None).iter().any(|n| n == name)
        };
        for c in [None, cc("US"), cc("CN"), cc("TW")] {
            assert!(has(d(2027, 2, 14), c, "情人节"));
            assert!(has(d(2026, 12, 24), c, "平安夜"));
        }
    }

    #[test]
    fn political_days_are_dropped() {
        let none: Vec<String> = Vec::new();
        for (code, date, label) in [
            ("CN", d(2026, 10, 1), "National Day"),
            ("TW", d(2026, 10, 10), "National Day"),
            ("US", d(2026, 7, 4), "Independence Day"),
            ("IN", d(2027, 1, 26), "Republic Day"),
            ("KR", d(2026, 10, 3), "National Foundation Day"),
            ("JP", d(2026, 2, 11), "Foundation Day"),
            ("DE", d(2026, 10, 3), "German Unity Day"),
            ("KR", d(2026, 3, 1), "Independence Movement Day"),
            ("KR", d(2026, 8, 15), "Liberation Day"),
            ("TW", d(2026, 2, 28), "Peace Memorial Day"),
            (
                "TW",
                d(2026, 10, 25),
                "Taiwan Restoration and Guningtou Victory Memorial Day",
            ),
            ("CN", d(2015, 9, 3), "70th Anniversary of the Victory of …"),
            ("HK", d(2026, 7, 1), "Hong Kong S.A.R. Establishment Day"),
            ("RU", d(2026, 5, 9), "Victory Day"),
            ("US", d(2026, 5, 25), "Memorial Day"),
        ] {
            assert_eq!(
                holidays_on(date, cc(code), None),
                none,
                "{code} {date}: {label}"
            );
        }
        for c in [None, cc("CN"), cc("TW"), cc("US")] {
            assert_eq!(holidays_on(d(2026, 10, 9), c, None), none);
            assert_eq!(holidays_on(d(2026, 10, 10), c, None), none);
            assert_eq!(holidays_on(d(2026, 10, 25), c, None), none);
        }
    }

    #[test]
    fn new_year_and_christmas_fill_in_only_where_the_calendar_has_none() {
        // TW's 01-01 (Founding Day of the Republic of China) and 12-25
        // (Constitution Day) are dropped; CN has no Christmas.
        assert_eq!(holidays_on(d(2027, 1, 1), cc("TW"), None), vec!["新年快乐"]);
        assert_eq!(holidays_on(d(2027, 1, 1), None, None), vec!["新年快乐"]);
        assert_eq!(
            holidays_on(d(2027, 1, 1), cc("CN"), None),
            vec!["New Year's Day"]
        );
        for c in [None, cc("TW"), cc("CN")] {
            assert_eq!(
                holidays_on(d(2026, 12, 25), c, None),
                vec!["Season's Greetings"]
            );
        }
        assert_eq!(
            holidays_on(d(2026, 12, 25), cc("US"), None),
            vec!["Christmas Day"]
        );
    }

    #[test]
    fn country_falls_back_to_the_timezone_only_when_absent() {
        assert_eq!(
            UserLocale::resolve(Some("Asia/Taipei"), None, None).country,
            cc("TW")
        );
        assert_eq!(
            UserLocale::resolve(Some("Asia/Taipei"), Some("XX"), None).country,
            None
        );
        assert_eq!(
            UserLocale::resolve(Some("Asia/Calcutta"), None, None).country,
            None
        );
    }

    #[test]
    fn a_region_counts_only_beside_a_named_country() {
        assert_eq!(
            UserLocale::resolve(Some("America/Los_Angeles"), None, Some("CA")).region,
            None
        );
        assert_eq!(
            UserLocale::resolve(None, Some("US"), Some("CA")).region,
            parse_region("CA")
        );
        assert_eq!(
            UserLocale::resolve(None, Some("US"), Some("ZZZ")).region,
            None
        );
    }

    #[test]
    fn blank_strings_are_absent() {
        assert_eq!(
            UserLocale::resolve(Some(""), Some(" "), Some("")),
            UserLocale::default()
        );
        // A blank country is absent, so the timezone still supplies one.
        assert_eq!(
            UserLocale::resolve(Some("Asia/Taipei"), Some(""), None).country,
            cc("TW")
        );
    }

    #[test]
    fn an_unparseable_timezone_is_absent_and_yields_no_holidays() {
        let l = UserLocale::resolve(Some("Not/AZone"), Some("CN"), None);
        assert_eq!(l.timezone, None);
        assert_eq!(l.country, cc("CN"));
        let now = Utc.with_ymd_and_hms(2026, 9, 25, 4, 0, 0).unwrap();
        assert!(upcoming(&l, now).is_empty());
    }

    #[test]
    fn upcoming_counts_days_in_the_users_zone_across_a_month_boundary() {
        // 2026-04-30 04:00 UTC = 12:00 in Shanghai. CN Labor Day 05-01..02;
        // 05-04 "(observed)" drops out.
        let l = UserLocale::resolve(Some("Asia/Shanghai"), None, None);
        let now = Utc.with_ymd_and_hms(2026, 4, 30, 4, 0, 0).unwrap();
        let days: Vec<u8> = upcoming(&l, now)
            .into_iter()
            .map(|(ahead, _)| ahead)
            .collect();
        assert_eq!(days, vec![1, 2]);
    }

    #[test]
    fn the_users_date_not_utc_decides_today() {
        // 2026-09-24 20:00 UTC is already 09-25 (中秋) in Taipei, still 09-24 in LA.
        let now = Utc.with_ymd_and_hms(2026, 9, 24, 20, 0, 0).unwrap();
        let taipei = UserLocale::resolve(Some("Asia/Taipei"), Some("TW"), None);
        assert_eq!(
            upcoming(&taipei, now)[0],
            (
                0,
                vec!["中秋节".to_string(), "Mid-Autumn Festival".to_string()]
            )
        );
        let la = UserLocale::resolve(Some("America/Los_Angeles"), Some("US"), None);
        assert_eq!(upcoming(&la, now)[0], (1, vec!["中秋节".to_string()]));
    }

    #[test]
    fn local_midnight_is_the_users_not_utc() {
        assert_eq!(
            local_midnight_utc(chrono_tz::Asia::Taipei, d(2026, 9, 25)),
            Utc.with_ymd_and_hms(2026, 9, 24, 16, 0, 0).unwrap()
        );
    }

    #[test]
    fn a_dst_gap_at_midnight_moves_to_the_first_valid_hour() {
        // Chile springs forward at local midnight on 2026-09-06: 00:00 does not exist.
        assert_eq!(
            local_midnight_utc(chrono_tz::America::Santiago, d(2026, 9, 6)),
            Utc.with_ymd_and_hms(2026, 9, 6, 4, 0, 0).unwrap()
        );
    }
}

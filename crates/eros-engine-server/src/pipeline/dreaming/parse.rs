// SPDX-License-Identifier: AGPL-3.0-only
//! The scan's model output: parsing, journal validation, and the arc guard.

use chrono::NaiveDate;
use chrono_tz::Tz;
use eros_engine_store::episode::ScanRow;
use eros_engine_store::journal::JournalInsert;
use serde::Deserialize;
use uuid::Uuid;

use super::MemoryCandidate;

// Episode summaries are cut by the store on write (`episode::EPISODE_MAX_CHARS`).
pub(crate) const OPENING_MAX_CHARS: usize = 150;
pub(crate) const ARC_MAX_CHARS: usize = 900;
/// A rewrite shorter than this share of the previous arc is a collapse.
pub(crate) const ARC_SHRINK_FLOOR: f64 = 0.7;
/// Shrink is only judged against a previous arc longer than this.
pub(crate) const ARC_SHRINK_MIN_PREVIOUS: usize = 300;
pub(crate) const JOURNAL_SUMMARY_MAX_CHARS: usize = 200;

/// The journal kinds the model may write. `first_meet` and `tier_reached` are
/// the engine's own.
const LLM_KINDS: [&str; 8] = [
    "confession",
    "first_intimacy",
    "conflict",
    "reconciliation",
    "promise",
    "gift",
    "shared_plan",
    "user_shared_milestone",
];

#[derive(Debug)]
pub(crate) struct ScanOutput {
    pub memories: Vec<MemoryCandidate>,
    pub episode: Option<String>,
    pub journal: Vec<JournalCandidate>,
}

/// One journal moment as the model wrote it, before validation.
#[derive(Debug, Deserialize)]
pub(crate) struct JournalCandidate {
    pub kind: String,
    #[serde(default)]
    pub realm: Option<String>,
    pub occurred_at: String,
    pub summary: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub due: Option<String>,
}

/// `None` when the reply holds no JSON object (a parse error to the caller).
/// Fenced or prose-wrapped replies are read through their `{…}` block. Absent
/// `memories` / `journal` are empty; an unusable item is skipped, not fatal.
pub(crate) fn parse_scan_output(raw: &str) -> Option<ScanOutput> {
    let v = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .filter(serde_json::Value::is_object)
        .or_else(|| {
            crate::pipeline::find_json_block(raw)
                .and_then(|block| serde_json::from_str::<serde_json::Value>(block).ok())
        })?;
    let journal = v
        .get("journal")
        .and_then(|j| j.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|x| serde_json::from_value::<JournalCandidate>(x.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    Some(ScanOutput {
        memories: super::extract_memory_array(&v),
        episode: episode_text(v.get("episode")),
        journal,
    })
}

/// The episode as a trimmed string; a model that wraps it in an object with a
/// `summary` key is read the same. Blank is `None`.
fn episode_text(v: Option<&serde_json::Value>) -> Option<String> {
    let v = v?;
    let s = v.as_str().or_else(|| v.get("summary")?.as_str())?.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// The candidates that survive validation, as rows ready to embed and insert.
/// `rows` is the stretch the transcript was rendered from: a date must be the
/// local date of one of its stamps, and `#n` names `rows[n - 1]`.
pub(crate) fn validate_journal(
    cands: &[JournalCandidate],
    rows: &[ScanRow],
    tz: Tz,
    instance_id: Uuid,
) -> Vec<JournalInsert> {
    cands
        .iter()
        .filter_map(|c| {
            let kind = c.kind.trim();
            if !LLM_KINDS.contains(&kind) {
                return None;
            }
            let occurred_on = parse_date(&c.occurred_at)?;
            if !rows
                .iter()
                .any(|r| r.sent_at.with_timezone(&tz).date_naive() == occurred_on)
            {
                return None;
            }
            let summary: String = c
                .summary
                .trim()
                .chars()
                .take(JOURNAL_SUMMARY_MAX_CHARS)
                .collect();
            if summary.is_empty() {
                return None;
            }
            let realm = match c.realm.as_deref().map(str::trim) {
                Some("real") => "real",
                _ => "roleplay",
            };
            let message_id = c
                .source
                .as_deref()
                .and_then(|s| s.trim().strip_prefix('#'))
                .and_then(|n| n.parse::<usize>().ok())
                .and_then(|n| rows.get(n.checked_sub(1)?))
                .map(|r| r.message_id);
            let due_on = if matches!(kind, "promise" | "shared_plan") {
                c.due.as_deref().and_then(parse_date)
            } else {
                None
            };
            let dedup_key = match (kind, message_id) {
                ("first_intimacy", _) => Some(format!("first_intimacy:{realm}")),
                ("gift", Some(id)) => Some(format!("gift:{id}")),
                _ => None,
            };
            Some(JournalInsert {
                instance_id,
                occurred_on,
                kind: kind.to_string(),
                realm: realm.to_string(),
                summary,
                source: "llm".to_string(),
                message_id,
                affinity_event_id: None,
                dedup_key,
                due_on,
                recurrence: if due_on.is_some() { "on_due" } else { "none" }.to_string(),
                embedding: None,
            })
        })
        .collect()
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

#[derive(Debug, PartialEq)]
pub(crate) enum ArcReject {
    TooLong,
    Shrunk,
    OpeningMissing,
    OpeningTooLong,
}

/// The engine's checks on an arc rewrite, in this order: the new arc over
/// `ARC_MAX_CHARS`; a previous arc over `ARC_SHRINK_MIN_PREVIOUS` chars that
/// the new one undercuts by `ARC_SHRINK_FLOOR`; and, on the first rewrite, a
/// missing (blank) or over-long opening. Lengths count the strings as given.
pub(crate) fn arc_guard(
    previous: Option<&str>,
    new_arc: &str,
    opening: Option<&str>,
    first_rewrite: bool,
) -> Result<(), ArcReject> {
    let new_len = new_arc.chars().count();
    if new_len > ARC_MAX_CHARS {
        return Err(ArcReject::TooLong);
    }
    if let Some(prev_len) = previous.map(|p| p.chars().count()) {
        if prev_len > ARC_SHRINK_MIN_PREVIOUS
            && (new_len as f64) < ARC_SHRINK_FLOOR * prev_len as f64
        {
            return Err(ArcReject::Shrunk);
        }
    }
    if first_rewrite {
        match opening {
            Some(o) if !o.trim().is_empty() => {
                if o.chars().count() > OPENING_MAX_CHARS {
                    return Err(ArcReject::OpeningTooLong);
                }
            }
            _ => return Err(ArcReject::OpeningMissing),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDate, Utc};

    const TZ: Tz = chrono_tz::Asia::Taipei;

    fn row_at(sent_at: &str) -> ScanRow {
        ScanRow {
            message_id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            role: "user".to_string(),
            content: "嗨".to_string(),
            channel: None,
            sent_at: sent_at.parse::<DateTime<Utc>>().unwrap(),
            metadata: serde_json::json!({}),
        }
    }

    /// #1 is 10-01 UTC but already 10-02 in Taipei; #2 is 10-02; #3 is 10-05.
    fn rows() -> Vec<ScanRow> {
        vec![
            row_at("2026-10-01T16:30:00Z"),
            row_at("2026-10-02T03:00:00Z"),
            row_at("2026-10-05T02:00:00Z"),
        ]
    }

    fn cand(kind: &str, occurred_at: &str) -> JournalCandidate {
        JournalCandidate {
            kind: kind.to_string(),
            realm: Some("real".to_string()),
            occurred_at: occurred_at.to_string(),
            summary: "他说了心里话".to_string(),
            source: None,
            due: None,
        }
    }

    fn validate(cands: &[JournalCandidate]) -> Vec<JournalInsert> {
        validate_journal(cands, &rows(), TZ, Uuid::nil())
    }

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn parse_accepts_fenced_json() {
        let raw = "好的：\n```json\n{\"memories\":[{\"content\":\"养了一只猫\",\"category\":\"fact\"}],\
                   \"episode\":\"  两人聊起了猫。 \",\
                   \"journal\":[{\"kind\":\"promise\",\"realm\":\"real\",\"occurred_at\":\"2026-10-02\",\
                   \"summary\":\"答应带他去看海\",\"source\":\"#2\",\"due\":\"2026-12-24\"}]}\n```";
        let out = parse_scan_output(raw).unwrap();
        assert_eq!(out.memories.len(), 1);
        assert_eq!(out.memories[0].content, "养了一只猫");
        assert_eq!(out.episode.as_deref(), Some("两人聊起了猫。"));
        assert_eq!(out.journal.len(), 1);
        assert_eq!(out.journal[0].kind, "promise");
        assert_eq!(out.journal[0].source.as_deref(), Some("#2"));
        assert_eq!(out.journal[0].due.as_deref(), Some("2026-12-24"));
    }

    #[test]
    fn parse_tolerates_missing_and_odd_fields() {
        let out = parse_scan_output(r#"{"memories":[]}"#).unwrap();
        assert!(out.memories.is_empty() && out.episode.is_none() && out.journal.is_empty());

        // Blank episode is no episode; an object carrying `summary` is accepted.
        let out = parse_scan_output(r#"{"episode":"   "}"#).unwrap();
        assert!(out.episode.is_none());
        let out = parse_scan_output(r#"{"episode":{"summary":"聊了天气"}}"#).unwrap();
        assert_eq!(out.episode.as_deref(), Some("聊了天气"));

        // One bad journal item does not take the others with it; null fields are fine.
        let raw = r#"{"journal":[{"kind":"gift"},
                                 {"kind":"conflict","realm":null,"occurred_at":"2026-10-02",
                                  "summary":"吵了一架","source":null,"due":null}]}"#;
        let out = parse_scan_output(raw).unwrap();
        assert_eq!(out.journal.len(), 1);
        assert_eq!(out.journal[0].kind, "conflict");
        assert!(out.journal[0].realm.is_none());
    }

    #[test]
    fn parse_returns_none_when_no_object_parses() {
        assert!(parse_scan_output("sorry, I can't").is_none());
        assert!(parse_scan_output("[1, 2]").is_none());
        assert!(parse_scan_output("").is_none());
    }

    #[test]
    fn journal_drops_unknown_kind_and_folds_realm() {
        let mut odd_realm = cand("conflict", "2026-10-02");
        odd_realm.realm = Some("dream".to_string());
        let mut no_realm = cand("reconciliation", "2026-10-02");
        no_realm.realm = None;
        let cands = vec![
            cand("first_meet", "2026-10-02"),
            cand("tier_reached", "2026-10-02"),
            cand("birthday", "2026-10-02"),
            cand("confession", "2026-10-02"),
            odd_realm,
            no_realm,
        ];
        let out = validate(&cands);
        let got: Vec<(&str, &str)> = out
            .iter()
            .map(|j| (j.kind.as_str(), j.realm.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("confession", "real"),
                ("conflict", "roleplay"),
                ("reconciliation", "roleplay"),
            ]
        );
        let j = &out[0];
        assert_eq!(j.source, "llm");
        assert_eq!(j.summary, "他说了心里话");
        assert_eq!(j.occurred_on, date("2026-10-02"));
        assert_eq!(j.instance_id, Uuid::nil());
        assert!(j.affinity_event_id.is_none() && j.embedding.is_none());
        assert_eq!(
            (j.dedup_key.as_deref(), j.recurrence.as_str()),
            (None, "none")
        );
    }

    #[test]
    fn journal_drops_date_not_on_a_stamp() {
        let cands = vec![
            // A real date no covered row carries.
            cand("confession", "2026-10-03"),
            // The UTC date of row #1, but its stamp reads 10-02 in Taipei.
            cand("confession", "2026-10-01"),
            cand("confession", "昨天"),
            cand("confession", ""),
            cand("confession", "2026-10-05"),
        ];
        let out = validate(&cands);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].occurred_on, date("2026-10-05"));
    }

    #[test]
    fn journal_drops_blank_summary_and_cuts_long_ones() {
        let mut blank = cand("confession", "2026-10-02");
        blank.summary = "  \n ".to_string();
        let mut long = cand("conflict", "2026-10-02");
        long.summary = format!("  {}  ", "吵".repeat(JOURNAL_SUMMARY_MAX_CHARS + 50));
        let out = validate(&[blank, long]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].summary.chars().count(), JOURNAL_SUMMARY_MAX_CHARS);
    }

    #[test]
    fn journal_source_out_of_range_keeps_row_without_message_id() {
        let rows = rows();
        let mut ok = cand("confession", "2026-10-02");
        ok.source = Some("#2".to_string());
        let mut first = cand("confession", "2026-10-02");
        first.source = Some("#1".to_string());
        let bad = ["#9", "#0", "12", "#x", "", "#2 嗨"];
        let mut cands = vec![ok, first];
        for s in bad {
            let mut c = cand("confession", "2026-10-02");
            c.source = Some(s.to_string());
            cands.push(c);
        }
        cands.push(cand("confession", "2026-10-02"));
        let out = validate_journal(&cands, &rows, TZ, Uuid::nil());
        assert_eq!(out.len(), cands.len(), "no row is dropped for its source");
        assert_eq!(out[0].message_id, Some(rows[1].message_id));
        assert_eq!(out[1].message_id, Some(rows[0].message_id));
        assert!(out[2..].iter().all(|j| j.message_id.is_none()));
    }

    #[test]
    fn journal_due_only_for_promise_and_shared_plan() {
        let with_due = |kind: &str, due: &str| {
            let mut c = cand(kind, "2026-10-02");
            c.due = Some(due.to_string());
            c
        };
        let cands = vec![
            with_due("promise", "2026-12-24"),
            with_due("shared_plan", "2027-01-01"),
            with_due("confession", "2026-12-24"),
            with_due("promise", "圣诞节"),
            with_due("promise", ""),
            cand("promise", "2026-10-02"),
        ];
        let out = validate(&cands);
        let got: Vec<(Option<NaiveDate>, &str)> = out
            .iter()
            .map(|j| (j.due_on, j.recurrence.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Some(date("2026-12-24")), "on_due"),
                (Some(date("2027-01-01")), "on_due"),
                (None, "none"),
                (None, "none"),
                (None, "none"),
                (None, "none"),
            ]
        );
    }

    #[test]
    fn journal_gift_dedup_uses_message_id() {
        let rows = rows();
        let mut sourced = cand("gift", "2026-10-02");
        sourced.source = Some("#2".to_string());
        let unsourced = cand("gift", "2026-10-02");
        let mut intimacy = cand("first_intimacy", "2026-10-02");
        let mut roleplay_intimacy = cand("first_intimacy", "2026-10-02");
        roleplay_intimacy.realm = Some("roleplay".to_string());
        intimacy.source = Some("#1".to_string());

        let out = validate_journal(
            &[sourced, unsourced, intimacy, roleplay_intimacy],
            &rows,
            TZ,
            Uuid::nil(),
        );
        let keys: Vec<Option<String>> = out.iter().map(|j| j.dedup_key.clone()).collect();
        assert_eq!(
            keys,
            vec![
                Some(format!("gift:{}", rows[1].message_id)),
                None,
                Some("first_intimacy:real".to_string()),
                Some("first_intimacy:roleplay".to_string()),
            ]
        );
    }

    #[test]
    fn arc_guard_order() {
        let prev_400 = "旧".repeat(400);
        let long = "新".repeat(ARC_MAX_CHARS + 1);
        let at_cap = "新".repeat(ARC_MAX_CHARS);

        // Too long wins over everything below it, shrink and opening included.
        assert_eq!(
            arc_guard(Some(&prev_400), &long, None, true),
            Err(ArcReject::TooLong)
        );
        assert_eq!(arc_guard(None, &at_cap, Some("开场"), true), Ok(()));

        // Shrink: under 70% of a previous arc over 300 chars.
        let shrunk = "新".repeat(279);
        let kept = "新".repeat(280);
        assert_eq!(
            arc_guard(Some(&prev_400), &shrunk, Some("开场"), true),
            Err(ArcReject::Shrunk)
        );
        assert_eq!(arc_guard(Some(&prev_400), &kept, None, false), Ok(()));
        // Shrink beats a missing opening.
        assert_eq!(
            arc_guard(Some(&prev_400), &shrunk, None, true),
            Err(ArcReject::Shrunk)
        );
        // A previous arc of 300 chars or fewer is never judged for shrink.
        let prev_300 = "旧".repeat(300);
        assert_eq!(arc_guard(Some(&prev_300), "短", None, false), Ok(()));

        // First rewrite needs an opening of 1..=150 chars; a later one ignores it.
        assert_eq!(
            arc_guard(None, "新", None, true),
            Err(ArcReject::OpeningMissing)
        );
        assert_eq!(
            arc_guard(None, "新", Some("  \n"), true),
            Err(ArcReject::OpeningMissing)
        );
        let opening_cap = "初".repeat(OPENING_MAX_CHARS);
        let opening_over = "初".repeat(OPENING_MAX_CHARS + 1);
        assert_eq!(arc_guard(None, "新", Some(&opening_cap), true), Ok(()));
        assert_eq!(
            arc_guard(None, "新", Some(&opening_over), true),
            Err(ArcReject::OpeningTooLong)
        );
        assert_eq!(arc_guard(None, "新", Some(&opening_over), false), Ok(()));
        assert_eq!(arc_guard(None, "新", None, false), Ok(()));
    }
}

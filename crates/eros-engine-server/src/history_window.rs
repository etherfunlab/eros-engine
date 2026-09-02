// SPDX-License-Identifier: AGPL-3.0-only
//! How many history rows a turn injects.
//!
//! Spec: `docs/superpowers/specs/2026-09-02-echo-cancellation-plus-design.md`
//! §4.6. A well-described character needs little transcript; a barely-described
//! one needs more, so the window is a function of how completely
//! `character_insights` is populated. This makes the low-activity coverage gap
//! self-correcting instead of a special case.
//!
//! Pure + unit-testable (no I/O). Separate from `repetition`, which answers
//! what is *inside* a row rather than how many rows there are.

use crate::repetition::Injected;
use eros_engine_store::character_insight::CharacterInsightsRow;
use uuid::Uuid;

/// Rows before the current turn that are never subject to the ladder: the
/// previous turn's user message and its assistant reply. `character_insights`
/// is a snapshot lagging one turn, so it can say what is true of the character
/// but never what was just said; reference resolution has no other carrier.
pub const PROTECTED_PRIOR: usize = 2;

/// Field count at or above which no extra rows are injected.
const FULL_ENOUGH: usize = 7;

/// Extra rows added per field below [`FULL_ENOUGH`].
const ROWS_PER_MISSING_FIELD: usize = 2;

/// How many of the ten `character_insights` fields carry a value. A missing
/// row counts as zero — those relationships are exactly the ones needing the
/// most transcript.
///
/// Blank-but-present strings do not count. Arrays are tested for emptiness
/// directly; note that the SQL equivalent must use `cardinality(col) = 0`, as
/// `array_length(col, 1)` returns NULL rather than 0 for an empty array.
pub fn filled_field_count(row: Option<&CharacterInsightsRow>) -> usize {
    let Some(r) = row else { return 0 };
    let text = [
        &r.location,
        &r.occupation,
        &r.current_situation,
        &r.desires,
        &r.vulnerabilities,
        &r.habits,
        &r.personal_values,
    ]
    .iter()
    .filter(|v| v.as_deref().is_some_and(|s| !s.trim().is_empty()))
    .count();
    let arrays = [&r.likes, &r.dislikes, &r.relationships]
        .iter()
        .filter(|v| !v.is_empty())
        .count();
    text + arrays
}

/// Rows to inject beyond the protected pair, from the ladder in spec §4.6.
/// Saturates at 0 for a fully-described character and at 14 for one the engine
/// knows nothing about — 17 injected rows in total, near the pre-change 20.
pub fn window_extra(filled: usize) -> usize {
    FULL_ENOUGH.saturating_sub(filled) * ROWS_PER_MISSING_FIELD
}

/// Keep the current turn plus the newest `PROTECTED_PRIOR + extra` of the
/// remaining rows, order-preserving.
///
/// The current row is kept by identity, not by position: on the async worker
/// path the driving row is pinned at index 0, older than everything else, and
/// must survive even the thinnest rung. An absent `current_id` (no driving row
/// in the window at all) degrades to keeping the newest rows.
pub fn select_window(msgs: Vec<Injected>, current_id: Uuid, extra: usize) -> Vec<Injected> {
    let budget = PROTECTED_PRIOR + extra;
    // Walk newest-first, spending the budget on everything that is not the
    // current row; the current row is free.
    let mut spent = 0usize;
    let mut keep: Vec<bool> = vec![false; msgs.len()];
    for (i, m) in msgs.iter().enumerate().rev() {
        if m.id == current_id {
            keep[i] = true;
        } else if spent < budget {
            keep[i] = true;
            spent += 1;
        }
    }
    msgs.into_iter()
        .zip(keep)
        .filter_map(|(m, k)| k.then_some(m))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text_fields: usize, arr_fields: usize) -> CharacterInsightsRow {
        let s = |n: usize, i: usize| (i < n).then(|| "x".to_string());
        let a = |n: usize, i: usize| if i < n { vec!["x".to_string()] } else { vec![] };
        CharacterInsightsRow {
            instance_id: Uuid::new_v4(),
            location: s(text_fields, 0),
            occupation: s(text_fields, 1),
            current_situation: s(text_fields, 2),
            desires: s(text_fields, 3),
            vulnerabilities: s(text_fields, 4),
            habits: s(text_fields, 5),
            personal_values: s(text_fields, 6),
            likes: a(arr_fields, 0),
            dislikes: a(arr_fields, 1),
            relationships: a(arr_fields, 2),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn a_missing_row_counts_as_zero_fields() {
        assert_eq!(filled_field_count(None), 0);
    }

    #[test]
    fn blank_strings_and_empty_arrays_do_not_count() {
        let mut r = row(0, 0);
        r.location = Some("   ".into());
        r.likes = vec![];
        assert_eq!(filled_field_count(Some(&r)), 0);
    }

    #[test]
    fn filled_field_count_counts_all_ten() {
        assert_eq!(filled_field_count(Some(&row(7, 3))), 10);
        assert_eq!(filled_field_count(Some(&row(3, 0))), 3);
        assert_eq!(filled_field_count(Some(&row(7, 0))), 7);
    }

    #[test]
    fn ladder_matches_the_spec_table() {
        // spec §4.6
        assert_eq!(window_extra(10), 0);
        assert_eq!(window_extra(7), 0);
        assert_eq!(window_extra(6), 2);
        assert_eq!(window_extra(5), 4);
        assert_eq!(window_extra(4), 6);
        assert_eq!(window_extra(3), 8);
        assert_eq!(window_extra(2), 10);
        assert_eq!(window_extra(1), 12);
        assert_eq!(window_extra(0), 14);
    }

    fn inj(role: &str, text: &str) -> Injected {
        Injected {
            id: Uuid::new_v4(),
            role: role.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn thinnest_rung_keeps_the_current_turn_and_the_one_before() {
        let msgs: Vec<Injected> = (0..10).map(|i| inj("user", &i.to_string())).collect();
        let current = msgs[9].id;
        let kept = select_window(msgs, current, 0);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["7", "8", "9"], "current + PROTECTED_PRIOR");
    }

    #[test]
    fn extra_rows_are_taken_from_the_newest_end() {
        let msgs: Vec<Injected> = (0..10).map(|i| inj("user", &i.to_string())).collect();
        let current = msgs[9].id;
        let kept = select_window(msgs, current, 4);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["3", "4", "5", "6", "7", "8", "9"]);
    }

    #[test]
    fn chronological_order_is_preserved() {
        let msgs: Vec<Injected> = (0..6).map(|i| inj("user", &i.to_string())).collect();
        let current = msgs[5].id;
        let kept = select_window(msgs, current, 1);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["2", "3", "4", "5"]);
    }

    #[test]
    fn a_pinned_driving_row_at_the_front_always_survives() {
        // The async worker path inserts the driving row at index 0, older than
        // everything else. It must survive the thinnest rung.
        let mut msgs: Vec<Injected> = (0..10).map(|i| inj("user", &i.to_string())).collect();
        let driving = inj("user", "driving");
        let current = driving.id;
        msgs.insert(0, driving);
        let kept = select_window(msgs, current, 0);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["driving", "8", "9"]);
    }

    #[test]
    fn a_session_shorter_than_the_protected_window_is_unchanged() {
        let msgs = vec![inj("user", "a")];
        let current = msgs[0].id;
        let kept = select_window(msgs.clone(), current, 0);
        assert_eq!(kept, msgs);
    }

    #[test]
    fn an_absent_current_id_still_keeps_the_newest_rows() {
        let msgs: Vec<Injected> = (0..6).map(|i| inj("user", &i.to_string())).collect();
        let kept = select_window(msgs, Uuid::new_v4(), 0);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["4", "5"]);
    }
}

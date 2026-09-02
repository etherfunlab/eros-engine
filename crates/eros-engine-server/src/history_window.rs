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

/// Floor on how many prior rows [`select_window`] protects, regardless of
/// how short the found previous exchange is. Two adjacent `user` rows —
/// a double-text, a ghosted turn, or an assistant row that §4.3's strip
/// emptied and `model_facing_history` dropped — collapse the exchange
/// search to a single row and would otherwise evict the character's actual
/// last reply. It is also what a fixture with no prior `user` row at all
/// (the async worker's pinned-driving-row shape) falls back to entirely.
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

/// Rows to inject beyond the previous exchange, from the ladder in spec §4.6.
/// Saturates at 0 for a fully-described character and at 14 for one the engine
/// knows nothing about — 17 injected rows in total, near the pre-change 20.
pub fn window_extra(filled: usize) -> usize {
    FULL_ENOUGH.saturating_sub(filled) * ROWS_PER_MISSING_FIELD
}

/// Keep the current turn, its previous exchange, and `extra` more rows spent
/// newest-first on anything not already kept. Order-preserving.
///
/// The previous exchange is the newest row with `role == "user"` strictly
/// older than the current row, plus every row between it and the current
/// row. In the common case that is exactly two rows — the previous turn's
/// user message and the assistant's one reply to it — but it stretches to
/// cover shapes like `[U_prev, A_text, A_image, U_cur]`, where one turn
/// produced two assistant rows (e.g. a text reply and a separate image row).
/// `character_insights` is a snapshot lagging one turn, so it can say what is
/// true of the character but never what was just said; reference resolution
/// has no other carrier (spec §4.6, decision D1).
///
/// The found exchange is then topped up to [`PROTECTED_PRIOR`] prior rows if
/// it came up short — most commonly because the row directly before current
/// is *also* `user` (a double-text, a ghosted turn, or a stripped-empty
/// assistant row dropped upstream), which otherwise collapses the exchange
/// to that single row and evicts the character's actual last reply. If no
/// user row precedes the current one at all, this floor is the entire
/// protected span.
///
/// The current row is kept by identity, not by position: on the async worker
/// path the driving row is pinned at index 0, older than everything else, and
/// must survive even the thinnest rung. An absent `current_id` (no driving row
/// in the window at all) degrades to keeping the newest rows.
pub fn select_window(msgs: Vec<Injected>, current_id: Uuid, extra: usize) -> Vec<Injected> {
    let cur = msgs.iter().position(|m| m.id == current_id);
    let mut keep = vec![false; msgs.len()];
    if let Some(c) = cur {
        keep[c] = true;
    }
    let search_end = cur.unwrap_or(msgs.len());
    let prev_user = msgs[..search_end].iter().rposition(|m| m.role == "user");
    if let Some(u) = prev_user {
        for k in keep.iter_mut().take(search_end).skip(u) {
            *k = true;
        }
    }
    let protected_prior = keep.iter().filter(|&&k| k).count() - usize::from(cur.is_some());
    let mut budget = extra + PROTECTED_PRIOR.saturating_sub(protected_prior);
    for k in keep.iter_mut().rev() {
        if budget == 0 {
            break;
        }
        if *k {
            continue;
        }
        *k = true;
        budget -= 1;
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

    /// The shape a real conversation actually produces: alternating turns.
    /// Even indices are `user`, odd are `assistant`.
    fn alt(i: usize) -> Injected {
        inj(
            if i % 2 == 0 { "user" } else { "assistant" },
            &i.to_string(),
        )
    }

    /// Same alternating shape as [`alt`], but with the parity flipped: odd
    /// indices are `user`, even are `assistant`. `select_window`'s only
    /// caller always passes a *user* row's id as `current_id`
    /// (`handlers.rs`'s `user_message_id`), so a fixture whose `current` is
    /// its last (odd) index needs this parity to land current on `user` —
    /// `alt` would put an odd index on `assistant`, a shape that never
    /// occurs in production.
    fn alt_odd_is_user(i: usize) -> Injected {
        inj(
            if i % 2 == 1 { "user" } else { "assistant" },
            &i.to_string(),
        )
    }

    #[test]
    fn thinnest_rung_keeps_the_current_turn_and_the_one_before() {
        // 10 rows alternating assistant/user, current = row 9 (user, as
        // every real current row is). Row 8 (assistant) is its immediate
        // reply-to-be-answered and row 7 (user) is the newest user row
        // older than current: the classic 3-row steady state.
        let msgs: Vec<Injected> = (0..10).map(alt_odd_is_user).collect();
        let current = msgs[9].id;
        let kept = select_window(msgs, current, 0);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["7", "8", "9"],
            "current + the previous exchange (user row 7, assistant row 8)"
        );
    }

    #[test]
    fn extra_rows_are_taken_from_the_newest_end() {
        // Same 10-row fixture as above: previous exchange is rows 7-8, so
        // extra=4 spends on rows 3-6, for 7 rows total.
        let msgs: Vec<Injected> = (0..10).map(alt_odd_is_user).collect();
        let current = msgs[9].id;
        let kept = select_window(msgs, current, 4);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["3", "4", "5", "6", "7", "8", "9"]);
    }

    #[test]
    fn chronological_order_is_preserved() {
        // Previous exchange is rows 3-4 (user, assistant); extra=1 reaches
        // back to row 2.
        let msgs: Vec<Injected> = (0..6).map(alt_odd_is_user).collect();
        let current = msgs[5].id;
        let kept = select_window(msgs, current, 1);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["2", "3", "4", "5"]);
    }

    #[test]
    fn double_texting_keeps_the_actual_last_reply_not_just_the_ghosted_user_row() {
        // Two adjacent user rows before current -- a double-text, a
        // ghosted turn left unanswered, or an assistant row §4.3 stripped
        // to empty and `model_facing_history` dropped. The newest-user
        // anchor alone finds only u_a (nothing lies between it and
        // current), which would evict a_x -- exactly the non-sequitur D1
        // exists to prevent, since the character's last actual reply is
        // the only thing that answers whatever u_x said. PROTECTED_PRIOR
        // is a floor, not an either/or: it tops the span back up to 2
        // prior rows, pulling a_x back in.
        let msgs = vec![
            inj("user", "u_x"),
            inj("assistant", "a_x"),
            inj("user", "u_a"),
            inj("user", "u_cur"),
        ];
        let current = msgs[3].id;
        let kept = select_window(msgs, current, 0);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["a_x", "u_a", "u_cur"],
            "a_x must survive: the floor tops up past the single-row collapse"
        );
    }

    #[test]
    fn two_assistant_rows_from_one_turn_keep_the_previous_user_row() {
        // The real shape that motivated this fix: `image_edit.rs` can write
        // an empty-content assistant row carrying image metadata alongside
        // a text reply, so one turn can leave two adjacent assistant rows.
        // The previous exchange is "the newest prior user row plus
        // everything between it and current" — here that is all three rows
        // before current, not just the newest two.
        let msgs = vec![
            inj("user", "u_prev"),
            inj("assistant", "a_text"),
            inj("assistant", "a_image"),
            inj("user", "u_cur"),
        ];
        let current = msgs[3].id;
        let kept = select_window(msgs, current, 0);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["u_prev", "a_text", "a_image", "u_cur"],
            "u_prev must survive alongside both assistant rows"
        );
    }

    #[test]
    fn a_pinned_driving_row_at_the_front_always_survives() {
        // The async worker path inserts the driving row at index 0, older than
        // everything else. No user row precedes it, so the PROTECTED_PRIOR
        // fallback applies and it must survive the thinnest rung.
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
        // No current row to anchor on, so the search for a previous user row
        // runs over the whole list: row 4 (user) plus row 5 (assistant)
        // between it and the end. Same result as before this fix, since
        // these two rows happen to be adjacent either way.
        let msgs: Vec<Injected> = (0..6).map(alt).collect();
        let kept = select_window(msgs, Uuid::new_v4(), 0);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["4", "5"]);
    }
}

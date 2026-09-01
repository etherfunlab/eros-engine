// SPDX-License-Identifier: AGPL-3.0-only
//! Anti-repetition, in two halves. `overused_openings` mines over-used
//! sentence-openings from a persona's recent assistant turns so the chat
//! prompt can discourage them before generation. `Injected` (and, from the
//! echo-cancellation change, `cancel_echo`) work on the other side: what the
//! model is allowed to condition on. Pure + unit-testable (no I/O).

use std::collections::{HashMap, HashSet};

/// One history row as it will reach the model: the role string the provider
/// sees, and the exact text that will be sent. Produced by
/// `handlers::model_facing_history`, which is the only layer that knows what
/// is injected and what it looks like once image markers are folded in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Injected {
    /// Source `chat_messages.id`. Carried so echo cancellation can exempt the
    /// current turn by identity rather than by position.
    pub id: uuid::Uuid,
    pub role: String,
    pub text: String,
}

/// Leading characters of a turn's first sentence that define its "opening".
/// CJK-aware (counted in `char`s, not bytes). Tunable.
const OPENING_CHARS: usize = 4;

/// Cap on how many openings we surface to the prompt.
const MAX_OUTPUT: usize = 5;

/// Characters that end a sentence. We split on any of these and take the first
/// non-empty segment as the turn's opening sentence.
const SENTENCE_DELIMS: &[char] = &['。', '！', '？', '\n', '…', '!', '?', '~'];

/// The normalized opening of a single turn, or `None` when the turn has no
/// non-empty leading sentence.
fn opening_of(turn: &str) -> Option<String> {
    let first_sentence = turn
        .split(|c| SENTENCE_DELIMS.contains(&c))
        .map(str::trim)
        .find(|s| !s.is_empty())?;
    let opening: String = first_sentence.chars().take(OPENING_CHARS).collect();
    let opening = opening.trim().to_string();
    if opening.is_empty() {
        None
    } else {
        Some(opening)
    }
}

/// Remove a turn's leading sentence — the noise carrier. Splits on
/// [`SENTENCE_DELIMS`], drops everything up to and including the terminator of
/// the first non-empty trimmed segment, and returns the trimmed remainder.
///
/// Returns an empty string when the text has no second sentence, which is the
/// signal for the caller to drop the row entirely (spec §4.3). This function
/// does not decide that; it only reports the remainder.
///
/// Production measurement: turns whose first sentence is one character are
/// 23.5% of turns and carry 58.2% of all opening repetition, so what this
/// removes is overwhelmingly `唔` / `啊` / `嗯啊`. The median first sentence is
/// 17.6% of the message.
pub fn strip_leading_sentence(text: &str) -> String {
    let mut rest = text;
    loop {
        // Position of the next delimiter, in bytes.
        let end = rest.find(SENTENCE_DELIMS);
        let (segment, after) = match end {
            Some(i) => {
                // Split AFTER the delimiter: `i` is its start, and delimiters
                // here are multi-byte, so step by the char's own length.
                let delim_len = rest[i..].chars().next().map_or(1, char::len_utf8);
                (&rest[..i], &rest[i + delim_len..])
            }
            // No delimiter left: the whole remainder is one segment and there
            // is nothing after it.
            None => (rest, ""),
        };
        if !segment.trim().is_empty() {
            return after.trim().to_string();
        }
        if after.is_empty() {
            return String::new();
        }
        rest = after;
    }
}

/// Mine over-used sentence-openings from the persona's recent assistant turns.
/// Returns openings that recur in **≥2** of the turns, deduped, in first-seen
/// order, capped at [`MAX_OUTPUT`]. Empty when nothing recurs or there is too
/// little history (< 2 usable openings).
pub fn overused_openings(recent_assistant: &[String]) -> Vec<String> {
    let openings: Vec<String> = recent_assistant
        .iter()
        .filter_map(|t| opening_of(t))
        .collect();
    if openings.len() < 2 {
        return Vec::new();
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for o in &openings {
        *counts.entry(o.as_str()).or_insert(0) += 1;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for o in &openings {
        if counts[o.as_str()] >= 2 && seen.insert(o.as_str()) {
            out.push(o.clone());
            if out.len() >= MAX_OUTPUT {
                break;
            }
        }
    }
    out
}

/// What one turn's echo cancellation removed. All zero when the window held
/// no duplicates. Logged, never stored — the same counts are recomputable
/// from `chat_messages` with a window query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EchoStats {
    /// Rows removed from the injected history.
    pub dropped: usize,
    /// Rows that survived.
    pub kept: usize,
    /// Distinct strings that occurred more than once.
    pub groups: usize,
    /// Largest occurrence count among the duplicated strings — the
    /// amplification reading. Zero when nothing was duplicated.
    pub max_occ: usize,
}

/// Drop every occurrence of any non-empty string that appears more than once
/// in `msgs`, preserving order.
///
/// Three ways to survive: the text is empty after `trim` (empty strings never
/// participate, and are passed through exactly as they arrive); the text
/// occurs exactly once; or the row IS the current turn (`current_id`), which
/// is never dropped — otherwise a user who repeats themselves gets a turn the
/// model receives with no user input at all.
///
/// The key is the text alone. The role is deliberately not part of it: a user
/// line and an assistant line with the same text are the model parroting, and
/// that is the case this exists for. Comparison is byte-exact; `trim` is used
/// only for the emptiness test.
pub fn cancel_echo(msgs: Vec<Injected>, current_id: uuid::Uuid) -> (Vec<Injected>, EchoStats) {
    let (keep, groups, max_occ) = {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for m in &msgs {
            if !m.text.trim().is_empty() {
                *counts.entry(m.text.as_str()).or_insert(0) += 1;
            }
        }
        let groups = counts.values().filter(|&&n| n > 1).count();
        let max_occ = counts
            .values()
            .copied()
            .filter(|&n| n > 1)
            .max()
            .unwrap_or(0);
        let keep: Vec<bool> = msgs
            .iter()
            .map(|m| {
                m.text.trim().is_empty()
                    || m.id == current_id
                    || counts.get(m.text.as_str()).copied().unwrap_or(0) < 2
            })
            .collect();
        (keep, groups, max_occ)
    };

    let mut kept = Vec::with_capacity(msgs.len());
    let mut dropped = 0usize;
    for (m, k) in msgs.into_iter().zip(keep) {
        if k {
            kept.push(m);
        } else {
            dropped += 1;
        }
    }
    let stats = EchoStats {
        dropped,
        kept: kept.len(),
        groups,
        max_occ,
    };
    (kept, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recurring_opening_is_surfaced() {
        // First 4 chars of the first sentence: "我看着你" recurs; "今天天气" once.
        let turns = v(&[
            "我看着你，轻轻笑了。",
            "我看着你的眼睛说。",
            "今天天气真好啊！",
        ]);
        let out = overused_openings(&turns);
        assert_eq!(out, vec!["我看着你".to_string()]);
    }

    #[test]
    fn no_recurrence_returns_empty() {
        let turns = v(&["我看着你笑。", "今天天气真好。", "晚安啦宝贝。"]);
        assert!(overused_openings(&turns).is_empty());
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(overused_openings(&[]).is_empty());
    }

    #[test]
    fn single_turn_returns_empty() {
        assert!(overused_openings(&v(&["我看着你笑了。"])).is_empty());
    }

    #[test]
    fn opening_taken_from_first_sentence_only() {
        // Both turns' FIRST sentence shares the opening; later sentences differ.
        let turns = v(&["嗯嗯好的，然后我们去吃饭。", "嗯嗯好的，那就这样吧。"]);
        assert_eq!(overused_openings(&turns), vec!["嗯嗯好的".to_string()]);
    }

    #[test]
    fn output_capped_at_five() {
        // Six distinct openings, each appearing twice → recurring count is 6,
        // but the cap clips it to 5.
        let bases = [
            "甲一二三",
            "乙一二三",
            "丙一二三",
            "丁一二三",
            "戊一二三",
            "己一二三",
        ];
        let mut turns: Vec<String> = Vec::new();
        for b in bases {
            turns.push(format!("{b}。"));
            turns.push(format!("{b}！"));
        }
        let out = overused_openings(&turns);
        assert_eq!(out.len(), MAX_OUTPUT);
    }

    #[test]
    fn single_long_turn_does_not_panic() {
        // One long delimiter-free turn → < 2 openings → empty, no panic on the
        // char-boundary slice.
        let long = "我看着你".repeat(500);
        assert!(overused_openings(&[long]).is_empty());
    }

    #[test]
    fn whitespace_and_delimiter_only_turns_yield_no_opening() {
        // A whitespace-only turn and a delimiter-only turn both produce no
        // usable opening, so nothing recurs → empty.
        let turns = v(&["   ", "。。。！", "我看着你笑了。"]);
        assert!(overused_openings(&turns).is_empty());
    }

    fn inj(id: uuid::Uuid, role: &str, text: &str) -> Injected {
        Injected {
            id,
            role: role.to_string(),
            text: text.to_string(),
        }
    }

    /// Build a window of (role, text) pairs with fresh ids, plus the id of the
    /// row at `current_idx` to pass as the exemption.
    fn window(rows: &[(&str, &str)], current_idx: usize) -> (Vec<Injected>, uuid::Uuid) {
        let msgs: Vec<Injected> = rows
            .iter()
            .map(|(r, t)| inj(uuid::Uuid::new_v4(), r, t))
            .collect();
        let current = msgs[current_idx].id;
        (msgs, current)
    }

    fn texts(msgs: &[Injected]) -> Vec<&str> {
        msgs.iter().map(|m| m.text.as_str()).collect()
    }

    #[test]
    fn duplicate_drops_every_occurrence() {
        // a b a c d → b c d. Both copies of `a` go; the spec's worked example.
        // Current turn is `d`, which is not duplicated.
        let (msgs, current) = window(
            &[
                ("user", "a"),
                ("assistant", "b"),
                ("user", "a"),
                ("assistant", "c"),
                ("user", "d"),
            ],
            4,
        );
        let (kept, stats) = cancel_echo(msgs, current);
        assert_eq!(texts(&kept), vec!["b", "c", "d"]);
        assert_eq!(stats.dropped, 2);
        assert_eq!(stats.kept, 3);
        assert_eq!(stats.groups, 1);
        assert_eq!(stats.max_occ, 2);
    }

    #[test]
    fn current_turn_survives_its_own_duplicate() {
        // a b c a(current) → b c a(current). The earlier copy goes, the thing
        // the user just typed does not.
        let (msgs, current) = window(
            &[
                ("user", "a"),
                ("assistant", "b"),
                ("assistant", "c"),
                ("user", "a"),
            ],
            3,
        );
        let (kept, stats) = cancel_echo(msgs, current);
        assert_eq!(texts(&kept), vec!["b", "c", "a"]);
        assert_eq!(kept[2].id, current);
        assert_eq!(stats.dropped, 1);
    }

    #[test]
    fn lone_current_turn_is_kept() {
        let (msgs, current) = window(&[("user", "a")], 0);
        let (kept, stats) = cancel_echo(msgs, current);
        assert_eq!(texts(&kept), vec!["a"]);
        assert_eq!(stats.dropped, 0);
        assert_eq!(stats.groups, 0);
        assert_eq!(stats.max_occ, 0);
    }

    #[test]
    fn empty_text_neither_counts_nor_drops() {
        // Two blank assistant rows and two whitespace-only rows all survive:
        // empty strings never participate in the rule.
        let (msgs, current) = window(
            &[
                ("assistant", ""),
                ("assistant", ""),
                ("assistant", "   "),
                ("assistant", "   "),
                ("user", "hi"),
            ],
            4,
        );
        let (kept, stats) = cancel_echo(msgs, current);
        assert_eq!(kept.len(), 5, "no empty row may be dropped");
        assert_eq!(stats.dropped, 0);
        assert_eq!(stats.groups, 0);
    }

    #[test]
    fn same_text_under_different_roles_is_one_key() {
        // The user says 好, the model says 好 back. Both go — the key does not
        // include the role (design §4.2).
        let (msgs, current) = window(
            &[("user", "好"), ("assistant", "好"), ("user", "接下来呢")],
            2,
        );
        let (kept, stats) = cancel_echo(msgs, current);
        assert_eq!(texts(&kept), vec!["接下来呢"]);
        assert_eq!(stats.dropped, 2);
        assert_eq!(stats.groups, 1);
    }

    #[test]
    fn no_duplicates_is_a_passthrough() {
        let (msgs, current) = window(&[("user", "a"), ("assistant", "b"), ("user", "c")], 2);
        let before = msgs.clone();
        let (kept, stats) = cancel_echo(msgs, current);
        assert_eq!(kept, before);
        assert_eq!(
            stats,
            EchoStats {
                dropped: 0,
                kept: 3,
                groups: 0,
                max_occ: 0
            }
        );
    }

    #[test]
    fn stats_report_groups_and_amplification() {
        // Two distinct duplicated strings; the worse one has 3 copies.
        let (msgs, current) = window(
            &[
                ("assistant", "x"),
                ("assistant", "x"),
                ("assistant", "x"),
                ("user", "y"),
                ("user", "y"),
                ("user", "z"),
            ],
            5,
        );
        let (kept, stats) = cancel_echo(msgs, current);
        assert_eq!(texts(&kept), vec!["z"]);
        assert_eq!(stats.dropped, 5);
        assert_eq!(stats.kept, 1);
        assert_eq!(stats.groups, 2);
        assert_eq!(stats.max_occ, 3);
    }

    #[test]
    fn strip_removes_only_the_first_sentence() {
        assert_eq!(
            strip_leading_sentence("我看着你，轻轻笑了。然后靠近了一点。"),
            "然后靠近了一点。"
        );
    }

    #[test]
    fn strip_splits_on_question_and_exclamation() {
        // ！？~ are in SENTENCE_DELIMS, so a short interjection opening ends there.
        assert_eq!(strip_leading_sentence("怎么了？我在呢。"), "我在呢。");
        assert_eq!(strip_leading_sentence("好呀~那我们走吧。"), "那我们走吧。");
        assert_eq!(strip_leading_sentence("唔！你回来了。"), "你回来了。");
    }

    #[test]
    fn strip_of_a_single_sentence_is_empty() {
        assert_eq!(strip_leading_sentence("我在呢。"), "");
        assert_eq!(strip_leading_sentence("唔"), "");
    }

    #[test]
    fn strip_skips_leading_delimiters_and_whitespace() {
        // Leading delimiters produce empty segments; the first NON-empty one is
        // the sentence that gets removed.
        assert_eq!(
            strip_leading_sentence("。。。我在呢。还有事吗？"),
            "还有事吗？"
        );
        assert_eq!(
            strip_leading_sentence("   我在呢。好久不见。"),
            "好久不见。"
        );
    }

    #[test]
    fn strip_of_blank_or_delimiter_only_is_empty() {
        assert_eq!(strip_leading_sentence(""), "");
        assert_eq!(strip_leading_sentence("   "), "");
        assert_eq!(strip_leading_sentence("。。。！"), "");
    }

    #[test]
    fn strip_splits_on_newline() {
        assert_eq!(strip_leading_sentence("我看着你\n你也看着我"), "你也看着我");
    }

    #[test]
    fn strip_is_char_boundary_safe_on_long_delimiterless_text() {
        // One long segment with no delimiter ⇒ the whole thing is the first
        // sentence ⇒ empty. Must not panic on a byte slice into CJK.
        let long = "我看着你".repeat(500);
        assert_eq!(strip_leading_sentence(&long), "");
    }

    #[test]
    fn strip_keeps_the_photo_marker_when_it_follows_a_sentence() {
        // model_facing_assistant_text appends "\n\n[你的照片：…]"; the marker
        // must survive when there is a sentence before it.
        assert_eq!(
            strip_leading_sentence("给你看这个。\n\n[你的照片：海边]"),
            "[你的照片：海边]"
        );
    }
}

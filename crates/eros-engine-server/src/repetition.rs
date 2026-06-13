// SPDX-License-Identifier: AGPL-3.0-only
//! Dynamic anti-repetition: mine over-used sentence-openings from a persona's
//! recent assistant turns so the chat prompt can discourage them this turn.
//! Pure + unit-testable (no I/O).

use std::collections::{HashMap, HashSet};

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

/// Mine over-used sentence-openings from the persona's recent assistant turns.
/// Returns openings that recur in **≥2** of the turns, deduped, in first-seen
/// order, capped at [`MAX_OUTPUT`]. Empty when nothing recurs or there is too
/// little history (< 2 usable openings).
pub fn overused_openings(recent_assistant: &[String]) -> Vec<String> {
    let openings: Vec<String> = recent_assistant.iter().filter_map(|t| opening_of(t)).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recurring_opening_is_surfaced() {
        // First 4 chars of the first sentence: "我看着你" recurs; "今天天气" once.
        let turns = v(&["我看着你，轻轻笑了。", "我看着你的眼睛说。", "今天天气真好啊！"]);
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
        let bases = ["甲一二三", "乙一二三", "丙一二三", "丁一二三", "戊一二三", "己一二三"];
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
}

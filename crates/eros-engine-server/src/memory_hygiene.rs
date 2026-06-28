// SPDX-License-Identifier: AGPL-3.0-only
//! Recall-time hygiene for companion memories (issue #113): drop recalled
//! items that echo the persona's own recent assistant output (the feedback
//! loop) or duplicate another recalled item. Pure, deterministic, no I/O.

/// Minimum normalized length (chars) for a *containment* match to count.
/// Guards against suppressing a memory because a 1–2 char fragment of it
/// happens to appear in a recent reply. Exact-equality matches are not
/// length-guarded (an identical line is always redundant).
const MIN_MATCH_CHARS: usize = 6;

/// Normalize a memory/turn line for comparison: strip a leading speaker label
/// (`用户：` / `AI：`), collapse internal whitespace to single spaces, trim,
/// and lowercase ASCII (CJK is unaffected). Char-boundary safe.
fn normalize(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix("用户：")
        .or_else(|| s.strip_prefix("AI："))
        .unwrap_or(s);
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.extend(ch.to_lowercase());
            prev_ws = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// True if two normalized strings are "close": equal, or one contains the
/// other with the shorter at least `MIN_MATCH_CHARS` chars long.
fn close_match(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let (short, long) = if a.chars().count() <= b.chars().count() {
        (a, b)
    } else {
        (b, a)
    };
    short.chars().count() >= MIN_MATCH_CHARS && long.contains(short)
}

/// Decide whether to keep `raw`; on keep, record its normalized form in `kept`.
fn keep_item(raw: &str, recent_norm: &[String], kept: &mut Vec<String>) -> bool {
    let n = normalize(raw);
    if n.is_empty() {
        return false;
    }
    if recent_norm.iter().any(|r| close_match(&n, r)) {
        return false; // (a) self-output suppression
    }
    if kept.iter().any(|k| close_match(&n, k)) {
        return false; // (b) dedup against already-kept
    }
    kept.push(n);
    true
}

/// Prune recalled memories before they enter the prompt. Profile groups are
/// processed first (they inject before `[shared_memories]` in `build_prompt`),
/// so a fact present in both layers is kept in the profile and dropped from
/// the relationship layer. Order-preserving.
pub fn prune_recalled(
    mut profile_groups: Vec<(String, Vec<String>)>,
    relationship_facts: Vec<String>,
    recent_assistant: &[String],
) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    let recent_norm: Vec<String> = recent_assistant
        .iter()
        .map(|s| normalize(s))
        .filter(|s| !s.is_empty())
        .collect();

    let mut kept_norm: Vec<String> = Vec::new();

    for (_label, items) in profile_groups.iter_mut() {
        items.retain(|raw| keep_item(raw, &recent_norm, &mut kept_norm));
    }

    let pruned_rel: Vec<String> = relationship_facts
        .into_iter()
        .filter(|raw| keep_item(raw, &recent_norm, &mut kept_norm))
        .collect();

    (profile_groups, pruned_rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_recalled_item_equal_to_recent_assistant_output() {
        let (_p, rel) = prune_recalled(
            vec![],
            vec!["我看着你，轻声说。".into(), "你今天过得怎么样".into()],
            &["我看着你，轻声说。".into()],
        );
        assert_eq!(rel, vec!["你今天过得怎么样".to_string()]);
    }

    #[test]
    fn suppresses_recalled_item_that_is_a_clause_of_a_recent_reply() {
        let (_p, rel) = prune_recalled(
            vec![],
            vec!["我看着你的眼睛".into()],
            &["嗯，我看着你的眼睛，慢慢说。".into()],
        );
        assert!(
            rel.is_empty(),
            "contained clause (>= MIN_MATCH_CHARS) must be dropped"
        );
    }

    #[test]
    fn keeps_short_incidental_overlap() {
        // "公园" (2 chars) is contained in the recent reply but below
        // MIN_MATCH_CHARS, so it is NOT suppressed by containment.
        let (_p, rel) = prune_recalled(vec![], vec!["公园".into()], &["我今天去了公园散步".into()]);
        assert_eq!(rel, vec!["公园".to_string()]);
    }

    #[test]
    fn dedups_cross_layer_user_turn() {
        // Same turn recalled both as a profile fact and a relationship row.
        let (profile, rel) = prune_recalled(
            vec![("fact".into(), vec!["住在上海".into()])],
            vec!["用户：住在上海".into()],
            &[],
        );
        assert_eq!(
            profile,
            vec![("fact".to_string(), vec!["住在上海".to_string()])]
        );
        assert!(
            rel.is_empty(),
            "relationship dup of a profile fact is dropped"
        );
    }

    #[test]
    fn preserves_order_and_unrelated_items() {
        let (_p, rel) = prune_recalled(
            vec![],
            vec!["甲".into(), "乙丙丁戊己庚".into(), "辛壬癸子丑寅".into()],
            &[],
        );
        assert_eq!(
            rel,
            vec![
                "甲".to_string(),
                "乙丙丁戊己庚".to_string(),
                "辛壬癸子丑寅".to_string()
            ]
        );
    }

    #[test]
    fn empty_inputs_return_empty() {
        let (p, rel) = prune_recalled(vec![], vec![], &[]);
        assert!(p.is_empty());
        assert!(rel.is_empty());
    }
}

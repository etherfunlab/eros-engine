// SPDX-License-Identifier: AGPL-3.0-only
//! Recall-time dedup for companion memories (issue #113): drop recalled items
//! whose normalized form exactly equals an already-kept item. Cross-layer: a
//! fact present in both the profile and relationship layers is kept in the
//! profile and dropped from the relationship layer. Pure, deterministic, no I/O.

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

/// Decide whether to keep `raw`; on keep, record its normalized form in `kept`.
/// Drops items whose normalized form exactly equals an already-kept item.
fn keep_item(raw: &str, kept: &mut Vec<String>) -> bool {
    let n = normalize(raw);
    if n.is_empty() {
        return false;
    }
    if kept.iter().any(|k| k == &n) {
        return false;
    }
    kept.push(n);
    true
}

/// Dedup recalled memories before they enter the prompt. Profile groups are
/// processed first (they inject before `[shared_memories]` in `build_prompt`),
/// so a fact present in both layers is kept in the profile and dropped from
/// the relationship layer. Order-preserving. Pure; no I/O.
pub fn prune_recalled(
    mut profile_groups: Vec<(String, Vec<String>)>,
    relationship_facts: Vec<String>,
) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    let mut kept_norm: Vec<String> = Vec::new();

    for (_label, items) in profile_groups.iter_mut() {
        items.retain(|raw| keep_item(raw, &mut kept_norm));
    }

    let pruned_rel: Vec<String> = relationship_facts
        .into_iter()
        .filter(|raw| keep_item(raw, &mut kept_norm))
        .collect();

    (profile_groups, pruned_rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedups_exact_duplicate_relationship_facts() {
        let (_p, rel) = prune_recalled(
            vec![],
            vec![
                "用户：今天好累".into(),
                "用户：今天好累".into(),
                "你还好吗".into(),
            ],
        );
        assert_eq!(
            rel,
            vec!["用户：今天好累".to_string(), "你还好吗".to_string()]
        );
    }

    #[test]
    fn dedups_cross_layer_user_turn() {
        // Same turn recalled both as a profile fact and a relationship row;
        // normalize strips the 用户： label so the two are equal → dedup.
        let (profile, rel) = prune_recalled(
            vec![("fact".into(), vec!["住在上海".into()])],
            vec!["用户：住在上海".into()],
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
    fn keeps_substring_distinct_items() {
        // "公园" is a substring of the longer item but they are not equal after
        // normalization — both must be kept.
        let (_p, rel) = prune_recalled(vec![], vec!["我今天去了公园散步".into(), "公园".into()]);
        assert_eq!(
            rel,
            vec!["我今天去了公园散步".to_string(), "公园".to_string()]
        );
    }

    #[test]
    fn preserves_order_and_unrelated_items() {
        let (_p, rel) = prune_recalled(
            vec![],
            vec![
                "甲乙丙丁戊己".into(),
                "庚辛壬癸子丑".into(),
                "寅卯辰巳午未".into(),
            ],
        );
        assert_eq!(
            rel,
            vec![
                "甲乙丙丁戊己".to_string(),
                "庚辛壬癸子丑".to_string(),
                "寅卯辰巳午未".to_string()
            ]
        );
    }

    #[test]
    fn empty_inputs_return_empty() {
        let (p, rel) = prune_recalled(vec![], vec![]);
        assert!(p.is_empty());
        assert!(rel.is_empty());
    }

    #[test]
    fn keeps_richer_memory_that_contains_a_shorter_profile_fact() {
        // A profile fact contained in a longer relationship memory must NOT drop
        // the richer memory — they are not duplicates (codex [P2]).
        let (profile, rel) = prune_recalled(
            vec![("preference".into(), vec!["喜欢吃意大利面".into()])],
            vec!["用户：喜欢吃意大利面，但是对海鲜过敏".into()],
        );
        assert_eq!(
            profile,
            vec![("preference".to_string(), vec!["喜欢吃意大利面".to_string()])]
        );
        assert_eq!(
            rel,
            vec!["用户：喜欢吃意大利面，但是对海鲜过敏".to_string()],
            "richer memory must not be dropped"
        );
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Incremental `clean_response ∘ trim`.
//!
//! The non-streaming client gates and serves `clean_response(raw.trim())`
//! (fence strip, surrounding-quote strip, trim — `openrouter.rs`). A streaming
//! consumer that must stay byte-identical to that feeds chunks in here and may
//! emit only `stable()`: the longest prefix of the FINAL cleaned text that no
//! future chunk can change. At end-of-stream `full_clean()` IS the batch text.

use crate::openrouter::clean_response;

pub struct StreamCleaner {
    acc: String,
}

impl StreamCleaner {
    pub fn new() -> Self {
        Self { acc: String::new() }
    }

    pub fn push(&mut self, chunk: &str) {
        self.acc.push_str(chunk);
    }

    /// `clean_response(trim(everything pushed so far))` — authoritative only at
    /// end-of-stream; mid-stream it is a moving target `stable()` protects
    /// against. Filter outputs are a few KB at most, so recomputing per call is
    /// cheaper than being clever.
    pub fn full_clean(&self) -> String {
        clean_response(self.acc.trim())
    }

    /// The prefix of the final cleaned text no future chunk can change. Three
    /// things can still move the tail: trailing whitespace/quote runs are
    /// trimmed at EOS, a trailing backtick run may turn out to be a closing (or
    /// splitting) fence, and inside an unterminated ```-opening line everything
    /// is still a potential language tag.
    pub fn stable(&self) -> String {
        let t = self.acc.trim_start();
        if let Some(rest) = t.strip_prefix("```") {
            match rest.split_once('\n') {
                // No newline yet: still inside (or before) the language-tag
                // line, could still turn out to be plain fenced content.
                None => return String::new(),
                // Newline seen but nothing (or only whitespace) after it yet:
                // `full_clean()`'s own `.trim()` would eat that boundary
                // newline and make `clean_response` misparse the tag line
                // itself as the fenced body. Hold back until real body bytes
                // arrive.
                Some((_, after)) if after.trim().is_empty() => return String::new(),
                Some(_) => {}
            }
        }
        self.full_clean()
            .trim_end_matches(|c: char| {
                c.is_whitespace() || c == '"' || c == '「' || c == '」' || c == '`'
            })
            .to_string()
    }
}

impl Default for StreamCleaner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openrouter::clean_response;

    /// Strings that exercise every clean_response branch and every unstable-tail
    /// class. Each is fed in EVERY possible 2-chunk split plus whole-at-once,
    /// asserting (a) stable() is always a prefix of the final full_clean(),
    /// (b) stable() never shrinks, (c) full_clean() at EOS == batch.
    const CORPUS: &[&str] = &[
        "plain text with no artifacts at all",
        "  leading and trailing whitespace  ",
        "\"quoted reply\"",
        "「引号回复」",
        "\"「double wrapped」\"",
        "```\nfenced body\n```",
        "```text\nfenced with language tag\n```",
        "```\nno closing fence",
        "```\nA```B",     // interior fence: batch cuts at the LAST ```
        "```\nA```B```C", // batch keeps A```B, drops C
        "ends with partial fence``",
        "ends in quote run\"」", // batch keeps the \" (」 stripped after ")
        "\"say \"",              // batch quirk: inner trailing space survives
        "trailing ideographic space\u{3000}",
        "trailing nbsp\u{a0}",
        "短中文", // multi-byte, shorter than any threshold
        "```",    // bare opening fence only
        "``",     // ambiguous partial opener
    ];

    #[test]
    fn stable_is_monotone_prefix_of_batch_for_all_two_chunk_splits() {
        for s in CORPUS {
            let batch = clean_response(s.trim());
            let byte_splits: Vec<usize> =
                (0..=s.len()).filter(|i| s.is_char_boundary(*i)).collect();
            for cut in byte_splits {
                let mut c = StreamCleaner::new();
                let mut prev_stable = String::new();
                for chunk in [&s[..cut], &s[cut..]] {
                    c.push(chunk);
                    let st = c.stable();
                    assert!(
                        st.starts_with(&prev_stable),
                        "stable shrank on {s:?} cut {cut}: {prev_stable:?} -> {st:?}"
                    );
                    assert!(
                        batch.starts_with(&st),
                        "stable not a prefix of batch on {s:?} cut {cut}: {st:?} vs {batch:?}"
                    );
                    prev_stable = st;
                }
                assert_eq!(c.full_clean(), batch, "EOS mismatch on {s:?} cut {cut}");
            }
        }
    }

    #[test]
    fn stable_is_monotone_prefix_under_char_by_char_feed() {
        for s in CORPUS {
            let batch = clean_response(s.trim());
            let mut c = StreamCleaner::new();
            let mut prev_stable = String::new();
            for ch in s.chars() {
                c.push(&ch.to_string());
                let st = c.stable();
                assert!(st.starts_with(&prev_stable), "shrank on {s:?} at {ch:?}");
                assert!(batch.starts_with(&st), "not prefix on {s:?} at {ch:?}");
                prev_stable = st;
            }
            assert_eq!(c.full_clean(), batch, "EOS mismatch on {s:?}");
        }
    }

    #[test]
    fn language_tag_line_is_fully_held_until_its_newline() {
        let mut c = StreamCleaner::new();
        c.push("```te"); // could still be a language tag, not content
        assert_eq!(c.stable(), "");
        c.push("xt\nHELLO WORLD");
        assert!(c.stable().starts_with("HELLO"));
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Byte-level-BPE garble detection + repair (GitHub issue #84).
//!
//! Some OpenRouter providers occasionally return completions whose whitespace
//! tokens are NOT detokenized: a space arrives as `Ġ` (U+0120) and a newline as
//! `Ċ` (U+010A) — the GPT-2 `bytes_to_unicode` renderings of 0x20 / 0x0A. CJK
//! characters arrive as their literal glyphs, so in practice ONLY whitespace is
//! affected. We detect a high density of these two markers and repair them with
//! a safe two-substitution pass.
//!
//! We deliberately key on `Ġ`/`Ċ` ONLY — not the whole U+0100–U+017F byte-marker
//! range — because legitimate pinyin (e.g. `ā` U+0101) lives in that range and
//! must never be flagged or rewritten.

/// Density (in percent) of `Ġ`/`Ċ` glyphs above which a string is treated as
/// byte-BPE garble. A clean reply scores ~0; a garbled reply scores far higher
/// because every space/newline becomes a marker.
const GARBLE_PCT_THRESHOLD: usize = 3;

/// `Ġ` U+0120 — byte-level-BPE rendering of a space.
const GLYPH_SPACE: char = '\u{0120}';
/// `Ċ` U+010A — byte-level-BPE rendering of a newline.
const GLYPH_NEWLINE: char = '\u{010A}';

/// True when `s` carries a `Ġ`/`Ċ` density at or above [`GARBLE_PCT_THRESHOLD`].
pub fn looks_byte_garbled(s: &str) -> bool {
    let total = s.chars().count();
    if total == 0 {
        return false;
    }
    let mut markers = 0usize;
    let mut real_ws = 0usize;
    for c in s.chars() {
        match c {
            GLYPH_SPACE | GLYPH_NEWLINE => markers += 1,
            ' ' | '\n' | '\t' | '\r' => real_ws += 1,
            _ => {}
        }
    }
    // Genuine byte-BPE garble converts EVERY whitespace byte into a marker, so a
    // garbled string carries many Ġ/Ċ and ZERO real whitespace (verified against
    // all observed production rows). Requiring `real_ws == 0` excludes languages
    // where Ġ/Ċ are ordinary letters (e.g. Maltese "Ġurnata tajba"), which always
    // keep real spaces between words — density alone would misflag them.
    real_ws == 0 && markers * 100 >= total * GARBLE_PCT_THRESHOLD
}

/// Safe, idempotent repair: `Ġ`→space, `Ċ`→newline. No-op on clean text.
/// Does NOT attempt the full gpt2 `bytes_to_unicode` inverse (unsafe for pinyin).
pub fn repair_byte_bpe(s: &str) -> String {
    s.replace(GLYPH_SPACE, " ").replace(GLYPH_NEWLINE, "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_is_not_garbled() {
        assert!(!looks_byte_garbled(""));
    }

    #[test]
    fn clean_cjk_reply_is_not_garbled() {
        assert!(!looks_byte_garbled(
            "嗯，今天过得怎么样？我一直在想你说的那件事。"
        ));
    }

    #[test]
    fn clean_reply_with_pinyin_macron_is_not_garbled() {
        // `ā` (U+0101) sits in the byte-marker range but is NOT Ġ/Ċ, so the
        // detector must leave it alone — no false positive.
        assert!(!looks_byte_garbled("你的名字读作 māo 吗？真可爱。"));
    }

    #[test]
    fn maltese_letters_with_real_spaces_are_not_garbled() {
        // Ġ/Ċ are ordinary letters in Maltese; real text keeps real spaces between
        // words, so even a high marker density must NOT be flagged (the presence of
        // real whitespace proves the detokenizer did not mangle whitespace).
        assert!(!looks_byte_garbled("Ġurnata tajba, kif inti?"));
        assert!(!looks_byte_garbled("Ċaqlaq il-karozza Ġiet lura."));
    }

    #[test]
    fn dense_markers_are_garbled() {
        // "Hello there." rendered as byte-BPE: spaces -> Ġ.
        assert!(looks_byte_garbled("HelloĠthere.ĊHowĠareĠyou?"));
    }

    #[test]
    fn detector_threshold_is_exactly_three_percent() {
        // Pins the documented 3% contract. 3 markers in 100 chars = 3% → garbled;
        // 2 markers in 100 chars = 2% → clean.
        let at = "a".repeat(97) + "ĠĠĠ";
        assert!(looks_byte_garbled(&at), "3% must count as garbled");
        let below = "a".repeat(98) + "ĠĠ";
        assert!(!looks_byte_garbled(&below), "2% must count as clean");
    }

    #[test]
    fn repair_substitutes_space_and_newline() {
        assert_eq!(repair_byte_bpe("HelloĠthere.ĊBye"), "Hello there.\nBye");
    }

    #[test]
    fn repair_is_idempotent_on_clean_text() {
        let clean = "你好，世界。\nHello.";
        assert_eq!(repair_byte_bpe(clean), clean);
    }

    #[test]
    fn repair_leaves_pinyin_untouched() {
        assert_eq!(repair_byte_bpe("māo"), "māo");
    }

    #[test]
    fn issue_84_json_sample_round_trips() {
        // The issue's synthetic example: a JSON envelope with Ġ/Ċ markers.
        let garbled = "{ĊĠĠĠ\"reply\":Ġ\"HelloĠthere.\"Ċ}";
        let repaired = repair_byte_bpe(garbled);
        assert!(!looks_byte_garbled(&repaired));
        // Repaired text parses as JSON.
        let v: serde_json::Value =
            serde_json::from_str(&repaired).expect("valid JSON after repair");
        assert_eq!(v["reply"], "Hello there.");
    }
}

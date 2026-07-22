// SPDX-License-Identifier: AGPL-3.0-only
//! Streaming-safe application of `output_regex` rules.
//!
//! The pipeline used to buffer a whole reply whenever any model in the chain
//! had any `output_regex` rule, because [`crate::model_config::apply_output_regex`]
//! needs the complete text. With the real production config every chat model is
//! targeted by the bracket-strip rule, so *every* chat turn buffered and TTFT
//! became full-generation time.
//!
//! This module streams through those rules with a bounded holdback. Each rule
//! becomes a small streaming [`Transform`]; the transforms are chained in
//! declaration order, so the composition is exactly the sequential
//! `replace_all`-per-rule that `apply_output_regex` performs — respecting rule
//! order (e.g. a leading `[artifact]嗯…` has the bracket stripped first, then
//! the `^嗯` head rule sees the exposed `嗢…`). The load-bearing guarantee is
//! that concatenating everything a scrubber emits equals `apply_output_regex`
//! over the same full text; see the property test in this module.
//!
//! Rules whose shape isn't a recognized head-anchored or bracket-span pattern
//! degrade to full buffering (correct for any rule, just no TTFT win), so a
//! downstream deployment with an exotic pattern loses nothing versus today.

use regex_syntax::hir::{Class, Hir, HirKind, Look};

use crate::model_config::CompiledRegexRule;

/// First-N chars held while a `^`-anchored (head) rule might still match. A
/// head match longer than this streams before the rule can fire — the persist
/// path re-runs the whole-text apply, so only the wire briefly shows it
/// (fail-open, same philosophy as [`SPAN_HOLDBACK_CAP`]).
const HEAD_HOLDBACK: usize = 64;
/// Max chars held across an unterminated span open delimiter before flushing it
/// verbatim (a lone `[` in prose, never closed). Bounds the holdback window.
const SPAN_HOLDBACK_CAP: usize = 256;

/// The streaming strategy inferred for one compiled rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleShape {
    /// `^`-anchored (start-of-haystack): can only match a leading prefix.
    Head,
    /// A `open [^close]* close` bracket span (delete/replace a bounded region).
    Span { open: char, close: char },
    /// Anything else — must buffer fully to apply safely.
    Opaque,
}

/// Decode a HIR `Literal` that is exactly one `char`.
fn single_char_literal(hir: &Hir) -> Option<char> {
    if let HirKind::Literal(lit) = hir.kind() {
        let s = std::str::from_utf8(&lit.0).ok()?;
        let mut it = s.chars();
        let c = it.next()?;
        if it.next().is_none() {
            return Some(c);
        }
    }
    None
}

/// True iff `hir` is exactly the Unicode class `[^close]` — every scalar except
/// `close`. Verified by negating the class and checking the result is precisely
/// the single char `close`. This is what makes `SpanTransform`'s `open`→`close`
/// scan equivalent to the regex: the run between the delimiters must be
/// "anything but the close char" (not `.`, which excludes newline, and not a
/// bounded/other class).
fn class_is_not_char(hir: &Hir, close: char) -> bool {
    if let HirKind::Class(Class::Unicode(cls)) = hir.kind() {
        let mut negated = cls.clone();
        negated.negate();
        let ranges = negated.ranges();
        ranges.len() == 1 && ranges[0].start() == close && ranges[0].end() == close
    } else {
        false
    }
}

/// Classify a rule's regex pattern into a streaming strategy. Uses the parsed
/// HIR so the decision is structural, not string-guessing.
pub fn classify(pattern: &str) -> RuleShape {
    let Ok(hir) = regex_syntax::parse(pattern) else {
        return RuleShape::Opaque;
    };
    // A `^` (non-multiline) anchors every match to the start of the haystack.
    if hir.properties().look_set_prefix().contains(Look::Start) {
        return RuleShape::Head;
    }
    // A span rule must be EXACTLY `open [^close]* close`: Concat of a literal
    // open, a `*` (min 0, unbounded) repetition of the negated-close class, and
    // a literal close. Anything looser (`a.+b`, `a.*b`, a bounded `{2,5}`, a
    // capture group) is NOT what SpanTransform implements, so it stays Opaque
    // (buffered) rather than risk diverging from apply_output_regex on the wire.
    if let HirKind::Concat(parts) = hir.kind() {
        if parts.len() == 3 {
            if let (Some(open), HirKind::Repetition(rep), Some(close)) = (
                single_char_literal(&parts[0]),
                parts[1].kind(),
                single_char_literal(&parts[2]),
            ) {
                if open != close
                    && rep.min == 0
                    && rep.max.is_none()
                    && class_is_not_char(&rep.sub, close)
                {
                    return RuleShape::Span { open, close };
                }
            }
        }
    }
    RuleShape::Opaque
}

/// One streaming transform. `push` returns the text now safe to hand downstream;
/// `finish` flushes whatever remains held. The invariant every transform keeps:
/// the concatenation of all `push` outputs plus `finish` equals its regex
/// applied (via `replace_all`) to the concatenation of all `push` inputs.
trait Transform: Send {
    fn push(&mut self, input: &str) -> String;
    fn finish(&mut self) -> String;
}

/// `^`-anchored rule: hold the first [`HEAD_HOLDBACK`] chars, apply once (a
/// start-anchored rule matches at most the leading prefix), then pass the rest
/// through untouched.
struct HeadTransform {
    regex: regex::Regex,
    replacement: String,
    head: String,
    done: bool,
}

impl Transform for HeadTransform {
    fn push(&mut self, input: &str) -> String {
        if self.done {
            return input.to_string();
        }
        self.head.push_str(input);
        if self.head.chars().count() < HEAD_HOLDBACK {
            return String::new();
        }
        self.done = true;
        self.regex
            .replace_all(&std::mem::take(&mut self.head), self.replacement.as_str())
            .into_owned()
    }

    fn finish(&mut self) -> String {
        if self.done {
            return String::new();
        }
        self.done = true;
        self.regex
            .replace_all(&std::mem::take(&mut self.head), self.replacement.as_str())
            .into_owned()
    }
}

/// `open [^close]* close` rule: pass through until an `open`, then hold until the
/// matching `close` (emit the replacement for the whole span) or until
/// [`SPAN_HOLDBACK_CAP`] chars accumulate with no close (flush the held text
/// verbatim — a lone unterminated `open`).
struct SpanTransform {
    /// The full rule regex, run on the completed span so any replacement
    /// (including `$0`/capture expansion) matches apply_output_regex exactly.
    regex: regex::Regex,
    open: char,
    close: char,
    replacement: String,
    /// Text held since an unmatched `open` (includes the `open`). Empty when
    /// not inside a span.
    held: String,
    in_span: bool,
}

impl SpanTransform {
    fn feed(&mut self, input: &str, out: &mut String) {
        for ch in input.chars() {
            if self.in_span {
                if ch == self.close {
                    // Complete span: run the regex over the matched text so the
                    // replacement is expanded exactly as replace_all would, then
                    // drop the held region.
                    let mut full = std::mem::take(&mut self.held);
                    full.push(ch);
                    out.push_str(&self.regex.replace(&full, self.replacement.as_str()));
                    self.in_span = false;
                } else {
                    self.held.push(ch);
                    if self.held.chars().count() > SPAN_HOLDBACK_CAP {
                        // Unterminated: fail open — flush the held text verbatim
                        // and resume scanning (a later `open` can start a span).
                        out.push_str(&self.held);
                        self.held.clear();
                        self.in_span = false;
                    }
                }
            } else if ch == self.open {
                self.in_span = true;
                self.held.push(ch);
            } else {
                out.push(ch);
            }
        }
    }
}

impl Transform for SpanTransform {
    fn push(&mut self, input: &str) -> String {
        let mut out = String::new();
        self.feed(input, &mut out);
        out
    }

    fn finish(&mut self) -> String {
        // An open span never closed: emit the held text verbatim (the regex
        // requires a close, so an unterminated open is not a match).
        std::mem::take(&mut self.held)
    }
}

/// Fallback for any rule shape not safe to stream: buffer all input, apply the
/// regex over the whole at `finish`.
struct OpaqueTransform {
    regex: regex::Regex,
    replacement: String,
    buf: String,
}

impl Transform for OpaqueTransform {
    fn push(&mut self, input: &str) -> String {
        self.buf.push_str(input);
        String::new()
    }

    fn finish(&mut self) -> String {
        self.regex
            .replace_all(&std::mem::take(&mut self.buf), self.replacement.as_str())
            .into_owned()
    }
}

/// Applies a model's `output_regex` rules to a delta stream incrementally.
/// Build once per attempt with the rules that target the served model; feed
/// each delta through [`push`](Self::push) and flush with [`finish`](Self::finish).
pub struct StreamScrubber {
    transforms: Vec<Box<dyn Transform>>,
}

impl StreamScrubber {
    /// Build a scrubber for `model_id` from the full compiled rule set. Rules
    /// that don't target the model are skipped; the rest become transforms in
    /// declaration order. With no matching rule the scrubber is pure passthrough.
    pub fn new(rules: &[CompiledRegexRule], model_id: &str) -> Self {
        let mut transforms: Vec<Box<dyn Transform>> = Vec::new();
        for rule in rules {
            if !rule.models.iter().any(|m| m == model_id) {
                continue;
            }
            let t: Box<dyn Transform> = match classify(rule.regex.as_str()) {
                RuleShape::Head => Box::new(HeadTransform {
                    regex: rule.regex.clone(),
                    replacement: rule.replacement.clone(),
                    head: String::new(),
                    done: false,
                }),
                RuleShape::Span { open, close } => Box::new(SpanTransform {
                    regex: rule.regex.clone(),
                    open,
                    close,
                    replacement: rule.replacement.clone(),
                    held: String::new(),
                    in_span: false,
                }),
                RuleShape::Opaque => Box::new(OpaqueTransform {
                    regex: rule.regex.clone(),
                    replacement: rule.replacement.clone(),
                    buf: String::new(),
                }),
            };
            transforms.push(t);
        }
        Self { transforms }
    }

    /// Feed one delta; returns the text now safe to emit to the client (may be
    /// empty while text is held back).
    pub fn push(&mut self, delta: &str) -> String {
        let mut cur = delta.to_string();
        for t in &mut self.transforms {
            cur = t.push(&cur);
        }
        cur
    }

    /// Stream ended: flush every transform's held state, cascading each one's
    /// flush through the transforms after it.
    pub fn finish(&mut self) -> String {
        let mut cur = String::new();
        for t in &mut self.transforms {
            let pushed = t.push(&cur);
            let finished = t.finish();
            cur = pushed + &finished;
        }
        cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{apply_output_regex, CompiledRegexRule};

    fn compile(patterns: &[&str]) -> Vec<CompiledRegexRule> {
        patterns
            .iter()
            .map(|p| CompiledRegexRule {
                models: vec!["m".to_string()],
                regex: regex::Regex::new(p).unwrap(),
                replacement: String::new(),
            })
            .collect()
    }

    /// The real production patterns (from eros-engine-web/infra/engine/model_config.toml).
    fn production_rules() -> Vec<CompiledRegexRule> {
        compile(&[
            r"\[[^\]]*\]",
            r"^嗯(?:\.{3,6}|…{1,2})\s*",
            r"^(?:[(（][^)）]*[)）]|\.{3,6}\s*|…{1,2}\s*)",
        ])
    }

    #[test]
    fn classify_production_patterns() {
        assert_eq!(classify(r"^嗯(?:\.{3,6}|…{1,2})\s*"), RuleShape::Head);
        assert_eq!(
            classify(r"^(?:[(（][^)）]*[)）]|\.{3,6}\s*|…{1,2}\s*)"),
            RuleShape::Head
        );
        assert_eq!(
            classify(r"\[[^\]]*\]"),
            RuleShape::Span {
                open: '[',
                close: ']'
            }
        );
        assert_eq!(classify(r"(?s)<think>.*?</think>\s*"), RuleShape::Opaque);
        assert_eq!(classify(r"foo"), RuleShape::Opaque);
    }

    #[test]
    fn classify_rejects_non_span_three_part_concats() {
        // `.+` / `.*` are NOT `[^close]*` (a.+b needs ≥1 char; `.` excludes \n),
        // a bounded repetition is not `*`, and a capture group breaks the shape.
        // All must be Opaque so SpanTransform never mis-strips them.
        assert_eq!(classify(r"a.+b"), RuleShape::Opaque);
        assert_eq!(classify(r"a.*b"), RuleShape::Opaque);
        assert_eq!(classify(r"\[[^\]]{2,5}\]"), RuleShape::Opaque);
        assert_eq!(classify(r"\[([^\]]*)\]"), RuleShape::Opaque);
        // But the exact production shape stays Span.
        assert_eq!(
            classify(r"\[[^\]]*\]"),
            RuleShape::Span {
                open: '[',
                close: ']'
            }
        );
    }

    #[test]
    fn span_replacement_expansion_matches_whole_text_apply() {
        // A span rule with a non-empty $0-expanding replacement must stream the
        // same text apply_output_regex produces (the whole match, wrapped).
        let rules = vec![CompiledRegexRule {
            models: vec!["m".to_string()],
            regex: regex::Regex::new(r"\[[^\]]*\]").unwrap(),
            replacement: "<$0>".to_string(),
        }];
        let text = "a[x]b[y]c";
        let expected = apply_output_regex(&rules, "m", text).cleaned;
        for split in 0..=text.chars().count() {
            let chars: Vec<char> = text.chars().collect();
            let mut s = StreamScrubber::new(&rules, "m");
            let a: String = chars[..split].iter().collect();
            let b: String = chars[split..].iter().collect();
            let mut got = s.push(&a);
            got.push_str(&s.push(&b));
            got.push_str(&s.finish());
            assert_eq!(got, expected, "split={split}");
        }
    }

    /// The load-bearing guarantee: for every chunk-split point, the scrubbed
    /// stream concatenation equals the whole-text apply (modulo the whitespace
    /// collapse apply_output_regex does at the persist layer, which the scrubber
    /// deliberately does not — see the trailing-artifact case).
    #[test]
    fn scrubbed_stream_equals_whole_text_apply_for_every_split() {
        let rules = production_rules();
        let cases = [
            "你好呀[你给对方发送了一张照片：海边]今天如何",
            "嗯......其实我在想你",
            "（轻轻靠近）说吧",
            "[你给对方发送了一张照片：自拍]",
            "多个[a]中间[b]结尾",
            "[bracket]嗯...紧接着说", // order: span strips bracket, then head sees 嗯
            "no artifacts here at all",
        ];
        for text in cases {
            let expected = apply_output_regex(&rules, "m", text).cleaned;
            let chars: Vec<char> = text.chars().collect();
            for split in 0..=chars.len() {
                let mut s = StreamScrubber::new(&rules, "m");
                let a: String = chars[..split].iter().collect();
                let b: String = chars[split..].iter().collect();
                let mut got = s.push(&a);
                got.push_str(&s.push(&b));
                got.push_str(&s.finish());
                assert_eq!(got, expected, "text={text:?} split={split}");
            }
        }
    }

    #[test]
    fn artifact_only_reply_streams_to_empty() {
        let rules = production_rules();
        let mut s = StreamScrubber::new(&rules, "m");
        let mut got = s.push("[你给对方发送了一张照片：自拍]");
        got.push_str(&s.finish());
        assert_eq!(got, "", "an artifact-only reply must emit nothing");
    }

    #[test]
    fn trailing_artifact_keeps_body() {
        // apply_output_regex collapses a whitespace-only *stripped* result to
        // empty at the persist layer; the scrubber keeps real body text.
        let rules = production_rules();
        let mut s = StreamScrubber::new(&rules, "m");
        let mut got = s.push("正文[你给对方发送了一张照片：床上]");
        got.push_str(&s.finish());
        assert_eq!(got, "正文");
    }

    #[test]
    fn unterminated_bracket_flushes_verbatim() {
        let rules = production_rules();
        // A lone `[` with a very long tail and no `]` must fail open, not hang.
        let text = format!("start[{}", "x".repeat(SPAN_HOLDBACK_CAP + 50));
        let expected = apply_output_regex(&rules, "m", &text).cleaned;
        let mut s = StreamScrubber::new(&rules, "m");
        let mut got = s.push(&text);
        got.push_str(&s.finish());
        assert_eq!(got, expected);
        assert!(got.contains('['), "unterminated open flushes verbatim");
    }

    #[test]
    fn non_targeted_model_is_passthrough() {
        let rules = production_rules();
        let mut s = StreamScrubber::new(&rules, "other/model");
        let got = s.push("verbatim [not stripped] text");
        assert_eq!(got, "verbatim [not stripped] text");
    }

    #[test]
    fn opaque_rule_matches_whole_text_apply() {
        let rules = compile(&[r"(?s)<think>.*?</think>\s*"]);
        let text = "before<think>hidden</think>after";
        let expected = apply_output_regex(&rules, "m", text).cleaned;
        let mut s = StreamScrubber::new(&rules, "m");
        // Everything buffers; nothing emitted until finish.
        assert_eq!(s.push("before<think>hid"), "");
        let mut got = s.push("den</think>after");
        got.push_str(&s.finish());
        assert_eq!(got, expected);
    }
}

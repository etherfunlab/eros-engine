// SPDX-License-Identifier: AGPL-3.0-only
//! Streaming-safe application of `output_regex` rules.
//!
//! The pipeline used to buffer a whole reply whenever any model in the chain
//! had any `output_regex` rule, because [`crate::model_config::apply_output_regex`]
//! needs the complete text. With the real production config every chat model is
//! targeted by the bracket-strip rule, so *every* chat turn buffered and TTFT
//! became full-generation time.
//!
//! This module streams through those rules with a hold-until-decidable
//! discipline. Each rule becomes a small streaming [`Transform`]; the
//! transforms are chained in declaration order, so the composition is exactly
//! the sequential `replace_all`-per-rule that `apply_output_regex` performs —
//! respecting rule order (e.g. a leading `[artifact]嗯…` has the bracket
//! stripped first, then the `^嗯` head rule sees the exposed `嗢…`).
//!
//! The load-bearing guarantee is twofold: (1) concatenating everything a
//! scrubber emits equals `apply_output_regex` over the same full text (the
//! property test in this module), and (2) the wire NEVER shows text the
//! whole-text apply would strip — text is held while a match is still viable
//! and released only once the match is decided (applied, provably dead, or
//! end-of-stream). Holds are bounded by the reply itself (max_tokens-capped);
//! there is no fixed cap that could flush a still-viable match to the client.
//!
//! Rules whose shape isn't a recognized head-anchored or bracket-span pattern
//! degrade to full buffering (correct for any rule, just no TTFT win), so a
//! downstream deployment with an exotic pattern loses nothing versus today.

use regex_syntax::hir::{Class, Hir, HirKind, Look};

use crate::model_config::CompiledRegexRule;

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

/// Max match length (bytes) after which a `^`-anchored rule's matchability is
/// fully decided, computed by stripping TRAILING nullable elements (e.g. a
/// trailing `\s*`) from the top-level concat and taking the remainder's
/// `maximum_len`. Rationale: the stripped tail matches empty, so a match
/// exists iff the bounded "core" matches — and an anchored core match lies
/// entirely within the first `bound` bytes. `None` = matchability can stay
/// open-ended (e.g. an inner unbounded class); the transform then holds until
/// a match completes or end-of-stream.
fn head_decision_bound(hir: &Hir) -> Option<usize> {
    if let HirKind::Concat(parts) = hir.kind() {
        let mut end = parts.len();
        while end > 0 && parts[end - 1].properties().minimum_len() == Some(0) {
            end -= 1;
        }
        Hir::concat(parts[..end].to_vec())
            .properties()
            .maximum_len()
    } else {
        hir.properties().maximum_len()
    }
}

/// Classify a rule's regex pattern into a streaming strategy. Uses the parsed
/// HIR so the decision is structural, not string-guessing. Head additionally
/// reports the decision bound (see [`head_decision_bound`]).
pub fn classify(pattern: &str) -> RuleShape {
    let Ok(hir) = regex_syntax::parse(pattern) else {
        return RuleShape::Opaque;
    };
    // Head: every match is anchored to the start of the haystack, and the
    // pattern contains NO other look-around (`$`/`\z` would make matches
    // depend on where the haystack ENDS — undecidable mid-stream; `\b` at the
    // tail flips when the next char arrives). `look_set()` == {Start} exactly.
    let looks = hir.properties().look_set();
    if looks == regex_syntax::hir::LookSet::singleton(Look::Start)
        && hir.properties().look_set_prefix().contains(Look::Start)
    {
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

/// `^`-anchored rule (whole `look_set` is exactly {Start}): hold input until
/// the leading match is DECIDED, then apply once and pass everything after
/// through untouched. A decision is reached when:
///
/// - a match exists and ends strictly BEFORE the buffer end (greedy/lazy
///   extension already stopped at a real char, alternation preference is
///   structural, and with no `$`/`\b` in the pattern no later input can alter
///   the leftmost-first anchored match) → apply + emit; or
/// - no match exists and the buffer exceeds [`head_decision_bound`] (an
///   anchored core match would have to lie entirely within that many bytes)
///   → provably dead, emit verbatim; or
/// - end-of-stream → apply to whatever is held.
///
/// A match that reaches exactly the buffer end stays held (it may still grow —
/// e.g. a trailing `\s*` mid-run). Never emits a prefix of a still-viable
/// match, so the wire can never show text the whole-text apply would strip.
struct HeadTransform {
    regex: regex::Regex,
    replacement: String,
    /// Byte bound past which no-match is final; `None` = only a completed
    /// match or end-of-stream decides.
    decision_bound: Option<usize>,
    head: String,
    done: bool,
}

impl Transform for HeadTransform {
    fn push(&mut self, input: &str) -> String {
        if self.done {
            return input.to_string();
        }
        self.head.push_str(input);
        match self.regex.find(&self.head) {
            Some(m) if m.end() < self.head.len() => {
                self.done = true;
                self.regex
                    .replace_all(&std::mem::take(&mut self.head), self.replacement.as_str())
                    .into_owned()
            }
            Some(_) => String::new(), // match may still grow — keep holding
            None => {
                if self.decision_bound.is_some_and(|b| self.head.len() > b) {
                    // Anchored core can no longer match: dead, stream verbatim.
                    self.done = true;
                    std::mem::take(&mut self.head)
                } else {
                    String::new()
                }
            }
        }
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

/// `open [^close]* close` rule: pass through until an `open`, then hold until
/// the matching `close` (emit the replacement for the whole span) or
/// end-of-stream (flush verbatim — the regex requires a close, so an
/// unterminated open can never match). There is deliberately NO holdback cap:
/// a cap would flush the prefix of a still-viable long span to the client that
/// the whole-text apply strips — a wire leak. Held memory is bounded by the
/// reply itself (max_tokens-capped); the latency cost hits only the
/// pathological lone-`open` reply, whose tail is delayed to end-of-stream.
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
                    decision_bound: regex_syntax::parse(rule.regex.as_str())
                        .ok()
                        .as_ref()
                        .and_then(head_decision_bound),
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
    fn unterminated_bracket_flushes_verbatim_at_finish() {
        let rules = production_rules();
        // A lone `[` with a very long tail and no `]`: held to end-of-stream
        // (it could still close), then flushed verbatim — never a mis-strip.
        let text = format!("start[{}", "x".repeat(400));
        let expected = apply_output_regex(&rules, "m", &text).cleaned;
        let mut s = StreamScrubber::new(&rules, "m");
        let mut got = s.push(&text);
        got.push_str(&s.finish());
        assert_eq!(got, expected);
        assert!(got.contains('['), "unterminated open flushes verbatim");
    }

    #[test]
    fn long_span_is_stripped_never_leaked() {
        // Codex series-review P1: a VALID span longer than any fixed cap must
        // be stripped on the wire exactly like the whole-text apply — a cap
        // that flushed its prefix leaked strippable content to the client.
        let rules = production_rules();
        let text = format!("前文[{}]后文", "藏".repeat(300));
        let expected = apply_output_regex(&rules, "m", &text).cleaned;
        assert_eq!(expected, "前文后文", "sanity: whole-text apply strips it");
        // Feed char-by-char — worst-case chunking.
        let mut s = StreamScrubber::new(&rules, "m");
        let mut got = String::new();
        for ch in text.chars() {
            got.push_str(&s.push(&ch.to_string()));
            assert!(
                !got.contains('藏'),
                "span interior must never reach the wire"
            );
        }
        got.push_str(&s.finish());
        assert_eq!(got, expected);
    }

    #[test]
    fn end_anchored_head_pattern_is_opaque() {
        // Codex series-review P1: `^...$` matches depend on where the haystack
        // ENDS — undecidable mid-stream. Must classify Opaque (full buffering),
        // not Head (the old 64-char finalize mis-stripped a 64-char prefix that
        // happened to look like a whole-reply artifact).
        assert_eq!(classify(r"^\s*\[[^\]]*\]\s*$"), RuleShape::Opaque);
        assert_eq!(classify(r"^foo$"), RuleShape::Opaque);
        assert_eq!(classify(r"^foo\b"), RuleShape::Opaque);
        // And the equivalence holds via the Opaque path on a long reply whose
        // 64-char prefix WOULD have mis-matched under the old finalize.
        let rules = compile(&[r"^\s*\[[^\]]*\]\s*$"]);
        let text = format!("[{}]还有正文在后面", "x".repeat(80));
        let expected = apply_output_regex(&rules, "m", &text).cleaned;
        assert_eq!(expected, text, "sanity: whole-text apply does NOT match");
        let mut s = StreamScrubber::new(&rules, "m");
        let mut got = s.push(&text);
        got.push_str(&s.finish());
        assert_eq!(got, expected, "no mid-stream mis-strip");
    }

    #[test]
    fn head_long_match_strips_exactly() {
        // A head match extending far past the old 64-char holdback must still
        // strip exactly (the old finalize left the excess whitespace on the
        // wire). Also covers the >64-char paren action marker.
        let rules = production_rules();
        let cases = [
            format!("嗯...{}正文继续", " ".repeat(80)),
            format!("（{}）好了", "很长的动作描写".repeat(15)),
        ];
        for text in &cases {
            let expected = apply_output_regex(&rules, "m", text).cleaned;
            let chars: Vec<char> = text.chars().collect();
            for split in [0, 1, 5, 40, 70, chars.len()] {
                let split = split.min(chars.len());
                let mut s = StreamScrubber::new(&rules, "m");
                let a: String = chars[..split].iter().collect();
                let b: String = chars[split..].iter().collect();
                let mut got = s.push(&a);
                got.push_str(&s.push(&b));
                got.push_str(&s.finish());
                assert_eq!(&got, &expected, "text={text:?} split={split}");
            }
        }
    }

    #[test]
    fn head_dead_prefix_streams_promptly() {
        // "嗯嗯..." has a viable first char but the pattern dies at the second
        // 嗯 (needs dots). The decision bound must release it during streaming,
        // not hold the whole reply to end-of-stream.
        let rules = compile(&[r"^嗯(?:\.{3,6}|…{1,2})\s*"]);
        let mut s = StreamScrubber::new(&rules, "m");
        let mut streamed = String::new();
        // 40 chars, pushed in 4 chunks — decision bound for the stripped core
        // (嗯 + 6 ascii dots ≈ 9 bytes) is crossed within the first chunk.
        for chunk in ["嗯嗯今天天气不错吧", "我们出去走走", "怎么样呢", "好不好呀"]
        {
            streamed.push_str(&s.push(chunk));
        }
        assert!(
            !streamed.is_empty(),
            "dead head prefix must stream before end-of-stream"
        );
        streamed.push_str(&s.finish());
        assert_eq!(streamed, "嗯嗯今天天气不错吧我们出去走走怎么样呢好不好呀");
    }

    #[test]
    fn head_bounded_pattern_decides_past_bound() {
        // `^a{100}` (bounded, no trailing nullable): no match at 64 chars must
        // NOT finalize (the old code did, then bypassed the strip). With 100
        // a's + tail, the match completes and strips exactly.
        let rules = compile(&[r"^a{100}"]);
        let text = format!("{}tail", "a".repeat(100));
        let expected = apply_output_regex(&rules, "m", &text).cleaned;
        assert_eq!(expected, "tail");
        let mut s = StreamScrubber::new(&rules, "m");
        let mut got = String::new();
        for ch in text.chars() {
            got.push_str(&s.push(&ch.to_string()));
        }
        got.push_str(&s.finish());
        assert_eq!(got, expected);
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

// SPDX-License-Identifier: AGPL-3.0-only
//! Speech markup for chat messages: her stored lines, prepared for a
//! text-to-speech voice.
//!
//! Spec: docs/superpowers/specs/2026-11-27-chat-speech-markup-design.md

use eros_engine_core::scope::AffinityScope;
use eros_engine_llm::model_config::{
    SPEECH_MODE_REWRITE, SPEECH_MODE_VERBATIM, SPEECH_TAGS_GUIDANCE,
};
use eros_engine_llm::openrouter::ChatMessage as WireMessage;

const TONE_EMOTIONAL: &str =
    "She said this in the grip of a feeling, unguarded: let it show in the delivery.";
const TONE_EROTIC_ADDS: &str = "She wants the other person: soft, slow, close, alluring.";
const TONE_FOCUSED: &str = "She said this with her mind on her work: brisk, even, brief.";

/// The one sentence that tells the model which tone the line was written in
/// (spec §5.1), from the row's `metadata.tone`. `erotic` is a superset of
/// `emotional` here as in the chat prompt. `default`, an unknown value and no
/// key say nothing.
pub(crate) fn tone_sentence(tone: Option<&str>) -> Option<String> {
    match tone? {
        "emotional" => Some(TONE_EMOTIONAL.to_string()),
        "erotic" => Some(format!("{TONE_EMOTIONAL} {TONE_EROTIC_ADDS}")),
        "focused" => Some(TONE_FOCUSED.to_string()),
        _ => None,
    }
}

/// The affinity scope the row recorded (`metadata.affinity_scope`); a row with
/// none, or with one that does not read, takes the chat stream's default
/// scope (spec §5.2).
pub(crate) fn row_scope(metadata: Option<&serde_json::Value>) -> AffinityScope {
    metadata
        .and_then(|m| m.get("affinity_scope"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

/// What one line is judged from (spec §5).
pub(crate) struct SpeechInput<'a> {
    /// The persona's authored system prompt.
    pub persona_head: &'a str,
    /// The base directive: the deployment's, else the built-in one.
    pub base: &'a str,
    /// Whether the line may be rephrased (spec §6.2).
    pub rewrite: bool,
    /// The row's `metadata.tone`.
    pub tone: Option<&'a str>,
    /// The relationship line, already rendered.
    pub relationship: Option<&'a str>,
    /// The rows before the line, oldest first, as `(role, content)`.
    pub earlier: &'a [(String, String)],
    /// The stored text of the message.
    pub line: &'a str,
}

/// The request's two messages. System: the persona head, the base directive,
/// the mode sentence and the tag guidance, then the tone sentence and the
/// relationship line when there are any. User: the earlier rows as a plain
/// transcript (blank rows skipped; the block left out when none remain), then
/// the line.
pub(crate) fn build_speech_messages(i: &SpeechInput) -> Vec<WireMessage> {
    let mode = if i.rewrite {
        SPEECH_MODE_REWRITE
    } else {
        SPEECH_MODE_VERBATIM
    };
    let mut system = vec![
        i.persona_head.to_string(),
        i.base.to_string(),
        mode.to_string(),
        SPEECH_TAGS_GUIDANCE.to_string(),
    ];
    system.extend(tone_sentence(i.tone));
    system.extend(i.relationship.map(str::to_string));

    let transcript: Vec<String> = i
        .earlier
        .iter()
        .filter(|(_, content)| !content.trim().is_empty())
        .map(|(role, content)| {
            let who = if role == "assistant" { "Her" } else { "User" };
            format!("{who}: {content}")
        })
        .collect();
    let mut user = String::new();
    if !transcript.is_empty() {
        user.push_str("Earlier lines (context only):\n");
        user.push_str(&transcript.join("\n"));
        user.push_str("\n\n");
    }
    user.push_str("The line to prepare:\n");
    user.push_str(i.line);

    vec![
        WireMessage {
            role: "system".into(),
            content: system.join("\n\n"),
        },
        WireMessage {
            role: "user".into(),
            content: user,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use eros_engine_llm::model_config::{
        DEFAULT_SPEECH_BASE, SPEECH_MODE_REWRITE, SPEECH_MODE_VERBATIM, SPEECH_TAGS_GUIDANCE,
    };

    const EMOTIONAL: &str =
        "She said this in the grip of a feeling, unguarded: let it show in the delivery.";
    const WANTS: &str = "She wants the other person: soft, slow, close, alluring.";
    const FOCUSED: &str = "She said this with her mind on her work: brisk, even, brief.";

    #[test]
    fn the_tone_sentences() {
        assert_eq!(tone_sentence(Some("emotional")).as_deref(), Some(EMOTIONAL));
        assert_eq!(
            tone_sentence(Some("erotic")),
            Some(format!("{EMOTIONAL} {WANTS}"))
        );
        assert_eq!(tone_sentence(Some("focused")).as_deref(), Some(FOCUSED));
        for none in [None, Some("default"), Some("cheerful"), Some("")] {
            assert_eq!(tone_sentence(none), None, "{none:?}");
        }
    }

    #[test]
    fn the_row_scope_defaults() {
        let full = AffinityScope::full();
        assert_eq!(row_scope(None), AffinityScope::default());
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"affinity_scope": 7}),
            serde_json::json!({"affinity_scope": null}),
        ] {
            assert_eq!(row_scope(Some(&bad)), AffinityScope::default(), "{bad}");
        }
        let stored = serde_json::json!({"affinity_scope": serde_json::to_value(full).unwrap()});
        assert_eq!(row_scope(Some(&stored)), full);
    }

    fn input<'a>(
        rewrite: bool,
        tone: Option<&'a str>,
        relationship: Option<&'a str>,
        earlier: &'a [(String, String)],
    ) -> SpeechInput<'a> {
        SpeechInput {
            persona_head: "PERSONA-HEAD",
            base: DEFAULT_SPEECH_BASE,
            rewrite,
            tone,
            relationship,
            earlier,
            line: "你怎么才来😊",
        }
    }

    #[test]
    fn the_request_is_a_system_and_a_user_message() {
        let m = build_speech_messages(&input(false, None, None, &[]));
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].role.as_str(), m[1].role.as_str()), ("system", "user"));
        assert_eq!(
            m[0].content,
            format!(
                "PERSONA-HEAD\n\n{DEFAULT_SPEECH_BASE}\n\n{SPEECH_MODE_VERBATIM}\n\n{SPEECH_TAGS_GUIDANCE}"
            )
        );

        // `rewrite` swaps the mode sentence and nothing else.
        let r = build_speech_messages(&input(true, None, None, &[]));
        assert_eq!(
            r[0].content,
            m[0].content
                .replace(SPEECH_MODE_VERBATIM, SPEECH_MODE_REWRITE)
        );
        assert_eq!(r[1].content, m[1].content);

        // The tone sentence, then the relationship line, after the guidance.
        let both = build_speech_messages(&input(false, Some("erotic"), Some("REL-LINE"), &[]));
        assert_eq!(
            both[0].content,
            format!("{}\n\n{EMOTIONAL} {WANTS}\n\nREL-LINE", m[0].content)
        );
        let tone_only = build_speech_messages(&input(false, Some("focused"), None, &[]));
        assert_eq!(
            tone_only[0].content,
            format!("{}\n\n{FOCUSED}", m[0].content)
        );
        let rel_only = build_speech_messages(&input(false, Some("default"), Some("REL-LINE"), &[]));
        assert_eq!(rel_only[0].content, format!("{}\n\nREL-LINE", m[0].content));
    }

    #[test]
    fn the_transcript_skips_blank_rows_and_may_be_empty() {
        let none = build_speech_messages(&input(false, None, None, &[]));
        assert_eq!(none[1].content, "The line to prepare:\n你怎么才来😊");

        let earlier = [
            ("user".to_string(), "在吗".to_string()),
            ("user".to_string(), "  ".to_string()),
            ("assistant".to_string(), "在呀".to_string()),
        ];
        let some = build_speech_messages(&input(false, None, None, &earlier));
        assert_eq!(
            some[1].content,
            "Earlier lines (context only):\nUser: 在吗\nHer: 在呀\n\nThe line to prepare:\n你怎么才来😊"
        );

        // Only blank rows: as none at all.
        let blank = [("user".to_string(), String::new())];
        assert_eq!(
            build_speech_messages(&input(false, None, None, &blank))[1].content,
            none[1].content
        );
    }
}

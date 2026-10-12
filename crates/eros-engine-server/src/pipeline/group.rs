// SPDX-License-Identifier: AGPL-3.0-only

//! Group chat turns (spec 2026-10-12-group-chat-design.md §5): how one
//! speaker sees the group, and the turn that casts and relays the speakers.

// `group_block` and `transcript_text` are called by the group turn (Task 11
// of the group-chat plan), which lands in this module next.
#![allow(dead_code)]

use std::collections::HashMap;

use eros_engine_core::group::{render_line, LineSpeaker, UNKNOWN_SPEAKER};
use eros_engine_store::chat::ChatMessage;
use uuid::Uuid;

use crate::repetition::Injected;

/// One speaker's view of the group, handed to `build_reply_request_with`.
pub(crate) struct GroupSpeakerView {
    pub group_session_id: Uuid,
    pub speaker_session_id: Uuid,
    /// Persona names by member session, for every speaker in the window.
    pub names: HashMap<Uuid, String>,
    /// The `[群聊]` block appended to the system prompt.
    pub block: String,
}

impl GroupSpeakerView {
    fn name_of(&self, s: Uuid) -> &str {
        self.names
            .get(&s)
            .map(String::as_str)
            .unwrap_or(UNKNOWN_SPEAKER)
    }
}

/// Relabel the model-facing history for one speaker. `raw` is the history
/// without the noise strip and `stripped` with it; own lines take the
/// stripped text (dropped if the strip dropped them), everyone else's keep
/// the raw text.
pub(crate) fn speaker_view(
    raw: Vec<Injected>,
    stripped: Vec<Injected>,
    speakers: &HashMap<Uuid, Option<Uuid>>,
    view: &GroupSpeakerView,
) -> Vec<Injected> {
    let own: HashMap<Uuid, String> = stripped.into_iter().map(|m| (m.id, m.text)).collect();
    raw.into_iter()
        .filter_map(|m| {
            let speaker = speakers.get(&m.id).copied().flatten();
            let line = match (m.role.as_str(), speaker) {
                ("user", _) => LineSpeaker::User,
                ("assistant", Some(s)) if s == view.speaker_session_id => LineSpeaker::Me,
                ("assistant", Some(s)) => LineSpeaker::Other(view.name_of(s)),
                ("assistant", None) => LineSpeaker::Other(UNKNOWN_SPEAKER),
                _ => return None,
            };
            let text = match line {
                LineSpeaker::Me => own.get(&m.id)?.clone(),
                _ => m.text,
            };
            if text.trim().is_empty() {
                return None;
            }
            let (role, text) = render_line(line, &text);
            Some(Injected {
                id: m.id,
                role: role.to_string(),
                text,
            })
        })
        .collect()
}

/// The speaker's `[群聊]` block. `others` are the other seated members'
/// names and one-line personalities.
pub(crate) fn group_block(title: Option<&str>, others: &[(String, Option<String>)]) -> String {
    let mut s = String::from("\n\n[群聊]\n");
    match title {
        Some(t) => s.push_str(&format!("你现在在群聊「{t}」里。\n")),
        None => s.push_str("你现在在一个群聊里。\n"),
    }
    if others.is_empty() {
        s.push_str("群里只有你和对方。\n");
    } else {
        s.push_str("在场的还有：\n");
        for (name, personality) in others {
            match personality
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
            {
                Some(p) => s.push_str(&format!("- {name}：{p}\n")),
                None => s.push_str(&format!("- {name}\n")),
            }
        }
    }
    s.push_str("只说你自己的话，不替别人说话，不在开头写名字。");
    s
}

/// The director's and the composer's plain-text transcript.
pub(crate) fn transcript_text(rows: &[ChatMessage], names: &HashMap<Uuid, String>) -> String {
    rows.iter()
        .filter(|m| m.channel.is_none())
        .filter_map(|m| match m.role.as_str() {
            "user" => Some(format!("用户：{}", m.content)),
            "assistant" => {
                let name = m
                    .speaker_session_id
                    .and_then(|s| names.get(&s))
                    .map(String::as_str)
                    .unwrap_or(UNKNOWN_SPEAKER);
                let text = if m.content.trim().is_empty() {
                    "（发了一张照片）"
                } else {
                    m.content.as_str()
                };
                Some(format!("{name}：{text}"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repetition::Injected;

    fn inj(id: Uuid, role: &str, text: &str) -> Injected {
        Injected {
            id,
            role: role.into(),
            text: text.into(),
        }
    }

    #[test]
    fn speaker_view_relabels_by_speaker() {
        let (me, other) = (Uuid::new_v4(), Uuid::new_v4());
        let (u, a_me, a_other, a_gone) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let raw = vec![
            inj(u, "user", "早"),
            inj(a_other, "assistant", "早上好呀。今天去哪"),
            inj(a_me, "assistant", "哈喽。我也在"),
            inj(a_gone, "assistant", "我先走了"),
        ];
        // The noise strip only ever applies to the speaker's own lines.
        let stripped = vec![
            inj(u, "user", "早"),
            inj(a_other, "assistant", "今天去哪"),
            inj(a_me, "assistant", "我也在"),
        ];
        let speakers = HashMap::from([
            (u, None),
            (a_other, Some(other)),
            (a_me, Some(me)),
            (a_gone, None),
        ]);
        let view = GroupSpeakerView {
            group_session_id: Uuid::new_v4(),
            speaker_session_id: me,
            names: HashMap::from([(other, "小美".to_string())]),
            block: String::new(),
        };
        let out = speaker_view(raw, stripped, &speakers, &view);
        let pairs: Vec<(&str, &str)> = out
            .iter()
            .map(|m| (m.role.as_str(), m.text.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("user", "用户：早"),
                ("user", "小美：早上好呀。今天去哪"),
                ("assistant", "我也在"),
                ("user", "某位群友：我先走了"),
            ]
        );
    }

    #[test]
    fn speaker_view_drops_own_lines_the_strip_emptied() {
        let me = Uuid::new_v4();
        let a = Uuid::new_v4();
        let raw = vec![inj(a, "assistant", "嗯。")];
        let stripped = vec![]; // model_facing_history dropped it
        let speakers = HashMap::from([(a, Some(me))]);
        let view = GroupSpeakerView {
            group_session_id: Uuid::new_v4(),
            speaker_session_id: me,
            names: HashMap::new(),
            block: String::new(),
        };
        assert!(speaker_view(raw, stripped, &speakers, &view).is_empty());
    }

    #[test]
    fn group_block_is_byte_exact() {
        let b = group_block(
            Some("周末爬山"),
            &[("小美".into(), Some("活泼".into())), ("阿哲".into(), None)],
        );
        assert_eq!(
            b,
            "\n\n[群聊]\n你现在在群聊「周末爬山」里。\n在场的还有：\n- 小美：活泼\n- 阿哲\n只说你自己的话，不替别人说话，不在开头写名字。"
        );
        let alone = group_block(None, &[]);
        assert_eq!(
            alone,
            "\n\n[群聊]\n你现在在一个群聊里。\n群里只有你和对方。\n只说你自己的话，不替别人说话，不在开头写名字。"
        );
    }
}

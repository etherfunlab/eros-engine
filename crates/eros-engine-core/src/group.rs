// SPDX-License-Identifier: AGPL-3.0-only

//! Group chat, the pure half: what the director may return, how its answer
//! becomes an ordered list of cues, and how a group transcript line reads to
//! one speaker. See docs/superpowers/specs/2026-10-12-group-chat-design.md.

use serde::{Deserialize, Serialize};

use crate::types::{ActionType, ReplyMode};

/// Personas per group, not counting the user.
pub const MAX_GROUP_MEMBERS: usize = 3;
/// Longest group title, in chars, after trimming.
pub const GROUP_TITLE_MAX_CHARS: usize = 64;
/// Name for a line whose speaker session no longer exists.
pub const UNKNOWN_SPEAKER: &str = "某位群友";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectorAction {
    ReplyText,
    ReplyImage,
    ReplyTextImage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectorEntry {
    pub member: String,
    pub action: DirectorAction,
    #[serde(default)]
    pub inner_state: Option<String>,
    #[serde(default)]
    pub reply_mode: Option<ReplyMode>,
    #[serde(default)]
    pub clothing: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectorVerdict {
    pub speakers: Vec<DirectorEntry>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// One speaker's turn: which seat, what it does, and the director's prose for
/// it (unsanitised — the server sanitises before it reaches a prompt).
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub seat: usize,
    pub action: ActionType,
    pub inner_state: Option<String>,
    pub reply_mode: Option<ReplyMode>,
    pub clothing: Option<String>,
}

/// Who said a transcript line, from the point of view of the speaker the
/// prompt is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineSpeaker<'a> {
    User,
    Me,
    Other(&'a str),
}

/// The director's name for a seat: `m1`, `m2`, `m3`.
pub fn handle(seat: usize) -> String {
    format!("m{}", seat + 1)
}

fn seat_of(member: &str, seats: usize) -> Option<usize> {
    let n: usize = member.trim().strip_prefix('m')?.parse().ok()?;
    (1..=seats).contains(&n).then(|| n - 1)
}

/// Parse the director's content. Tolerates a ```json fence; anything else
/// that is not the JSON object is `None`.
pub fn parse_verdict(text: &str) -> Option<DirectorVerdict> {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    serde_json::from_str(t).ok()
}

/// Turn a verdict into cues: unknown or unseated handles are dropped, a
/// repeated handle keeps its first entry, and an image action on a turn
/// without image availability degrades to text (an action only goes down).
/// An empty result is the caller's cue to fall back.
pub fn normalize(v: &DirectorVerdict, seats: usize, image_available: bool) -> Vec<Cue> {
    let mut seen = vec![false; seats];
    let mut out = Vec::new();
    for e in &v.speakers {
        let Some(seat) = seat_of(&e.member, seats) else {
            continue;
        };
        if std::mem::replace(&mut seen[seat], true) {
            continue;
        }
        let action = match (e.action, image_available) {
            (DirectorAction::ReplyImage, true) => ActionType::ReplyImage,
            (DirectorAction::ReplyTextImage, true) => ActionType::ReplyTextImage,
            _ => ActionType::ReplyText,
        };
        out.push(Cue {
            seat,
            action,
            inner_state: e.inner_state.clone(),
            reply_mode: e.reply_mode,
            clothing: e.clothing.clone(),
        });
    }
    out
}

/// No director, or one that gave nothing usable: every seat answers with
/// text, in join order.
pub fn fallback_cues(seats: usize) -> Vec<Cue> {
    (0..seats)
        .map(|seat| Cue {
            seat,
            action: ActionType::ReplyText,
            inner_state: None,
            reply_mode: None,
            clothing: None,
        })
        .collect()
}

/// One transcript line as the speaker's model sees it: its own lines stay
/// `assistant`; everyone else's become `user` with a name prefix.
pub fn render_line(speaker: LineSpeaker<'_>, text: &str) -> (&'static str, String) {
    match speaker {
        LineSpeaker::User => ("user", format!("用户：{text}")),
        LineSpeaker::Me => ("assistant", text.to_string()),
        LineSpeaker::Other(name) => ("user", format!("{name}：{text}")),
    }
}

/// A caller-supplied group title, trimmed (Unicode whitespace, so a title of
/// full-width spaces is blank), or `None` when blank, too long, or carrying a
/// control character.
pub fn normalize_title(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.chars().count() > GROUP_TITLE_MAX_CHARS || t.chars().any(char::is_control)
    {
        return None;
    }
    Some(t.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(member: &str, action: DirectorAction) -> DirectorEntry {
        DirectorEntry {
            member: member.into(),
            action,
            inner_state: None,
            reply_mode: None,
            clothing: None,
        }
    }

    #[test]
    fn handles_are_one_based() {
        assert_eq!(handle(0), "m1");
        assert_eq!(handle(2), "m3");
    }

    #[test]
    fn parse_verdict_accepts_plain_and_fenced_json() {
        let plain = r#"{"speakers":[{"member":"m2","action":"reply_text"}],"reason":null}"#;
        let v = parse_verdict(plain).expect("plain parses");
        assert_eq!(v.speakers, vec![entry("m2", DirectorAction::ReplyText)]);
        let fenced = format!("```json\n{plain}\n```");
        assert_eq!(parse_verdict(&fenced), Some(v));
        assert!(parse_verdict("m2 先说").is_none());
    }

    #[test]
    fn normalize_drops_unseated_and_duplicates() {
        let v = DirectorVerdict {
            speakers: vec![
                entry("m3", DirectorAction::ReplyText), // only two seats
                entry("m2", DirectorAction::ReplyText),
                entry("x", DirectorAction::ReplyText),
                entry("m2", DirectorAction::ReplyImage), // repeat
                entry("m1", DirectorAction::ReplyText),
            ],
            reason: None,
        };
        let cues = normalize(&v, 2, true);
        assert_eq!(cues.iter().map(|c| c.seat).collect::<Vec<_>>(), vec![1, 0]);
        assert_eq!(
            cues[0].action,
            ActionType::ReplyText,
            "first occurrence wins"
        );
    }

    #[test]
    fn normalize_degrades_images_without_availability() {
        let v = DirectorVerdict {
            speakers: vec![
                entry("m1", DirectorAction::ReplyImage),
                entry("m2", DirectorAction::ReplyTextImage),
            ],
            reason: None,
        };
        let off = normalize(&v, 2, false);
        assert!(off.iter().all(|c| c.action == ActionType::ReplyText));
        let on = normalize(&v, 2, true);
        assert_eq!(on[0].action, ActionType::ReplyImage);
        assert_eq!(on[1].action, ActionType::ReplyTextImage);
    }

    #[test]
    fn normalize_of_an_empty_list_is_empty() {
        let v = DirectorVerdict {
            speakers: vec![],
            reason: None,
        };
        assert!(normalize(&v, 3, true).is_empty());
    }

    #[test]
    fn fallback_cues_are_every_seat_text_in_order() {
        let cues = fallback_cues(3);
        assert_eq!(
            cues.iter().map(|c| c.seat).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(cues
            .iter()
            .all(|c| c.action == ActionType::ReplyText && c.inner_state.is_none()));
    }

    #[test]
    fn render_line_maps_roles_and_prefixes() {
        assert_eq!(
            render_line(LineSpeaker::User, "早"),
            ("user", "用户：早".to_string())
        );
        assert_eq!(
            render_line(LineSpeaker::Me, "早呀"),
            ("assistant", "早呀".to_string())
        );
        assert_eq!(
            render_line(LineSpeaker::Other("小美"), "早上好"),
            ("user", "小美：早上好".to_string())
        );
    }

    #[test]
    fn normalize_title_trims_unicode_and_bounds_length() {
        assert_eq!(
            normalize_title("  周末爬山　"),
            Some("周末爬山".to_string())
        );
        assert_eq!(normalize_title(&"字".repeat(64)), Some("字".repeat(64)));
        assert_eq!(normalize_title(&"字".repeat(65)), None);
    }

    #[test]
    fn normalize_title_rejects_blank_unicode_and_control_chars() {
        assert_eq!(normalize_title("　　"), None);
        assert_eq!(normalize_title(""), None);
        assert_eq!(normalize_title("周末\u{0007}爬山"), None);
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! The scan transcript: the chat rows of one stretch, one stamped line each.

use chrono_tz::Tz;
use eros_engine_store::episode::ScanRow;

use super::strip_audio_tags;
use crate::prompt::{
    action_from_metadata, action_outcome, action_response_from_metadata, user_text_with_action,
};

/// One line per row, `[#i YYYY-MM-DD HH:MM] 用户：…` / `AI：…`, stamped in `tz`.
/// `i` is the row's 1-based position in `rows`, so `#n` always names
/// `rows[n - 1]`; a row with nothing left to say produces no line but keeps
/// its number.
///
/// A user row that carries an action reads as the chat prompt shows it (marker
/// before the text) and, once the action has a decided response, closes with
/// the outcome sentence. Voice assistant rows lose their TTS audio tags.
pub(crate) fn render_transcript(rows: &[ScanRow], tz: Tz) -> Vec<String> {
    rows.iter()
        .enumerate()
        .filter_map(|(i, row)| {
            let (label, text) = if row.role == "user" {
                ("用户", user_text(row))
            } else if row.channel.as_deref() == Some("voice") {
                ("AI", strip_audio_tags(&row.content))
            } else {
                ("AI", row.content.clone())
            };
            if text.trim().is_empty() {
                return None;
            }
            let stamp = row.sent_at.with_timezone(&tz).format("%Y-%m-%d %H:%M");
            Some(format!("[#{} {stamp}] {label}：{text}", i + 1))
        })
        .collect()
}

fn user_text(row: &ScanRow) -> String {
    let meta = Some(&row.metadata);
    let Some(action) = action_from_metadata(meta) else {
        return row.content.clone();
    };
    let text = user_text_with_action(Some(&action), &row.content);
    match action_response_from_metadata(meta) {
        Some(response) => format!("{text} {}", action_outcome(&action, response)),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use uuid::Uuid;

    fn row(role: &str, content: &str, channel: Option<&str>, sent_at: &str) -> ScanRow {
        ScanRow {
            message_id: Uuid::new_v4(),
            session_id: Uuid::nil(),
            role: role.to_string(),
            content: content.to_string(),
            channel: channel.map(str::to_string),
            sent_at: sent_at.parse::<DateTime<Utc>>().unwrap(),
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn transcript_stamps_in_tz_and_keeps_indices_when_a_row_drops() {
        let rows = vec![
            row("user", "你好", None, "2026-10-01T16:30:00Z"),
            row(
                "assistant",
                "[laughs]",
                Some("voice"),
                "2026-10-01T16:31:00Z",
            ),
            row(
                "assistant",
                "嗨 [sighs] 你来啦",
                Some("voice"),
                "2026-10-02T03:05:00Z",
            ),
            row(
                "assistant",
                "[laughs] 文字里的方括号不动",
                None,
                "2026-10-02T03:06:00Z",
            ),
        ];
        let lines = render_transcript(&rows, chrono_tz::Asia::Taipei);
        assert_eq!(
            lines,
            vec![
                "[#1 2026-10-02 00:30] 用户：你好",
                "[#3 2026-10-02 11:05] AI：嗨 你来啦",
                "[#4 2026-10-02 11:06] AI：[laughs] 文字里的方括号不动",
            ]
        );
    }

    #[test]
    fn transcript_appends_action_outcome() {
        let mut refused = row("user", "来嘛", None, "2026-10-01T10:00:00Z");
        refused.metadata = serde_json::json!({"action": {"type": "kiss", "response": "refuse"}});
        let mut undecided = row("user", "抱抱", None, "2026-10-01T10:01:00Z");
        undecided.metadata = serde_json::json!({"action": {"type": "hug"}});
        // An action with no text still reads as a line.
        let mut bare = row("user", "", None, "2026-10-01T10:02:00Z");
        bare.metadata = serde_json::json!({"action": {"type": "hug", "response": "accept"}});

        let lines = render_transcript(&[refused, undecided, bare], chrono_tz::UTC);
        assert_eq!(
            lines,
            vec![
                "[#1 2026-10-01 10:00] 用户：（凑过来想亲你）来嘛 对方凑过来想亲你，你没有接受。",
                "[#2 2026-10-01 10:01] 用户：（想抱你）抱抱",
                "[#3 2026-10-01 10:02] 用户：（想抱你） 对方想抱你，你接受了。",
            ]
        );
    }
}

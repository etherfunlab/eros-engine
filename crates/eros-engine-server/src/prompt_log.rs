//! Optional raw-prompt disk log for the main chat reply. Gated by
//! `ServerConfig.prompt_log_dir` (env `PROMPT_LOG_DIR`). Fire-and-forget:
//! the capture happens before the network send and never blocks or fails
//! the reply path. Files contain raw chat content — operator-only.

use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use eros_engine_llm::openrouter::ChatRequest;
use uuid::Uuid;

/// Owned snapshot of everything a single reply prompt log needs, decoupled
/// from the borrowed `ChatRequest` so the writer task can own it.
struct PromptLogSnapshot {
    ts: DateTime<Utc>,
    session_id: Uuid,
    user_message_id: Uuid,
    task: &'static str,
    model: String,
    fallback_model: Vec<String>,
    temperature: f32,
    top_p: Option<f32>,
    max_tokens: u32,
    /// (role, content) pairs cloned from `req.messages`, in order.
    messages: Vec<(String, String)>,
}

/// Build a snapshot from a borrowed request. `ts` is injected so the pure
/// core stays deterministic for tests; `spawn_write` passes `Utc::now()`.
fn snapshot(
    req: &ChatRequest,
    session_id: Uuid,
    user_message_id: Uuid,
    ts: DateTime<Utc>,
) -> PromptLogSnapshot {
    PromptLogSnapshot {
        ts,
        session_id,
        user_message_id,
        task: "reply",
        model: req.model.clone(),
        fallback_model: req.fallback_model.clone(),
        temperature: req.temperature,
        top_p: req.top_p,
        max_tokens: req.max_tokens,
        messages: req
            .messages
            .iter()
            .map(|m| (m.role.clone(), m.content.clone()))
            .collect(),
    }
}

/// Human-readable rendering: metadata header + one verbatim block per message.
fn render(snap: &PromptLogSnapshot) -> String {
    let mut out = String::new();
    out.push_str("# eros-engine prompt log\n");
    out.push_str(&format!(
        "# ts:       {}\n",
        snap.ts.to_rfc3339_opts(SecondsFormat::Millis, true)
    ));
    out.push_str(&format!("# session:  {}\n", snap.session_id));
    out.push_str(&format!("# user_msg: {}\n", snap.user_message_id));
    out.push_str(&format!("# task:     {}\n", snap.task));
    let fallbacks = if snap.fallback_model.is_empty() {
        String::new()
    } else {
        format!("   fallbacks: {}", snap.fallback_model.join(", "))
    };
    out.push_str(&format!("# model:    {}{}\n", snap.model, fallbacks));
    let top_p = snap
        .top_p
        .map(|v| format!("{v:.2}"))
        .unwrap_or_else(|| "none".to_string());
    out.push_str(&format!(
        "# params:   temperature={:.2} top_p={} max_tokens={}\n",
        snap.temperature, top_p, snap.max_tokens
    ));
    out.push_str(&format!("# messages: {}\n\n", snap.messages.len()));
    for (i, (role, content)) in snap.messages.iter().enumerate() {
        out.push_str(&format!(
            "================= [{i:02}] {role} =================\n"
        ));
        out.push_str(content);
        out.push_str("\n\n");
    }
    out
}

/// `{compactUtc}__{session}__{user_message_id}.prompt.txt` — all components
/// are colon-free and path-safe.
fn file_name(snap: &PromptLogSnapshot) -> String {
    let ts = snap.ts.format("%Y%m%dT%H%M%S%3fZ");
    format!(
        "{ts}__{}__{}.prompt.txt",
        snap.session_id, snap.user_message_id
    )
}

/// Synchronous write core: ensure the directory exists, then write the file.
fn write_file(dir: &Path, snap: &PromptLogSnapshot) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(file_name(snap)), render(snap))
}

/// Fire-and-forget. Builds the snapshot on the caller thread (the only place
/// `req` is borrowed), then writes on a blocking pool thread. Never blocks the
/// reply path; an IO error is logged once and swallowed.
pub(crate) fn spawn_write(
    dir: std::path::PathBuf,
    req: &ChatRequest,
    session_id: Uuid,
    user_message_id: Uuid,
) {
    let snap = snapshot(req, session_id, user_message_id, Utc::now());
    tokio::task::spawn_blocking(move || {
        if let Err(e) = write_file(&dir, &snap) {
            tracing::warn!(error = %e, dir = %dir.display(), "prompt_log: write failed");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PromptLogSnapshot {
        PromptLogSnapshot {
            ts: DateTime::parse_from_rfc3339("2026-06-27T12:34:56.789Z")
                .unwrap()
                .with_timezone(&Utc),
            session_id: Uuid::nil(),
            user_message_id: Uuid::from_u128(1),
            task: "reply",
            model: "vendor/model-a".into(),
            fallback_model: vec!["vendor/model-b".into()],
            temperature: 0.9,
            top_p: Some(0.95),
            max_tokens: 1024,
            messages: vec![
                ("system".into(), "You are Aria.\nBe warm.".into()),
                ("user".into(), "hi".into()),
            ],
        }
    }

    #[test]
    fn render_includes_header_and_verbatim_messages() {
        let out = render(&fixture());
        assert!(out.contains("# task:     reply"));
        assert!(out.contains("# model:    vendor/model-a   fallbacks: vendor/model-b"));
        assert!(out.contains("# messages: 2"));
        assert!(out.contains("================= [00] system ================="));
        assert!(out.contains("You are Aria.\nBe warm.")); // verbatim newline preserved
        assert!(out.contains("================= [01] user ================="));
    }

    #[test]
    fn file_name_is_path_safe_and_suffixed() {
        let name = file_name(&fixture());
        assert!(name.ends_with(".prompt.txt"));
        assert!(!name.contains(':')); // colon-free timestamp
        assert!(name.starts_with("20260627T123456789Z__"));
        assert!(name.contains(&Uuid::from_u128(1).to_string()));
    }

    #[test]
    fn write_file_creates_readable_file() {
        let snap = fixture();
        let dir = std::env::temp_dir().join(format!("eros-promptlog-test-{}", Uuid::new_v4()));
        write_file(&dir, &snap).expect("write");
        let path = dir.join(file_name(&snap));
        let contents = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(contents, render(&snap));
        std::fs::remove_dir_all(&dir).ok();
    }
}

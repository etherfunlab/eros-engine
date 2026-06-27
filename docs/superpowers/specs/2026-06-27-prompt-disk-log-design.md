# eros-engine — Raw prompt disk log for the main reply (Spec)

**Status**: design, pending implementation plan
**Target release**: `0.6.5` dev track. **No migration**, no HTTP-contract change.
One new optional env key (`PROMPT_LOG_DIR`).
**Audience**: anyone working on the streaming chat reply path
(`build_reply_request` / `drive_chat_burst`) or operating an eros-engine
deployment who wants to inspect the fully-assembled prompt on disk.

---

## 0. Background

### What already exists

The main chat reply prompt is assembled by `build_reply_request()`
(`crates/eros-engine-server/src/pipeline/handlers.rs:558`), which calls
`build_prompt()` (`crates/eros-engine-server/src/prompt.rs:332`) to produce the
system prompt string and then assembles a `ChatRequest`:

```rust
// crates/eros-engine-llm/src/openrouter.rs:29-61
pub struct ChatRequest {
    pub model: String,
    pub fallback_model: Vec<String>,
    pub messages: Vec<ChatMessage>,   // [system, user, assistant, ...]
    pub temperature: f32,
    pub top_p: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub max_tokens: u32,
    // user / session_id / metadata / reasoning / response_format ...
}
// ChatMessage { role: String, content: String }  (openrouter.rs:23-27)
```

That `ChatRequest` is returned to the streaming driver and bound as `req` in the
`Ok(r)` arm at `crates/eros-engine-server/src/pipeline/stream.rs:2466`, *before*
the per-model fallback send loop inside `drive_chat_burst` (the actual POST is
`state.openrouter.execute_stream(per_model_req)` at `stream.rs:307` / `:518`).

Configuration is read by `ServerConfig::from_env()`
(`crates/eros-engine-server/src/state.rs:156`) using plain `std::env::var()` with
no unified prefix (`BIND_ADDR`, `DREAMING_*`, `OPENROUTER_*`, …). Logging is
`tracing` → **stdout only**, plain text, `RUST_LOG`-controlled
(`tracing_subscriber::fmt::init()` at `main.rs:37`). There is **no file-based
logging** today, and the Docker image (`docker/Dockerfile`) is **fully stateless
with no volumes**.

### The problem

When debugging persona / prompt-assembly behaviour, there is no way to see the
*exact* prompt the engine assembled for a given turn. `RUST_LOG` traces usage
metadata (`log_openrouter_usage`, `pipeline/mod.rs:25`) but never the prompt
body, and the prompt is too large and newline-heavy to dump to stdout. The
operator needs the fully-assembled prompt persisted somewhere they can open and
read, one turn at a time.

### Goal & non-goals

**Goal**: optionally persist the fully-assembled **main-reply** prompt to a
human-readable file on disk, one file per reply turn, controlled by a single env
var, designed so the operator can point that env var at a Docker/fly volume.

**Non-goals (v1, explicit YAGNI)** — see §6:
rotation/retention/size caps, redaction, capturing auxiliary OpenRouter calls
(affinity / dreaming / snapshot), `generation_id` correlation lines, and
JSON/JSONL output.

---

## 1. Decision summary

| Decision | Choice | Why |
|---|---|---|
| Primary purpose | Debug prompt assembly | Eyeball "did my assembly produce the right prompt"; precise OpenRouter correlation not required |
| Capture scope | Main reply generation only | Smallest change, single hook point |
| On-disk shape | One human-readable file per request | Prompt is newline-heavy; per-file transcript reads best |
| Env control | Single `PROMPT_LOG_DIR` | One var is both the on/off switch and the destination; maps cleanly onto a volume mount |
| Where it lives | `eros-engine-server` (the binary) only | No change to the published `core`/`llm`/`store` lib crates → zero crates.io surface change |
| Hot-path cost | Fire-and-forget; never blocks the send | The user reply must never wait on or fail because of disk IO |

**Rejected alternative**: inject a sink at the `WireRequest` boundary in
`eros-engine-llm` (`openrouter.rs`). It would capture the exact wire bytes and
uniformly cover *all* call types, but it forces env-reading and file IO into a
published library crate, muddying its boundary. Since scope is "main reply only,"
a single server-side hook is cleaner.

---

## 2. Architecture & components

Everything lives in `eros-engine-server`. No new dependency is required
(`tokio`, `tracing`, `uuid`, `chrono` are already in the crate).

### 2.1 Config: `ServerConfig.prompt_log_dir`

Add to `ServerConfig` (`state.rs`):

```rust
pub prompt_log_dir: Option<std::path::PathBuf>,
```

Populated in `from_env()`:

```rust
prompt_log_dir: std::env::var("PROMPT_LOG_DIR")
    .ok()
    .filter(|s| !s.is_empty())
    .map(std::path::PathBuf::from),
```

`None` (unset or empty) ⇒ logging disabled.

### 2.2 New module: `crates/eros-engine-server/src/prompt_log.rs`

A small, focused module with three units:

- **`PromptLogSnapshot`** — an owned snapshot built from borrowed call-site data
  (so the spawned task owns everything it needs):
  ```rust
  struct PromptLogSnapshot {
      ts: chrono::DateTime<chrono::Utc>,
      session_id: Uuid,
      user_message_id: Uuid,
      task: &'static str,        // "reply"
      model: String,
      fallback_model: Vec<String>,
      temperature: f32,
      top_p: Option<f32>,
      max_tokens: u32,
      messages: Vec<(String, String)>,  // (role, content), cloned from req.messages
  }
  ```

- **`render(&PromptLogSnapshot) -> String`** — pure, deterministic, unit-tested.
  Produces the header + per-message blocks (see §4).

- **`file_name(&PromptLogSnapshot) -> String`** — pure; builds a path-safe name
  (see §4), unit-tested.

- **`write_file(dir: &Path, snap: &PromptLogSnapshot) -> std::io::Result<()>`** —
  synchronous IO core: `create_dir_all(dir)` defensively, then write
  `dir/<file_name>`. Unit-tested against a tempdir. Kept synchronous so tests are
  deterministic (no async sleeps).

- **`spawn_write(dir: PathBuf, req: &ChatRequest, session_id, user_message_id)`** —
  the only async/`tokio` surface: builds the `PromptLogSnapshot` from the borrows
  (the single `messages` clone happens here), then
  `tokio::task::spawn_blocking(move || write_file(&dir, &snap))` and **does not
  await it**. On `Err`, logs a single `tracing::warn!`. Never panics, never
  propagates an error to the caller.

  > `spawn_blocking` (not `tokio::fs`) keeps `write_file` a plain sync fn that the
  > unit tests call directly, and moves the blocking write off the async worker.

### 2.3 Hook point: `stream.rs:2466`

In the `Ok(r)` arm where `req` is bound, after `build_reply_request` succeeds and
before `drive_chat_burst`:

```rust
let (req, injected_tags) = match req_res {
    Ok(r) => r,
    Err(e) => { /* unchanged */ }
};
if let Some(dir) = state.config.prompt_log_dir.as_ref() {
    crate::prompt_log::spawn_write(
        dir.clone(), &req, user_msg.session_id, user_msg.user_message_id,
    );
}
```

`req` is still owned by the surrounding scope and continues into
`drive_chat_burst` unchanged; `spawn_write` only borrows it to build the snapshot.

### 2.4 Startup visibility

In `main.rs` (after the subscriber is installed and `ServerConfig` is built), if
`prompt_log_dir` is `Some(dir)`:

- `std::fs::create_dir_all(&dir)` once; on error, `tracing::warn!` but **do not
  fail boot**.
- Emit one `tracing::info!` line, e.g.:
  `prompt logging ENABLED → {dir} (writes raw assembled chat prompts to disk; operator-only)`.

This makes the privacy-sensitive mode visible in logs and surfaces a bad path
early rather than per-request.

---

## 3. Data flow & the non-blocking guarantee

```
build_reply_request ─► Ok(req) ─┬─► [flag off] one Option check, continue            ─► drive_chat_burst ─► execute_stream (POST)
                                └─► [flag on]  clone snapshot + spawn_blocking(write) ─► drive_chat_burst ─► execute_stream (POST)
                                                     (background, not awaited)
```

Hard requirements:

1. **Flag off (default)**: a single `Option` check. No clone, no spawn, no IO.
2. **Flag on**: one `messages` clone (a few KB) + a spawned blocking task. The
   send path proceeds immediately and **never awaits** the write.
3. The write happens **before** the network send, so the prompt lands on disk even
   when the OpenRouter call later fails.
4. A slow or full disk can only make a file **lag or be dropped** (logged via
   `warn`). It can **never** delay, block, or fail the user's reply.

---

## 4. File format

One file per reply turn.

**File name**: `{ts}__{session}__{user_message_id}.prompt.txt`

- `ts`: compact UTC, no colons (filesystem-safe), e.g. `20260627T123456789Z`.
- `session` / `user_message_id`: the `Uuid`s, rendered hyphenated (already
  path-safe). `user_message_id` is unique per user turn, so files do not collide
  in normal operation and the on-disk prompt ties back to the DB row for free.
  (Regeneration of the same `user_message_id` is the only collision risk; v1
  accepts last-writer-wins. If that proves annoying, append a short random suffix
  later — out of scope for now.)

**Contents**:

```
# eros-engine prompt log
# ts:       2026-06-27T12:34:56.789Z
# session:  6b1c…-uuid
# user_msg: 9a2f…-uuid
# task:     reply
# model:    moonshotai/kimi-k2   fallbacks: anthropic/claude-…, …
# params:   temperature=0.90 top_p=0.95 max_tokens=1024
# messages: 7

================= [00] system =================
<full system prompt, verbatim, original newlines>

================= [01] user =================
<content>

================= [02] assistant =================
<content>
...
```

The header is metadata for triage; each message is rendered verbatim with its
original newlines so the system prompt is read exactly as assembled.

---

## 5. Error handling & edge cases

- **Write failure** (permissions, disk full, bad path): `tracing::warn!` once in
  the spawned task; the turn is unaffected. No retry in v1.
- **`create_dir_all` at startup fails**: `warn`, continue booting; the writer also
  calls `create_dir_all` defensively, so a later-available mount still works.
- **Empty `PROMPT_LOG_DIR`**: treated as unset (filtered in `from_env`).
- **Concurrency**: per-request unique file names ⇒ no append contention, no lock.

---

## 6. Out of scope (v1, YAGNI)

Documented so operators know what they own:

- **Rotation / retention / size caps** — the operator manages the volume. (A
  future `PROMPT_LOG_MAX_FILES` could prune oldest.)
- **Redaction** — files contain raw chat content by design; mitigated by docs +
  the startup warning, not by scrubbing.
- **Auxiliary OpenRouter calls** (affinity / dreaming / snapshot) — main reply
  only. (A future shared sink could generalize this.)
- **`generation_id` correlation line** — purpose is assembly debugging;
  `user_message_id` already links to the DB.
- **JSON / JSONL output** — readability beats machine-parsing for this use case.

---

## 7. Docker / fly volume usage (docs only)

No `fly.toml` is committed (the OSS engine is not responsible for deployment); the
docs ship example snippets with placeholder paths only.

- **Docker**: mount a volume and point the env at it:
  ```
  docker run … \
    -v "$(pwd)/prompt-logs:/data/prompt-logs" \
    -e PROMPT_LOG_DIR=/data/prompt-logs \
    eros-engine serve
  ```
- **fly.io**: declare a mount and set the env (illustrative):
  ```toml
  [mounts]
  source = "prompt_logs"
  destination = "/data/prompt-logs"
  ```
  ```toml
  [env]
  PROMPT_LOG_DIR = "/data/prompt-logs"
  ```
- **`.env.example`**: add a commented, **off-by-default** entry noting it writes
  raw chat content and should point at a volume the operator controls.

---

## 8. Testing

Unit tests in `prompt_log.rs` (pure / sync, no sleeps):

1. `render` — given a `PromptLogSnapshot`, the output contains the verbatim system
   content, the correct `# messages:` count, the model line (incl. fallbacks), and
   one block per message in order.
2. `file_name` — correct `.prompt.txt` suffix, colon-free timestamp, path-safe
   components.
3. `write_file` — writes into a tempdir; assert the expected file exists and its
   contents round-trip what `render` produced.
4. `ServerConfig::from_env` — `PROMPT_LOG_DIR` unset ⇒ `None`; empty ⇒ `None`;
   set ⇒ `Some(path)`.

`spawn_write` itself is a thin spawn wrapper over the tested sync core, so it
needs no async test.

---

## 9. Release / crate impact

- Lives entirely in `eros-engine-server` (the binary); **no change** to the
  published `eros-engine-core` / `-llm` / `-store` lib crates → no crates.io
  surface change, consistent with the release flow (server is never published to
  crates.io; it ships via GHCR).
- One new optional env key (`PROMPT_LOG_DIR`); document it in `.env.example` and
  the Docker/deploy docs. No migration, no OpenAPI change.

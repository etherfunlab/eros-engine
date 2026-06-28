# `reply_to_message_id`: rewind chat context to a quoted message

**Status:** design (ready for implementation)
**Area:** chat stream pipeline — request intake (`companion_stream.rs`) and reply context assembly (`pipeline/handlers.rs`, `pipeline/stream.rs`, store `chat.rs`).

A downstream product can attach a `reply_to_message_id` (a `chat_messages.id` in
the same session) to a stream request. When present and valid, the turn's
conversation context **rewinds** to that message's position: the model sees the
history up to and including the quoted message, then the new user message —
everything in between is dropped, as if the user went back and replied at that
point. Absent the field, behavior is unchanged (context anchored at the latest
message). This covers **engine code support only**.

## Current behavior (verified against source)

- `POST /comp/chat/{session_id}/message/stream` deserializes `StreamSendRequest`
  (`crates/eros-engine-server/src/routes/companion_stream.rs:109`), validates it,
  builds a sparse `metadata` bag (the `*_raw` request-provenance keys, tips, tier,
  image_url — `companion_stream.rs:430-471`), **upserts the new user message at the
  tail** (`sent_at = now()`, `companion_stream.rs:481`), then calls `run_stream`
  with a `PersistedUserMessage` (`companion_stream.rs:503-518`).
- `run_stream` (`crates/eros-engine-server/src/pipeline/stream.rs:2165`) wraps the
  `PersistedUserMessage` into `Event::UserMessage`
  (`crates/eros-engine-core/src/types.rs:39`, the variant carried in `DecisionInput.event`).
- `build_reply_request` (`crates/eros-engine-server/src/pipeline/handlers.rs:558`)
  fetches `chat_repo.history(session_id, HISTORY_WINDOW /* 20 */, 0)`
  (`handlers.rs:568`) — the last 20 messages by `sent_at`, which **already includes
  the just-inserted user message** at the tail — and `assemble_chat_request`
  (`handlers.rs:195`) turns those rows into the `system + user/assistant` array sent
  to OpenRouter. `recall_query_text` finds the current user row inside `history` by
  `id` (`handlers.rs:574-582`).
- Separately, three "look-back" context blocks anchor on the **new** message id
  (`user_message_id`): short-term memory `recent_turn_pairs_before_message(.., 3)`
  (`handlers.rs:732`), avoid-repetition `recent_assistant_contents(.., 6)`
  (`handlers.rs:645`), emotional trajectory `recent_emotional_reasons(.., 5)`
  (`handlers.rs:660`). These are embedded in the **system prompt** via `build_prompt`.

## API change

Add one optional field to `StreamSendRequest` (`companion_stream.rs:109`):

```jsonc
{
  "content": "...",
  "client_msg_id": "...",
  "reply_to_message_id": "0e2c…-uuid"   // optional; a chat_messages.id in this session
}
```

`#[serde(default)] pub reply_to_message_id: Option<Uuid>`. Clients that omit it
get byte-for-byte the current behavior.

## Semantics — three cases

The field resolves to a `HistoryAnchor` (below) that decides **only the main
20-message history**. The system-prompt look-back blocks (short-term memory,
emotional trajectory, avoid-repetition) and memory recall **always stay anchored
on the new message** — they reflect the persona's latest state, not the rewound
thread. (Deliberate: the quoted *thread* rewinds; persona-consistency signals do not.)

| Case | Trigger | Main history fed to the model |
|------|---------|-------------------------------|
| **Latest** (default) | field absent | last-20 window ending at the new message — **unchanged** |
| **Rewind** | valid id → message `M` | turns up to & including `M`, then the new message `U`; messages between `M` and `U` dropped |
| **DropHistory** | invalid id (not found / not in this session / not older than `U`) | **no prior turns** — system prompt + look-back blocks + `U` only |

Worked example — history `… A B M C D E U` with `reply_to_message_id = M`:
model receives `… A B M` then `U` (C, D, E dropped). `M` itself is included.

"Invalid" deliberately does **not** fall back to Latest: the caller asked to
anchor at `M`; if `M` is unresolvable the engine refuses to guess a thread
(silently using latest context could leak turns the caller meant to exclude), so
it drops history and flags the error (below). Still returns `200`.

## Resolution, validation & metadata

Resolve once, in the **pre-stream phase** of `send_message_stream`, after
`get_session` (`companion_stream.rs:379`) and **before** the user-message upsert
(so the result can be written into that row's `metadata`):

- New store method `ChatRepo::message_sent_at_in_session(session_id, id) ->
  Result<Option<DateTime<Utc>>, sqlx::Error>` —
  `SELECT sent_at FROM engine.chat_messages WHERE id = $1 AND session_id = $2`.
  Uses the PK; one cheap query, only when `reply_to_message_id.is_some()`.
- Resolution:
  - field absent → `HistoryAnchor::Latest`.
  - `Some(ts)` from the lookup → `HistoryAnchor::At { message_id, sent_at: ts }`.
  - `None` from the lookup → `HistoryAnchor::DropHistory`. (At resolution time `U`
    is not yet inserted, so the lookup naturally cannot return `U`; the "not older
    than `U`" guard is therefore implicit — any resolvable row predates `U`.)
- Metadata (built alongside the existing `*_raw` keys, `companion_stream.rs:430-471`):
  - `At` → `"reply_to_message_id": "<uuid>"`.
  - `DropHistory` → `"reply_to_error": "not_found"` (no `reply_to_message_id` key).
  - `Latest` → neither key (keeps the JSONB bag sparse; metadata stays `NULL` when
    otherwise empty, per `companion_stream.rs:467`).

**Storage decision:** the link lives in `metadata`, not a dedicated column. The
engine consumes it once (here) to compute the anchor and never reads it back, so
it is request provenance — the same category as `memory_scope_raw` et al. — and
keeping the valid value and the `reply_to_error` diagnostic together is cleaner
than splitting across a column + metadata. (`continues_from_message_id` earns a
column precisely because the engine *does* read it back, for assistant chaining;
this does not.) Promote to a column only if the engine later needs to read it
back or there's a concrete need to index replies at scale; until then the BFF can
project it like `history_slim` already does for `tips_amount_usd`
(`chat.rs:249`).

`reply_to_message_id` is **not** part of the idempotency key (still just
`client_msg_id`): a retry with the same `client_msg_id` replays the original
stored frames regardless of `reply_to_message_id` — correct idempotent behavior.

## Threading the anchor through the pipeline

`HistoryAnchor` is defined in `eros-engine-core` (`types.rs`) so `Event` can carry
it (core already depends on `chrono` + `serde`; `DateTime<Utc>` serializes there
— cf. `affinity.rs`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum HistoryAnchor {
    #[default]
    Latest,
    At { message_id: Uuid, sent_at: DateTime<Utc> },
    DropHistory,
}
```

Thread it along the existing request → handler path (mirrors how `memory_scope` /
`affinity_scope` already flow):

1. `Event::UserMessage` (`types.rs:39`): add `#[serde(default)] history_anchor:
   HistoryAnchor`. The `#[serde(default)]` keeps existing serialized events parsing.
2. `PersistedUserMessage` (`stream.rs:2142`): add `pub history_anchor: HistoryAnchor`.
   Populated in `send_message_stream` from the resolution above.
3. `run_stream` copies `user_msg.history_anchor` into the `Event::UserMessage` it
   builds (`stream.rs:2219`). The background re-eval `Event::UserMessage`
   constructions (`stream.rs:2624`, `:2995`, and any other site that rebuilds the
   event from `user_msg`) copy it too — these re-decide the *same* user turn, so
   they must use the same anchor.
4. `build_reply_request` (`handlers.rs:558`) reads the anchor from `input.event`
   (alongside the existing `memory_scope` / `affinity_scope` / `tier` match arms,
   `handlers.rs:584-622`) and selects the history fetch (next section).

## History fetch

`build_reply_request` replaces the unconditional `history(session_id, 20, 0)` call
(`handlers.rs:568`) with a match on the anchor:

- **Latest** → `chat_repo.history(session_id, HISTORY_WINDOW, 0)` (existing).
- **At { sent_at, .. }** → new `chat_repo.history_anchored(session_id,
  user_message_id, Some(sent_at), HISTORY_WINDOW)`.
- **DropHistory** → `chat_repo.history_anchored(session_id, user_message_id, None,
  HISTORY_WINDOW)`.

New store method `ChatRepo::history_anchored(session_id, current_msg_id, anchor:
Option<DateTime<Utc>>, limit) -> Result<Vec<ChatMessage>, sqlx::Error>`:

- `anchor = Some(ts)` (Rewind):
  ```sql
  SELECT * FROM engine.chat_messages
  WHERE session_id = $1 AND (sent_at <= $ts OR id = $current)
  ORDER BY sent_at DESC LIMIT $limit
  ```
  then `rows.reverse()` → `[…, M, U]`. The `OR id = $current` clause re-admits the
  new message `U` (whose `sent_at > ts`), so the result always ends with `U`; the
  `sent_at <= ts` clause includes `M` and everything before it within the window,
  and excludes the between-messages. Uses the existing `(session_id, sent_at DESC)`
  index — no migration. (Edge: rows sharing `M`'s exact `sent_at` are included —
  same tolerance the existing `recent_turn_pairs` cutoff has, `chat.rs:301`.)
- `anchor = None` (DropHistory):
  ```sql
  SELECT * FROM engine.chat_messages
  WHERE session_id = $1 AND id = $current
  ```
  → `[U]` (or empty if `U` somehow absent). No prior turns.

In every case the current message `U` is present in the returned vec, so
`recall_query_text` (`handlers.rs:574`) and `model_facing_user_text` keep working
unchanged. `assemble_chat_request` (`handlers.rs:195`) is untouched — it just
receives a different slice. **No migration** (no schema change; the link rides in
`metadata`).

## Quotable roles

Any real chat turn is a valid anchor: `assistant` (the primary use case — replying
to something the AI said earlier), `user`, and `gift_user` (tip turns, which are
user-authored — model-facing they already render under the `user` role,
`handlers.rs:215`). Resolution keys only on `(id, session_id)`, so no role filter
is applied. If `M` is a user/gift_user turn the model sees two consecutive user
turns (`M` then `U`) — harmless; providers tolerate it.

## Out of scope

- Rendering the quoted message in any UI ("replying to: …") — downstream/BFF concern.
- Changing which look-back blocks anchor where (they stay on the new message).
- Persisting `reply_to` as a queryable column / FK, analytics, or threading models.
- Any `model_config.toml` / prompt-text change.

## Testing

- **Unit (resolution):** field-absent → `Latest`; lookup `Some(ts)` → `At`; lookup
  `None` → `DropHistory`. Pure mapping over `(Option<Uuid>, Option<DateTime>)`.
- **Store (`sqlx::test`)** for `history_anchored`:
  - Rewind: seed `A B M C D E` + `U`; `Some(M.sent_at)` returns `[A B M U]`
    (excludes `C D E`, includes `M` and `U`, chronological).
  - DropHistory: `None` returns `[U]` only.
  - `message_sent_at_in_session`: returns `Some` for an in-session id; `None` for a
    cross-session id and a non-existent id.
- **Route (`sqlx::test`)** on `send_message_stream`:
  - valid `reply_to_message_id` → `200`; new user row's `metadata.reply_to_message_id`
    set; no `reply_to_error`.
  - invalid (cross-session / nonexistent) `reply_to_message_id` → `200`;
    `metadata.reply_to_error = "not_found"`; no `reply_to_message_id`.
  - field absent → `200`; neither key present (regression guard on the existing
    sparse-metadata contract).

## Implementation touch-point checklist

- `eros-engine-core/src/types.rs` — add `HistoryAnchor` enum; add `history_anchor`
  to `Event::UserMessage` (`#[serde(default)]`). Update the event round-trip tests
  (`types.rs:177`, `:222`) only if they assert exact field sets.
- `eros-engine-store/src/chat.rs` — add `message_sent_at_in_session` and
  `history_anchored`.
- `eros-engine-server/src/routes/companion_stream.rs` — add `reply_to_message_id`
  to `StreamSendRequest`; resolve + validate pre-stream; write metadata key; pass
  `history_anchor` into `PersistedUserMessage`. Update the `StreamSendRequest` test
  builders (`req_with_tier`, `req_tip`, `base`, `minimal_req`) for the new field.
- `eros-engine-server/src/pipeline/stream.rs` — add `history_anchor` to
  `PersistedUserMessage`; copy it into every `Event::UserMessage` built from
  `user_msg`.
- `eros-engine-server/src/pipeline/handlers.rs` — branch the history fetch in
  `build_reply_request` on `input.event`'s `history_anchor`.
- Regenerate the OpenAPI spec (new request field) per the repo's pre-PR checklist.

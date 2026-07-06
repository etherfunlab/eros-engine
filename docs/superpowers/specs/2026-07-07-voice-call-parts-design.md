# Voice-call parts (engine side) — design

- Date: 2026-07-07
- Status: design agreed, implementation plan pending
- Repo: `eros-engine`

## Background & goals

Add engine support for a **voice-call-style** companion interaction (prior art: Grok Ani — real-time spoken conversation, switchable with a text mode). A voice turn is the loop **STT → LLM → TTS**.

Following the delegation philosophy established in v0.7.1 (the `image_request` frame: the engine only composes the prompt and hands drawing to the consumer), **the engine owns only the LLM segment and ships only the parts the consumer needs. STT and TTS belong entirely to the consumer; the engine never touches audio.** This keeps the engine lean, avoids needless I/O and round-trips, and keeps heavy frames off the chat SSE shape (which would perturb response shape and latency).

`eros-engine` is OSS and ships parts, not a hosted product. This spec defines only the **engine contract**: one lean endpoint, one `model_config` task, light persistence, and a memory-exclusion tweak. Everything on the audio/orchestration side is the consumer's responsibility and is out of scope here.

## Non-goals

- No STT, no TTS, no audio provider selection — all consumer-side.
- No PDE judge, no vector recall, no per-turn post-processing (affinity scoring / insight extraction).
- No replay / full idempotent-replay machinery.
- No changes to the `/comp/chat/{session_id}/message/stream` hot path (only its parts are reused).

## Responsibility split

| Concern | Owner |
| --- | --- |
| Mic capture, VAD, STT, TTS, call UI, turn-taking / barge-in | Consumer (out of scope) |
| LLM text reply (text in → text out, streaming) | Engine |
| Persona prompt, thin relationship state, single-model selection, persistence | Engine |

## Architecture: per-turn stateless

OpenRouter chat completions are **stateless** — there is no provider-side session holding context; every turn must resend the (trimmed) message array. So context is held by the engine **reusing the existing chat session**: each turn does one cheap `history()` read, with no vector recall.

The engine's voice endpoint is therefore **per-turn stateless**: a whole call is a sequence of independent SSE requests. The consumer orchestrates the call; the engine per turn just does "receive one user utterance → read the last N turns of history → generate → stream text back → persist."

## Single-turn data flow

```
consumer:  mic → VAD → STT → text
consumer → engine:  POST /comp/voice/{session_id}/turn/stream  { content, client_msg_id }
engine:
   1. JWT auth + session-ownership check (same as message/stream)
   2. acquire a per-user stream slot (reuse existing guard, cap 3)
   3. persist user turn (role=user, channel="voice")
   4. read last N turns of history (text turns + voice turns, interleaved)
   5. load 1 affinity row (cheap single-row read)
   6. assemble thin system_prompt (persona + voice directive + one relationship line)
   7. single-model OpenRouter streaming call ([tasks.chat_voice])
   8. SSE: delta* → final
   9. persist assistant turn (role=assistant, assistant_action_type="reply", channel="voice")
engine → consumer:  SSE  delta* / final ( / error )
consumer:  text deltas → sentence-wise TTS → playback
```

On this path the engine does **not**: PDE judge, vector recall, insight/memory extraction, affinity scoring, image/traits/scopes/tips branches, replay.

## Engine endpoint contract

**Route** (name TBD): `POST /comp/voice/{session_id}/turn/stream`

A dedicated thin handler that only shares the parts — OpenRouter client / `model_config` / `PersonaRepo` / `ChatRepo` / `AffinityRepo` — and never enters `run_stream`.

**Request body** (minimal):

```json
{ "content": "the user's utterance text", "client_msg_id": "<26..36 ASCII printable>" }
```

- `content`: non-empty, `chars() <= MAX_CONTENT_CHARS` (reuse the existing validation style and `StreamPreError` shape).
- `client_msg_id`: reuse the existing format constraints. Used for lightest-touch idempotency — `(session_id, client_msg_id)` is unique; on collision, do not double-insert. **No** replay stream.

**Reused**: JWT + session-ownership gate, per-user stream slot guard (`CONCURRENT_STREAMS_PER_USER = 3`), the `StreamPreError` pre-stream error shape, `upsert_user_message_idempotent` for the user turn.

**SSE frames (thin)**: only `delta` / `final` / `error` (reuse the corresponding `ProtocolFrame` variants); **no** `meta` / `image*` / `pending` heavy frames. Streaming is required so the consumer can TTS sentence-by-sentence for a real-time feel. Keep-alive reuses the existing 15s ping.

**Errors**:
- `[tasks.chat_voice]` not configured ⇒ `501` (same pattern as the image endpoint: the feature is opt-in).
- Other 4xx (422/400/401/403/404/409/429) match `message/stream` semantics and error shape.

## Thin prompt assembly

```
system_prompt =
    genome.system_prompt                    # persona, from DB
  + <voice directive>                        # [tasks.chat_voice].filter_prompt (built-in default, overridable)
  + <one relationship line>                  # relationship_label + a tone hint, from the one affinity row
```

- The **voice directive** lives in the new task's `filter_prompt`, with a product-identity-free **built-in default** (same pattern as `chat_image_prompt_compose`: the deployment may override). Default intent: *you are on a voice call; keep replies short and spoken; no markdown / emoji / bracketed stage directions* (TTS reads everything literally, so symbols and asides must be suppressed).
- **Relationship state**: inject `relationship_label` and a short tone hint so the call's tone tracks the text relationship's progress. **No** vector recall, no memory pull, no traits/scopes/emotional_context/avoid_repetition heavy blocks.

**history → wire messages**: read the session's last N turns (a dedicated constant `VOICE_HISTORY_WINDOW`, shorter than the text path to cut latency/tokens). Text and voice turns are **interleaved**, so a call remembers the text chat and vice versa. Voice-turn `role` stays `user`/`assistant`, so mapping to wire role is free.

## New model_config task

```toml
[tasks.chat_voice]          # name TBD
model = "some/fast-model"   # single fixed id: no rotation (no round-robin / weighted)
fallback = ["backup/model"] # allowed — but this is an outage retry chain, not rotation
temperature = 0.8
max_tokens = 300            # call replies are short
reasoning = { enabled = false }
# filter_prompt optional: overrides the built-in voice directive default
```

- Latency-sensitive; the deployment picks a fast model (time-to-first-token first).
- Matches the constraint of a **single model**: `model` accepts only a single fixed id, not the round-robin/weighted array/table forms. `fallback` as an outage retry chain is allowed, same semantics as other tasks.
- Task absent ⇒ endpoint 501 (opt-in feature).
- Add a commented example block to `examples/model_config.toml`.

## Persistence & the `channel` column

Voice turns land in the existing `chat_messages` (reusing the session for continuity). A new dedicated column `channel` distinguishes them — cleaner and more SQL-friendly than a JSON flag, and **symmetric** across user and assistant rows (voice I/O is still text; only the entry endpoint differs).

- `channel IS NULL` → prior behavior (text chat). Every existing INSERT leaves it NULL, so the hot path is untouched.
- `channel = 'voice'` → voice channel.

| Turn | role | assistant_action_type | channel |
| --- | --- | --- | --- |
| user voice turn | `user` (unchanged) | — | `voice` |
| assistant voice turn | `assistant` (unchanged) | `reply` (unchanged) | `voice` |

- `role` never changes, so every time-ordered history read naturally includes voice turns → two-way continuity, and existing role-filtered queries/analytics are undisturbed.
- `assistant_action_type` stays `'reply'` → **no** change to its CHECK constraint, and no asymmetric "what do we call the user side" problem. The `channel` column marks both sides identically.
- `channel` is the single exclusion/selection key for everything voice.

**Migration `0030`** adds the column:

```sql
ALTER TABLE engine.chat_messages
  ADD COLUMN channel TEXT NULL
    CHECK (channel IS NULL OR channel IN ('voice'));
```

Adding a nullable column is a metadata-only change in Postgres (no table rewrite), so it is non-blocking. The CHECK follows the codebase convention (cf. `assistant_action_type` in `0012`) and is trivially extended when new channels appear. No new index is needed for the dreaming filter (already narrowed by `session_id`); a partial index `WHERE channel IS NOT NULL` can be added later if cross-session voice analytics ever needs it.

**Store surface**: the voice endpoint's user/assistant inserts set `channel = 'voice'`; existing `ChatRepo` inserts leave it NULL. `AssistantInsert` and the user upsert gain an optional `channel` (defaulting NULL) so the text path is unchanged.

## Memory exclusion (experimental — keep it out of memory for now)

Voice is still experimental, so keep it **out** of long-term memory to avoid polluting the profile.

- **per-turn**: the voice endpoint runs no insight/memory extraction (see non-goals), so it produces no memories.
- **session-end dreaming sweeper**: `crates/eros-engine-server/src/pipeline/dreaming.rs` `classify_session` currently pulls the whole conversation with
  `SELECT role, content FROM engine.chat_messages WHERE session_id = $1 ORDER BY ...`
  and feeds it to `memory_extraction`. Add `AND channel IS DISTINCT FROM 'voice'` to that WHERE so voice turns are excluded from memory extraction. (`IS DISTINCT FROM` keeps NULL/text rows and drops only voice.)

> To let calls contribute to memory later, drop this filter — a single, reversible switch.

## Reuse / skip list

**Reuse (parts)**: OpenRouter client, `model_config` (new task), `PersonaRepo::load_companion`, `ChatRepo` (history + upsert), `AffinityRepo` (single-row load), the JWT/ownership gate, the stream slot guard, `StreamPreError`, the delta/final/error `ProtocolFrame` variants, SSE keep-alive.

**Skip (lean)**: PDE judge, `build_reply_request`'s vector recall + insight + recent_turns + emotional_context + avoid_repetition + heavy `build_prompt`, per-turn affinity eval, per-turn insight/memory extraction, image/traits/scopes/tips branches, replay / full idempotent replay.

## Naming — open items (settle before implementation)

- Endpoint path (`/comp/voice/{session_id}/turn/stream`?)
- Task name (`chat_voice`?)
- The `channel` value string (`'voice'`?) — the column name `channel` is settled.

(Naming was explicitly deferred; this spec uses the placeholders above consistently.)

## Testing

- Endpoint: empty content → 422; malformed client_msg_id → 400; session not owned → 403; session missing → 404; `[tasks.chat_voice]` absent → 501; slot cap exceeded → 429.
- Idempotency: repeating the same `(session_id, client_msg_id)` produces no duplicate row.
- Persistence: user/assistant voice turns carry `channel='voice'`; `role` (`user`/`assistant`) and `assistant_action_type` (`reply`) unchanged; text turns keep `channel IS NULL`.
- Migration: an assistant-side `channel` value outside the CHECK set is rejected; existing text INSERTs default `channel` to NULL.
- Prompt: thin prompt contains persona + voice directive + relationship line; **excludes** recall/memory blocks.
- SSE: only delta/final(/error) appear, no meta/image frames; deltas are incrementally consumable.
- Interleaving: a voice turn reads prior text turns and vice versa.
- Memory exclusion: with a mixed session, `classify_session` feeds only rows where `channel IS DISTINCT FROM 'voice'`; voice content never reaches `memory_extraction`.
- Single-model constraint: an array/table form for `chat_voice.model` is rejected at load time.

## Consumer-side contract (out of scope, for reference)

The consumer captures and gates audio (VAD), runs STT, POSTs the recognized text per turn to the voice endpoint, consumes the SSE text deltas and runs TTS sentence-by-sentence, plays the audio, and orchestrates turn-taking / barge-in and the "in a call" session state. The engine is agnostic to all of it.

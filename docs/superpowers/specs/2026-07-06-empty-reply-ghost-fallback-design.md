# Empty `reply_text` → ghost fallback — design

**Date:** 2026-07-06
**Status:** design (approved shape, pending spec review)
**Area:** `crates/eros-engine-server/src/pipeline/stream.rs` (streaming reply path), the SSE wire (`ProtocolFrame::Done`), and a one-condition change in the downstream consumer (`eros-engine-web`).

## 1. Problem

On the streaming chat path a `reply_text` turn can end up with an **empty**
final reply, from two distinct causes:

- **(a) regex strip empties it.** The per-model output-regex filter
  (`apply_output_regex`) strips an artifact-only reply down to `""`
  (`drive_chat_burst`, filtered mode, `stream.rs:847-853`). Since the fail-safe
  was removed (commit `4bee255`, 2026-06-29) this is intentional. Today the
  engine persists an empty assistant row (`content=""`, with `pre_filter_content`
  / `generation_id` / `filter_triggers` audit), emits `Meta{Reply}` + **no**
  `Delta` + `Done`, and the web silently drops the empty bubble.
- **(b) the model returns an empty completion.** A served attempt yields no
  content (a 2xx response with an empty body — not a length truncation, not a
  transport error). Today this is folded into `truncated` (`stream.rs:524-526`
  live / `724` filtered), advances the model chain, and on chain exhaustion
  produces either the canned "pseudo-ghost" phrase
  (`build_stream_failure_pseudo_ghost`) or a hard `Error` frame.

In both cases the user sees **nothing** — no reply, and no signal that the
companion chose not to answer. We want an empty `reply_text` to render as a
**ghost** ("AI 伴侣没回复"), the same affordance the real ghost path produces,
while keeping a server-side audit trail (`generation_id`, `pre_filter_content`)
so a "real ghost" and a "fell-back-to-ghost" are distinguishable after the fact.

## 2. Goal & non-goals

**Goal.** When a *successfully served* `reply_text` turn produces an empty final
reply, surface it to the consumer as a ghost (live), record why in the audit
trail, and stay affinity-neutral.

**In scope**

- (a) regex-strip-to-empty (filtered mode).
- (b) empty completion (200-ish response, empty body), in **both** live and
  filtered modes. The model chain still advances on an empty completion; only the
  **last** attempt being empty falls back to ghost.

**Out of scope (unchanged behavior)**

- Genuine transport errors / stream-open failures / length truncation → keep the
  existing `truncated` → chain-advance → `build_stream_failure_pseudo_ghost` /
  `Error` behavior.
- The real ghost path (PDE `Ghost` decision, `stream.rs:2537-2561`) — untouched.
- The canned pseudo-ghost phrase fallback — untouched (a distinct feature; see
  naming note §9).
- Any change to `training_level`, memory, or insight extraction.

## 3. Locked design decisions

1. **Signal via a `Done` flag, not `Meta{Ghost}`.** `Meta{Reply}` is emitted
   before the empty state is known (live: `stream.rs:487`, upfront; filtered:
   `792`), so it cannot cleanly become `Meta{Ghost}`. `Done` is emitted in both
   modes *after* the completion is known (live: `589`, filtered: `948`), so a
   flag on `Done` has no ordering hazard and works uniformly in both modes.
2. **Keep the audit assistant row.** The empty row is the natural home for the
   audit (`content=""`, `pre_filter_content`, `generation_id`, `filter_triggers`
   already persisted for case (a)). Add `metadata.fallback_reason`.
3. **Wire flag is a generic `bool`; the reason lives in the DB.** `Done` carries
   `ghost_fallback: bool`; `chat_messages.metadata.fallback_reason` distinguishes
   `"regex_strip"` vs `"empty_completion"`.
4. **Affinity-neutral.** A fallback-ghost turn must NOT run `record_ghost`, must
   NOT bump/reset `ghost_streak`, and must NOT emit a per-turn affinity delta —
   it is a technical empty reply, not the companion choosing to go silent, and
   must not pollute ghost gating or affinity analytics.
5. **The ghost hint is live-only.** We do NOT stamp the user row's
   `ghost_decision` and do NOT emit `Meta{Ghost}`. On reload/replay the empty
   audit row is dropped by the web's existing `isEmptyAssistantTurn` → nothing is
   shown, byte-identical to how a real ghost and today's empty rows reload. So no
   replay-path change is required.
6. **One condition on the web.** The consumer marks a finalized assistant message
   as ghost when its `Done` carried `ghost_fallback`, reusing the existing ghost
   rendering, instead of dropping the empty bubble.

## 4. Wire protocol change

`ProtocolFrame::Done` (`stream.rs:67-76`) gains one field:

```rust
Done {
    message_id: String,
    truncated: bool,
    usage: Option<serde_json::Value>,
    generation_id: Option<String>,
    /// True when this turn's reply_text was served but resolved empty and is
    /// being surfaced as a ghost ("AI didn't reply"). The specific cause lives
    /// in the persisted row's `metadata.fallback_reason`, not on the wire.
    #[serde(default, skip_serializing_if = "is_false")]
    ghost_fallback: bool,
},
```

`skip_serializing_if` keeps the frame byte-identical for the overwhelming
majority (non-fallback) case, so only fallback turns carry the new field.

## 5. Engine data flow

A shared requirement across all three trigger sites: emit
`Done{ ghost_fallback: true, generation_id: <last_gen_id>, truncated: false,
usage: <served usage or None> }`, persist (or keep) an empty assistant audit row
with `metadata.fallback_reason` set, do **not** advance the chain or fall through
to pseudo-ghost/Error, and mark the turn as affinity-neutral (§6).

### 5.1 Filtered mode — case (a) regex-strip-to-empty

`drive_chat_burst`, filtered served path (`stream.rs:809-953`). `visible` is
empty **iff** the regex-strip-to-empty branch (`847`) fired (the LLM output
filter never emptifies a non-empty `cleaned` — it fails open to `cleaned`). So at
the existing `Done` emit (`948`):

- Set `ghost_fallback: visible.is_empty()`.
- When empty, add `fallback_reason = "regex_strip"` to the row `metadata` built
  at `931` (the row + regex audit at `921-935` are otherwise unchanged).

No new persist and no control-flow change — this is a flag + a metadata key.

### 5.2 Filtered mode — case (b) empty completion

Split empty-completion out of `truncated`. Today `stream.rs:724` does
`if !truncated && acc.is_empty() { truncated = true }`. Introduce
`empty_completion` tracked separately (empty body, not length, not transport):

- Non-last attempt empty → advance the chain (as today).
- **Last** attempt empty → emit the shared ghost-fallback outcome (§5), persisting
  a fresh empty audit row (`content=""`, `generation_id`, `fallback_reason =
  "empty_completion"`), instead of `build_stream_failure_pseudo_ghost` / `Error`
  (`stream.rs:740-789`).

Transport errors and length truncation continue to set `truncated` and keep the
existing path.

### 5.3 Live mode — case (b) empty completion

`drive_chat_burst`, live path (`stream.rs:479-684`). Same split. Live mode emits
`Meta{Reply}` upfront (`487`), persists the row (`560`), then emits `Done` (`589`)
— all before the last-attempt decision (`607`). Compute
`is_ghost_fallback = empty_completion && idx + 1 == chain.len()` before the
persist/`Done`, then:

- Persist the row with `metadata.fallback_reason = "empty_completion"` when
  `is_ghost_fallback`.
- Emit `Done{ ghost_fallback: is_ghost_fallback }`.
- **A non-last empty completion is marked `truncated = true`** (before the
  persist/`Done`) so it flows through the *existing* superseded-attempt
  advance path unchanged — the persisted row and its `Done{truncated:true}`
  carry the "replace me" signal, exactly as before this feature, so the client
  and replay never see a spurious *successful* empty turn. (An earlier draft
  routed it through a separate `if empty_completion { continue }` branch that
  left `truncated:false` and emitted a phantom completed empty turn — a bug
  caught in codex review; marking it truncated is the fix.) Only the **last**
  empty attempt (`is_ghost_fallback`) returns via the ghost path; genuine
  length/transport truncation keeps its current pseudo-ghost/`Error` path. The
  "accept reply" gate (`if !truncated`) is therefore **unchanged**.

Case (a) does not occur in live mode (regex only runs when buffering), so live
mode only handles case (b).

> **Risk (primary).** §5.3 is the most delicate part — it restructures the
> live-mode chain-exhaustion control flow. It is the main area for careful tests
> (§8) and review.

## 6. Affinity effect — *partial* neutrality (consciously scoped)

A fallback-ghost turn is **partially** affinity-neutral. The final whole-branch
review (2026-07-06) found that strict "zero affinity effect" is not achieved by
construction, and the maintainer chose to accept the status quo rather than
expand scope. The actual, guaranteed contract:

**Neutral (guaranteed):**
- No `record_ghost` — we are not in the ghost branch. So **no `ghost` affinity
  event** is written.
- **`ghost_streak` reset skipped** — `run_stream` gates the post-burst
  `ghost_streak = 0` reset on `BurstOutcome.ghost_fallback`. A technical empty
  reply does not clear a real-ghost streak (neither increments nor resets it).
- **LLM affinity eval skipped** — the empty `produced.full_text` makes the
  evaluator's `eval_text` empty, so `eval_skip_reason` marks the turn skipped and
  no LLM-scored delta is computed.
- **Memory + insight skipped** — empty produced text is filtered by
  `should_write_user_turn` (`post_process.rs:291-293`) and the insight loop
  (`74-87`).

**NOT neutral (accepted status quo):**
- `post_process::run` still runs for the `ReplyText` arm (it is not gated on
  `ghost_fallback`). Inside it, `persist_affinity` still writes an
  `event_type="message"` affinity event and applies the **rule-based** delta
  `predict_reply_deltas` (`pde.rs`), which is derived from the *user* message's
  length / staleness — **not** from the empty reply — and `refresh_lead_score`
  runs. So affinity meters can still move by the small user-side rule delta.

**Rationale.** For case (a) (regex-strip-to-empty) this is exactly the
pre-existing behavior — **not a regression**. Making it strictly neutral would
require threading `ghost_fallback` into `post_process::run` and gating
`persist_affinity` / lead refresh there — a change to `post_process.rs` beyond
this feature's scope, for a small effect (the rule delta is user-derived and
typically tiny). The guaranteed-neutral pieces above are what keep *ghost*
analytics clean (no ghost event, no streak change, no LLM eval, no
memory/insight); the residual `message` event + rule delta are identical to any
other reply turn. A test (§10) locks this accepted contract against silent
drift. Revisiting strict neutrality is a possible follow-up, not a blocker.

## 7. Persistence, replay, reload

- **Audit row.** `chat_messages`: `content=""`, `assistant_action_type="reply"`
  (unchanged), `pre_filter_content` = raw pre-strip reply (case a) / empty
  (case b), `generation_id` = served generation, `filter_triggers` = regex audit
  (case a) / null (case b), `metadata.fallback_reason ∈ {regex_strip,
  empty_completion}`. No schema migration — `metadata` is existing JSONB.
- **Distinguisher.** Real ghost = `ghost_decision=true` on the user row + **no**
  assistant row. Fallback-ghost = an empty assistant row whose
  `metadata.fallback_reason` is set (+ no `ghost_decision`).
- **Reload (`GET …/history`).** The slim-history endpoint returns only
  `{id, role, content, sent_at, client_msg_id}` (no ghost signal). The empty
  audit row is dropped by the web's `isEmptyAssistantTurn` → nothing shown,
  identical to a real ghost's reload and to today. No history/BFF change.
- **Replay (idempotent re-request).** `upsert_user_message_idempotent`
  (`chat.rs:581-608`) returns the assistant row as a normal reply; with empty
  content the web drops it → nothing shown. The live-only hint is not
  reconstructed on replay — consistent with real-ghost reload. **No replay
  change.**

## 8. Consumer (eros-engine-web) change

Confirmed against `etherfunlab/eros-engine-web@main`. One condition:

- `erosClient.ts`: add `ghost_fallback?: boolean` to the `done` `StreamFrame`
  variant (`~137`).
- `stores/chat.ts` finalize / `onDone`: when a finalized assistant message has
  `ghost_fallback` (empty content + flag), set `ghosted = true` (the same flag
  `Meta{action_type:'ghost'}` sets at `chat.ts:393,406`) instead of letting it be
  dropped. `chatVisibility.ts`'s `isEmptyAssistantTurn` already **keeps** `m.ghosted`
  rows, so the existing ghost rendering is reused — no new component.

No change to history mapping, `chatVisibility.ts`, or the affinity store.

## 9. Naming

The codebase already has `build_stream_failure_pseudo_ghost` (a **canned-phrase**
reply persisted as a real reply row) — a different concept. This feature uses
**`ghost_fallback`** on the wire and `fallback_reason ∈ {regex_strip,
empty_completion}` in the DB. Do not reuse "pseudo_ghost".

## 10. Testing

- **Filtered case (a):** a served reply that a regex rule strips to empty →
  `Done{ghost_fallback:true}`, audit row `content=""` +
  `fallback_reason="regex_strip"` + `pre_filter_content` = raw, no `Delta`.
- **Filtered case (b):** last-attempt empty completion → `Done{ghost_fallback:true}`,
  `fallback_reason="empty_completion"`, no pseudo-ghost/Error; a non-last empty
  completion still advances to the next model.
- **Live case (b):** same as filtered case (b) on the live path; non-last empty
  advances; a real transport error / length truncation on the last attempt still
  produces pseudo-ghost/Error (regression guard).
- **Neutrality:** a fallback-ghost turn leaves `ghost_streak` unchanged (no reset)
  and writes no affinity delta / memory / insight.
- **Non-fallback regression:** a normal non-empty reply emits `Done` **without**
  the `ghost_fallback` field (wire byte-identical), and `record_ghost` /
  `ghost_streak` reset behavior for real replies and real ghosts is unchanged.
- **Wire back-compat:** `Done` serialization for a non-fallback turn is
  byte-identical to before (skip_serializing_if).

## 11. Rollout

Engine and web ship independently and are forward/backward compatible: the engine
only ever *adds* `ghost_fallback:true` on fallback turns; an un-updated web
ignores the unknown field (renders as today — empty bubble dropped). Land the
engine change first; the web condition can follow. No migration, no config.

# eros-engine — LLM-based PDE (opt-in decision layer + rule guardrails)

**Status**: design, pending implementation plan
**Target release**: `0.6.x` dev track. **One migration (`0028`).**
**Scope**: replace the role of the deterministic Persona Decision Engine with an
**opt-in LLM judge** that decides, per turn, the **action** (`reply_text` / `ghost` /
`reply_image` / `reply_text_image`) and a free-text **inner_state** that is folded
into the reply prompt. The existing rule engine in `eros-engine-core/src/pde.rs` is
**not deleted** — it is demoted to a deterministic **fallback** (when the feature is
off or the judge fails) plus a **guardrail** layer that can veto an unsafe `ghost`.
A new append-only `companion_decision_events` audit table records each judge run.

The judge is wired exactly like the already-shipped `chat_input_filter` /
`chat_vision` / extraction LLM tasks: config-driven `filter_prompt`, model chain with
timeout, fail-open. The reserved `[tasks.pde_decision]` config block (present since the
six-axis work, never consumed) becomes live.

---

## 0. Background

### 0.1 What the PDE is today

`eros-engine-core/src/pde.rs` — a pure, I/O-free `decide(&DecisionInput) -> ActionPlan`.

- **Input** `DecisionInput`: `event` (UserMessage / ProactiveTrigger / AppOpen),
  `affinity` (six axes), `persona`, `signals` (message_count, hours_since_last_message,
  ghost_streak, hours_since_last_ghost).
- **Output** `ActionPlan`: `action_type` (Reply / Ghost / Proactive), `reply_style`
  (Warm / Neutral / Cold / Tsundere / Excited), `affinity_deltas`, `energy_cost`,
  `context_hints`.
- **Rules**: ① tip → always Reply ② `ghost::decide` (score
  `(1-intrigue)*0.4 + (1-patience)*0.4 + tension*0.2`, 4 protection layers) → Ghost+Cold
  ③/④ Proactive ⑤ Reply+Neutral.

**What is actually used vs. dormant:**

| Field | Reality today |
| --- | --- |
| `action_type` | Only `Reply` / `Ghost` reachable — the sole caller (`stream.rs::run_stream`) always passes a `UserMessage` event. `Proactive` / `AppOpen` exist only in tests. |
| `reply_style` | Only `Neutral` (normal), `Cold` (ghost), `Tsundere` (tip w/o personality) are ever emitted. `Warm` / `Excited` are never produced. |
| `context_hints` | **Always `vec![]`** — the `[inner_state]` prompt section never renders. |
| `affinity_deltas` | **Consumed**: `post_process.rs` merges `plan.affinity_deltas` (heuristic nudges) with the `affinity_evaluation` LLM's semantic deltas via `merge_deltas`. |
| `energy_cost` | **No consumer** (vestigial). |

### 0.2 Ghost behaviour today (verified — unchanged by this spec)

A ghost writes **no assistant row, generates no text, makes no LLM call**. It does two
DB writes — `mark_user_message_ghosted` (`UPDATE engine.chat_messages SET ghost_decision
= true WHERE id = $1 AND role = 'user'`) and `record_ghost` (ghost_streak++, last_ghost_at,
total_ghosts, ghost affinity penalty) — and emits `Meta{action_type: Ghost, model: None}`
+ `Done{generation_id: None}` (no `Delta`) + `Final`. A ghost is deliberate silence.

This spec changes **when** a ghost is chosen (LLM, guarded), not **how** a ghost behaves.

### 0.3 The reference pattern

`chat_input_filter` (`stream.rs::run_input_filter`, ~line 989) already proves a
lightweight judge LLM on the chat path: `[system = filter_prompt, user = payload]`,
walk the model chain with `FILTER_TIMEOUT`, fail-open, parse a JSON verdict, log usage.
The judge emits **only a verdict** — it never echoes the user's NSFW content, so it does
not trip output-side safety alignment. The LLM PDE copies this skeleton.

### 0.4 The reserved config block

`[tasks.pde_decision]` exists in `examples/model_config.toml` and is locked by the
`COMPAT_FIXTURE` schema test, but no `resolve_pde()` / `filter_prompt` consumes it. This
spec makes it live.

---

## 1. Goals / Non-goals

**Goals**

1. An **opt-in** LLM judge decides action + inner_state per turn, gated only by whether
   `[tasks.pde_decision].filter_prompt` is set. With it unset, behaviour is byte-identical
   to today (rule engine).
2. Use the dormant `context_hints` → `[inner_state]` channel for the judge's free-text
   tone/mood.
3. Make `ghost` an LLM decision (the rule score is demoted to a guardrail).
4. Reserve two **image** actions (`reply_image`, `reply_text_image`) as first-class
   actions decided by the PDE — the executor (`tasks.chat_image_generation`) is future work.
5. Per-run audit of each judge call in a new `companion_decision_events` table.

**Non-goals**

- Building the image executor (`tasks.chat_image_generation`), its OpenRouter wiring,
  the `ReplyImage` wire frame, or any tier/safety gating for image replies.
- Changing ghost mechanics, the SSE protocol, the persisted `assistant_action_type`
  strings, or the `affinity_evaluation` post-process.
- Proactive / AppOpen paths (still dead).

---

## 2. Action model

### 2.1 `ActionType` (internal, `eros-engine-core/src/types.rs`)

Rename `Reply` → `ReplyText` and add two reserved image variants:

```rust
pub enum ActionType {
    ReplyText,        // was `Reply` — text only
    Ghost,            // silent (§0.2, unchanged)
    ReplyImage,       // reserved — image only;        degrades to ReplyText until executor ships
    ReplyTextImage,   // reserved — text + image;       degrades to ReplyText until executor ships
}
```

**Rename is safe — verified.** `ActionType` is never serde-serialized to the DB or wire
(no `to_value` / `json!` over it). It is a pure internal enum used at 15 call sites
(`pde.rs`, `stream.rs`, `post_process.rs`). The DB-persisted action string
(`assistant_action_type` column) and the wire enum (`FrameActionType`) are **separate**
`"reply"` / `"ghost"` literals and are **not** renamed. So the change is a mechanical
`s/ActionType::Reply/ActionType::ReplyText/` with zero migration and zero client-protocol
impact.

### 2.2 `FrameActionType` (wire, `stream.rs`) — unchanged

Stays `Reply` / `Ghost`. All text-producing internal actions (`ReplyText`, and the
degraded forms of `ReplyImage` / `ReplyTextImage`) map to the wire `Reply` frame; `Ghost`
maps to `Ghost`. The client protocol does not change in this spec. A `ReplyImage` wire
frame is added only when the image executor ships (future).

### 2.3 Rule fallback

`pde::decide` only ever produces `ReplyText` / `Ghost` (never an image variant). `ReplyStyle`
stays for the rule path. `pde::decide`'s body is unchanged apart from the enum rename.

---

## 3. Verdict schema & judge runner

### 3.1 Verdict

The judge returns JSON:

```json
{
  "action": "reply_text" | "ghost" | "reply_image" | "reply_text_image",
  "inner_state": "free text — mood / tone for this turn (used by every text-producing action)",
  "image_prompt": "optional — what photo to send (reply_image / reply_text_image only); stored, unused until the executor ships",
  "reason": "optional — short, for audit/debug; never injected into the prompt"
}
```

Parsed by `parse_pde_verdict(&str) -> Option<PdeVerdict>` (mirrors
`parse_input_filter_verdict`). `image_prompt` rides inside the verdict/payload — no
dedicated column anywhere.

```rust
struct PdeVerdict {
    action: PdeAction,            // reply_text | ghost | reply_image | reply_text_image
    inner_state: String,
    image_prompt: Option<String>,
    reason: Option<String>,
}
```

### 3.2 Runner

`run_pde_decision(state, &ResolvedPde, ctx) -> Option<PdeVerdict>` in `stream.rs`, modelled
on `run_input_filter`:

- Messages: `[system = decision_prompt, user = ctx]`.
- `ctx` payload assembled by the engine (operator controls only the system prompt, per
  the customization goal): recent N-row transcript (reuse the input-filter transcript
  builder), current affinity six axes, signals (message_count, hours_since_last_message,
  hours_since_last_ghost, ghost_streak), and the user's latest message.
- Walk `[model] + fallback_model` with `FILTER_TIMEOUT`; fail-open (timeout / error /
  empty / unparseable ⇒ `None`).
- `log_openrouter_usage("pde_decision", None, &resp)`.
- Returns `None` on any failure → caller uses the rule fallback (§6).

---

## 4. Config layer (`eros-engine-llm/src/model_config.rs`)

New resolver mirroring `resolve_vision` exactly:

```rust
pub struct ResolvedPde {
    pub model: String,
    pub fallback_model: Vec<String>,   // truncated to retry_depth
    pub temperature: f64,
    pub max_tokens: u32,
    pub decision_prompt: String,       // from [tasks.pde_decision].filter_prompt
    pub retry_depth: u32,
    pub reasoning: Option<ReasoningConfig>,
}

pub fn resolve_pde(&self) -> Option<ResolvedPde>;
```

`resolve_pde()`: reads `[tasks.pde_decision]`; returns `None` when the task is absent **or**
its `filter_prompt` is blank → feature off → rule engine. Task-level only (no tier
override), consistent with `chat_vision` / extraction. **Blank ⇒ silently off (rule
fallback), NOT a boot refusal** — unlike the extraction tasks, the PDE has a working
deterministic fallback, so a blank prompt should fall back rather than refuse to boot.

**OSS template** (`examples/model_config.toml`): keep `[tasks.pde_decision]`, flip the
"reserved — not consumed" comment to describe the opt-in LLM layer, and add a
commented-out sample `filter_prompt`. Default ships **off** (no `filter_prompt`).

**Schema lock**: add a `filter_prompt` line to `pde_decision` in `COMPAT_FIXTURE`
(additive — does not break the lock) and assert `resolve_pde()` resolves from it.

---

## 5. Guardrails (`eros-engine-core`)

Extract a pure predicate from `ghost.rs` so the rule engine and the LLM-guard path share
the same hard protections:

```rust
/// True when a ghost is permitted (none of the hard protections forbid it).
/// = ghost.rs `decide`'s protection layers WITHOUT the score test.
pub fn ghost_permitted(a: &Affinity, s: GhostSignals) -> bool;
//   false if: message_count < 10, ghost_streak >= 2, or hours_since_last_ghost < 1.0
```

Tip is **not** a parameter here — it is handled at the call site: the rule path
(`pde::decide`) forces tip → reply before ever touching ghost logic, and on the LLM path
tip turns skip the judge entirely (§6), so a tipped turn never reaches `ghost_permitted`.

**Guardrails only downgrade toward `ReplyText`; never upgrade.** Applied to the judge's
proposed action:

| Judge proposes | Condition | Acted action |
| --- | --- | --- |
| `ghost` | `ghost_permitted == false` (new relationship / streak / cooldown) | `ReplyText` |
| `ghost` | `ghost_permitted == true` | `Ghost` |
| `reply_image` / `reply_text_image` | `tasks.chat_image_generation` not configured/wired (always, today) | `ReplyText` |
| `reply_text` | — | `ReplyText` |

The judge can never force a ghost the rules would not allow, and can never produce an
image reply before the executor exists.

---

## 6. Stream wiring (`stream.rs::run_stream`)

Today: build `input` → `pde::decide(&input)` → `match plan.action_type`.

New (runs **every turn**, before prompt assembly; the judge is a blocking pre-generation
step, same placement as `input_filter`):

```text
build DecisionInput `input`
plan =
  if tip turn OR resolve_pde() == None:
      pde::decide(&input)                         // rule engine (today's behaviour)
  else:
      match run_pde_decision(p, ctx):
        Some(verdict):
            action  = guardrails(verdict.action, &input)     // §5 — may downgrade
            hints   = if verdict.inner_state non-empty { vec![verdict.inner_state] } else { vec![] }
            ActionPlan {
              action_type:    action,
              reply_style:    Neutral,                        // tone now lives in inner_state
              context_hints:  hints,                          // → [inner_state] in build_prompt
              affinity_deltas: predict_reply_deltas(&input),  // reuse rule heuristic (§9)
              energy_cost:    ENERGY_COST_REPLY,
            }
            + record audit row (§8, fire-and-forget)
        None:                                                 // fail-open
            pde::decide(&input) + record audit row (status=timeout/error/parse_error)
match plan.action_type { Ghost => …unchanged…, ReplyText/ReplyImage/ReplyTextImage => Reply path }
```

- **Tip turns skip the judge** (the rule path already handles tip → reply +
  `tip_personality` tone), saving one call — consistent with how `input_filter` / vision
  skip tipped turns.
- `context_hints` is the **existing** plumbing; `build_prompt` already renders it as
  `[inner_state]`. No new prompt parameters — the dormant channel just gets populated.
- The `match plan.action_type` adds arms for `ReplyImage` / `ReplyTextImage`; because the
  guardrail degrades them to `ReplyText` today, they route through the existing Reply path.
  (When the executor ships, these arms branch to image handling.)

---

## 7. Replay & persistence (no new replay-critical state)

`replay_stream` serves the **persisted outcome**, not the decision:

- Ghost: the `ghost_decision` boolean on the user row → `replay_stream(ghost=true)` emits
  Ghost frames.
- Reply: the stored assistant content rows → replayed verbatim (content, model, usage,
  generation_id).

The PDE verdict (inner_state, proposed action) only shapes the **prompt** at generation
time; once the assistant text exists (or the ghost flag is set), replay is wire-identical
without it. `run_stream` only runs on a fresh idempotent insert, so the judge never
re-runs on a retry.

**Therefore no verdict persistence is required for correctness.** (This supersedes the
earlier brainstorming note about writing the verdict to the user row — it is unnecessary.)
`companion_decision_events` (§8) is purely best-effort audit.

---

## 8. `companion_decision_events` audit table

Modelled on `companion_insights_events` (migration 0025) — append-only, one row per judge
run, `run_id` join key, `status` CHECK, `payload` JSONB, OpenRouter audit trio, Supabase
lockdown.

### 8.1 Migration `0028_companion_decision_events.sql`

```sql
CREATE TABLE engine.companion_decision_events (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id          UUID NOT NULL,
    user_id         UUID NOT NULL,
    session_id      UUID,
    message_id      UUID,            -- the user message that triggered the decision
    status          TEXT NOT NULL CHECK (status IN ('ok','empty','parse_error','timeout','error')),
    action          TEXT,            -- acted action: reply_text/ghost/reply_image/reply_text_image
    proposed_action TEXT,            -- judge's pre-guardrail action; NULL when status != 'ok'
    payload         JSONB,           -- full verdict {action,inner_state,image_prompt,reason}; raw text on parse_error
    model           TEXT,
    usage           JSONB,
    generation_id   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_companion_decision_events_user_time
    ON engine.companion_decision_events (user_id, created_at DESC);
CREATE INDEX idx_companion_decision_events_run
    ON engine.companion_decision_events (run_id);
-- Supabase lockdown — copy the 0025 DO-block (REVOKE anon/authenticated guarded by
-- pg_roles existence) + ENABLE ROW LEVEL SECURITY verbatim.
```

### 8.2 Semantics

- **One row per judge run** — only on turns where the judge runs (feature on, non-tip).
  Feature-off / tip turns make no LLM call and write **no row** (faithful to
  `companion_insights_events`' "one row per OpenRouter call"). Pure-rule decisions are
  deterministic and already replayable, so they need no audit.
- `status`: `ok` (parseable verdict), `empty` (blank reply), `parse_error`, `timeout`,
  `error` (transport). On any non-`ok` status the engine used the rule fallback; `action`
  records what was actually done.
- `proposed_action` vs `action`: captures guardrail downgrades (e.g. judge proposed
  `ghost`, guard forced `reply_text`). `NULL` when `status != 'ok'` (no judge proposal).
- `run_id`: a fresh UUID per PDE run (PDE is single-stage, so ≈ per-turn; kept for the
  audit-join convention and any future multi-call PDE).

### 8.3 Writer (`eros-engine-store/src/decision.rs`, new module)

Mirror `insight.rs`'s `InsightEventInsert` + `record`:

```rust
pub struct DecisionEventInsert<'a> {
    pub run_id: Uuid,
    pub user_id: Uuid,
    pub session_id: Option<Uuid>,
    pub message_id: Option<Uuid>,
    pub status: &'a str,
    pub action: Option<&'a str>,
    pub proposed_action: Option<&'a str>,
    pub payload: Option<serde_json::Value>,
    pub model: Option<&'a str>,
    pub usage: Option<serde_json::Value>,
    pub generation_id: Option<&'a str>,
}
pub struct DecisionEventRepo<'a> { pub pool: &'a sqlx::PgPool }
impl DecisionEventRepo<'_> { pub async fn record(&self, ev: DecisionEventInsert<'_>) -> Result<(), sqlx::Error> { /* parametrised INSERT */ } }
```

Register `pub mod decision;` in `eros-engine-store/src/lib.rs`.

### 8.4 Write placement — inline, fire-and-forget

Write the row **in the PDE path right after the guarded decision is computed**, via
`tokio::spawn` (best-effort, warn-on-error).

- **Why inline, not post_process**: a `Ghost` turn does **not** run post_process (the Ghost
  arm only marks + records + emits frames). Ghost is the most important decision to audit,
  so the write must sit where both `Ghost` and reply paths pass — i.e. the decision site.
- **Why fire-and-forget**: the PDE runs before generation (blocking); an awaited INSERT
  would add to pre-first-token latency. Audit is best-effort, like other warn-on-error
  persists.

---

## 9. `affinity_deltas` / `energy_cost`

- `affinity_deltas`: on the LLM path, keep sourcing them from the existing
  `predict_reply_deltas(&input)` heuristic (deterministic, cheap, orthogonal to tone). The
  judge does **not** produce affinity deltas — that overlaps `affinity_evaluation`. So
  `merge_deltas(plan.affinity_deltas, llm_deltas)` in post_process is unaffected.
- `energy_cost`: vestigial; set from the existing rule constants. Out of scope to remove.

---

## 10. Error handling

Fail-open throughout → rule fallback (`pde::decide`). The judge can never block a chat
turn: timeout / transport error / empty / unparseable all resolve to the rule decision.
Guardrails are pure functions. The audit write is best-effort and never affects the
served stream.

---

## 11. Docs / compat

- `docs/architecture.md` + `.zh`: PDE described as "rules-based" → "rules-based by
  default; opt-in LLM decision layer via `[tasks.pde_decision].filter_prompt`, with the
  rule engine as fallback + guardrails". Add `companion_decision_events` to the schema/flow
  notes.
- `docs/model-config.md` + `.zh`: document `[tasks.pde_decision].filter_prompt` and the
  four actions.
- `README.md` + `.zh`: update any "PDE = rule-based" phrasing and the table list.
- `COMPAT_FIXTURE` (`model_config.rs`): add `filter_prompt` to `pde_decision` (additive).

---

## 12. Testing

- **core**: `ghost_permitted` unit tests (msg<10 / streak / cooldown → false; clear case →
  true); existing `pde::decide` tests stay green after the `ReplyText` rename.
- **model_config**: `resolve_pde` (task absent → None; blank `filter_prompt` → None; set →
  `Some`); compat fixture asserts the new `filter_prompt`.
- **server**: `parse_pde_verdict` (all four actions, missing/extra fields, junk → None);
  guardrail application (tip → ReplyText; msg<10 vetoes ghost; ghost honoured when
  permitted; `reply_image`/`reply_text_image` → ReplyText today; fail-open → rule). One
  stream E2E: judge → ghost (and inner_state injected into the prompt on a reply turn).
- **store**: `DecisionEventRepo::record` round-trips a row (run_id, status, payload, audit
  trio), mirroring the insight-event test.

---

## 13. Out of scope / future

- **Image executor** — `tasks.chat_image_generation` task, its OpenRouter call, the
  `image_prompt` consumption, the `ReplyImage` wire frame + handler, and any tier/safety
  gating. This spec only **reserves** the two image actions, the verdict `image_prompt`
  field, and the degrade-to-`ReplyText` guardrail.
- A debug/BFF read route over `companion_decision_events` (the insight table has one; the
  PDE one can follow the same pattern later).
- "Cold reluctant reply" as a distinct action (today `ghost` is pure silence; §0.2).

---

## 14. File-by-file change list

| File | Change |
| --- | --- |
| `crates/eros-engine-core/src/types.rs` | `ActionType`: `Reply`→`ReplyText`, add `ReplyImage`, `ReplyTextImage`. |
| `crates/eros-engine-core/src/ghost.rs` | Extract `ghost_permitted(a, s, is_tip) -> bool`; `decide` reuses it. |
| `crates/eros-engine-core/src/pde.rs` | Rename `Reply`→`ReplyText` (no logic change). |
| `crates/eros-engine-llm/src/model_config.rs` | `ResolvedPde` + `resolve_pde()`; add `filter_prompt` to `COMPAT_FIXTURE` + test. |
| `crates/eros-engine-server/src/pipeline/stream.rs` | `PdeVerdict`, `parse_pde_verdict`, `run_pde_decision`; `run_stream` decision flow; `match` arms for the two image actions (degrade); fire-and-forget audit write; `ActionType::Reply`→`ReplyText`. |
| `crates/eros-engine-server/src/pipeline/post_process.rs` | `ActionType::Reply`→`ReplyText` (rename only). |
| `crates/eros-engine-store/src/decision.rs` | **New** — `DecisionEventInsert` + `DecisionEventRepo::record`. |
| `crates/eros-engine-store/src/lib.rs` | `pub mod decision;`. |
| `crates/eros-engine-store/migrations/0028_companion_decision_events.sql` | **New** — table + indexes + Supabase lockdown. |
| `examples/model_config.toml` | `[tasks.pde_decision]`: live comment + commented sample `filter_prompt`. |
| `docs/architecture.md` / `.zh`, `docs/model-config.md` / `.zh`, `README.md` / `.zh` | PDE description + schema/flow updates. |

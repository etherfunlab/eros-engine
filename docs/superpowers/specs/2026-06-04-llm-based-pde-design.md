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
3. Make `ghost` an LLM decision (the rule score is demoted to a guardrail), with a
   `[tasks.pde_decision].ghosting = false` kill-switch (default `true`) that disables ghost
   across the entire PDE path — a product-safety lever for downstream consumers (§4.1).
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
    Proactive,        // KEPT — pde.rs builds it for ProactiveTrigger/AppOpen; post_process matches it
}
```

**`Proactive` must stay.** `pde::decide` constructs `Proactive` for the
`ProactiveTrigger` / `AppOpen` events (`pde.rs:71,83`) and `post_process.rs:178` matches
on it. Dropping it would fail to compile. Those paths remain dead-but-present (out of
scope), exactly as today.

**Rename is safe — verified, with the claim stated precisely.** `ActionType` **does**
derive `Serialize` / `Deserialize`, but that representation is **used by no DB or SSE wire
path**: the persisted action string (`assistant_action_type`, a separate `String` literal
`"reply"` / `"ghost"`) and the wire enum (`FrameActionType`) are independent and are **not**
renamed. No `to_value` / `json!` ever serializes `ActionType`. So `s/ActionType::Reply/
ActionType::ReplyText/` is a mechanical internal rename with zero migration and zero
client-protocol impact. The now-unused `Serialize` / `Deserialize` derive on `ActionType`
is **removed** in this change (it is dead; dropping it eliminates any future "the serde
name changed" trap). ~11 non-test call sites across `pde.rs`, `stream.rs`,
`post_process.rs`.

### 2.2 `FrameActionType` (wire, `stream.rs`) — unchanged

Stays `Reply` / `Ghost`. All text-producing internal actions (`ReplyText`, and the
degraded forms of `ReplyImage` / `ReplyTextImage`) map to the wire `Reply` frame; `Ghost`
maps to `Ghost`. The client protocol does not change in this spec. A `ReplyImage` wire
frame is added only when the image executor ships (future).

### 2.3 Rule fallback + the core constructor

`pde::decide` only ever produces `ReplyText` / `Ghost` / `Proactive` (never an image
variant). `ReplyStyle` stays for the rule path. `pde::decide`'s body is unchanged apart
from the enum rename.

**New core API — `pde::plan_for`.** The LLM path in `stream.rs` must NOT reach into
`pde.rs` internals: `predict_reply_deltas` / `ghost_affinity_deltas` (`pde.rs:106,131`) and
the `ENERGY_COST_*` constants are all private. Add ONE public constructor in
`eros-engine-core` that builds the `ActionPlan` for any chosen action, sourcing the right
deltas / style / energy cost internally:

```rust
/// Build the ActionPlan for an LLM-chosen action. Per action:
///   ReplyText/ReplyImage/ReplyTextImage → Neutral, predict_reply_deltas, ENERGY_COST_REPLY,
///                                          context_hints = hints (→ [inner_state])
///   Ghost                               → Cold, ghost_affinity_deltas, ENERGY_COST_GHOST,
///                                          hints ignored (a ghost emits no prompt)
///   Proactive                           → unreachable!() — never built via plan_for;
///                                          Proactive comes only from pde::decide. The
///                                          arm exists solely to keep the match exhaustive.
pub fn plan_for(input: &DecisionInput, action: ActionType, hints: Vec<String>) -> ActionPlan;
```

`stream.rs` calls `pde::plan_for(&input, acted_action, hints)` for both the LLM reply plan
**and** the LLM-honoured ghost plan — it never touches the private helpers/constants. This
keeps all plan construction owned by core. Note `plan_for(Ghost, …)` discards `hints`
(a ghost has no prompt); when a ghost is later **downgraded** to a reply (hard-safety
guardrail or `ghosting` kill-switch), the caller re-supplies the sanitized `hints` it kept
in scope (§6), so the forced reply still carries the judge's mood.

**Invariant — image actions never reach post_process.** Because the guardrails (§5) always
degrade `ReplyImage` / `ReplyTextImage` to `ReplyText` while the executor is unshipped, the
`ActionPlan` that flows downstream is only ever `ReplyText` / `Ghost` / `Proactive`.
`post_process.rs`'s existing two-arm `match` on `plan.action_type` (`:118,:178,:237`) stays
valid (no image arm needed). A core helper `ActionType::is_text_reply()` (true for
`ReplyText`, and later the image variants once they execute) centralizes the check; a test
asserts no plan carries an un-degraded image action while the executor is absent (§12).

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
    inner_state: String,          // sanitized before use — see §3.3
    image_prompt: Option<String>,
    reason: Option<String>,
}
```

### 3.2 Runner — returns a status-bearing result

The runner must **not** return a bare `Option`: the audit table's `status` CHECK column
(§8) needs to distinguish `ok` / `empty` / `parse_error` / `timeout` / `error`, and a bare
`Option` collapses every failure into `None`. So it returns a run record (mirrors how
`extract_facts` returns a `CallAudit` rather than a bare value):

```rust
struct PdeDecisionRun {
    status: PdeStatus,            // Ok | Empty | ParseError | Timeout | Error
    verdict: Option<PdeVerdict>,  // Some only when status == Ok
    raw: Option<String>,          // raw model text — kept on ParseError for the audit payload
    model: Option<String>,        // winning model id (audit trio)
    usage: Option<serde_json::Value>,
    generation_id: Option<String>,
}

fn run_pde_decision(state, &ResolvedPde, ctx) -> PdeDecisionRun;
```

- Messages: `[system = decision_prompt, user = ctx]`.
- `ctx` payload assembled by the engine (operator controls only the system prompt, per the
  customization goal): the **shared** recent-history transcript (§6 — one fetch reused by
  the judge and the input filter), current affinity six axes, signals (message_count,
  hours_since_last_message, hours_since_last_ghost, ghost_streak), and the user's latest
  message.
- Walk `[model] + fallback_model` with `FILTER_TIMEOUT`. A transport failure (timeout /
  error / empty) walks to the next model; only after the chain is exhausted does the run
  carry the terminal `status`. A parseable reply ⇒ `Ok` + `verdict`. A non-empty but
  unparseable reply ⇒ `ParseError` + `raw` (no chain walk past a content-level reply,
  matching `run_input_filter`'s "definitive keep" semantics).
- `log_openrouter_usage("pde_decision", None, &resp)`.
- Any non-`Ok` status → caller uses the rule fallback (§6) **and** writes an audit row with
  that status (§8).

### 3.3 `inner_state` sanitization (prompt-injection control)

`inner_state` is free model-authored text folded straight into the system prompt's
`[inner_state]` section (`prompt.rs:436`). Because the judge sees user-influenced context,
a hostile or confused judge could emit fake section headers (`[output]`, `[iron_rules]`,
`---`) or meta-instructions that hijack the prompt. Before use, `inner_state` is sanitized:

- **Length cap** (e.g. ≤ 200 chars; tunable) — truncate beyond it.
- **Strip structural markers** — drop/escape lines that look like section headers
  (`^\s*\[.*\]`, leading `---`, `[output]`/`[iron_rules]`/`[now]`-style tokens) and control
  characters; collapse to a single bullet of plain prose.
- **Judge system prompt forbids instructions** — the shipped sample `filter_prompt` states
  `inner_state` must be a short mood description, never instructions or formatting.
- Sanitization happens once, in the PDE path, before the text reaches `context_hints`.
- Tests cover hostile `inner_state` (injected `[output]`, overlong, control chars) → the
  rendered prompt is unaffected (§12).

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

### 4.1 `ghosting` kill-switch

`[tasks.pde_decision]` gains a `ghosting: Option<bool>` field — a product-safety switch for
downstream consumers, since LLM-driven ghosting is more aggressive than the old
score-gated rule ghost (the score layer is ceded to the LLM, §5).

```rust
/// True (default) ⇒ ghost decisions are honoured. false ⇒ the whole PDE path
/// never produces a Ghost. Read INDEPENDENTLY of filter_prompt, so it also
/// governs the pure rule engine (LLM PDE off).
pub fn pde_ghosting_enabled(&self) -> bool {
    self.tasks.get("pde_decision").and_then(|t| t.ghosting).unwrap_or(true)
}
```

- **Default `true`** (absent / `[tasks.pde_decision]` missing) — today's behaviour, no change.
- **`ghosting = false`** — a hard kill-switch across the **entire** PDE path (ratified
  scope): an LLM `ghost` verdict, a rule-fallback `ghost`, **and** the pure rule engine's
  `ghost` are all degraded to `ReplyText`. A consumer that sets it is guaranteed the
  companion never goes silent. Read independently of `filter_prompt` so it works whether or
  not the LLM PDE is enabled.
- Task-level only (no per-tier override) — matches `resolve_pde`. (Per-tier ghosting is a
  possible future extension if a consumer wants it on for some tiers.)
- The `ghosting` field is added to the shared `TaskConfig` struct (`Option<bool>`, default
  `None`); other tasks ignore it, exactly like `input_filter` / `dimensions`.

**OSS template** (`examples/model_config.toml`): keep `[tasks.pde_decision]`, flip the
"reserved — not consumed" comment to describe the opt-in LLM layer, and add a
commented-out sample `filter_prompt`. Default ships **off** (no `filter_prompt`).

**Schema lock**: add a `filter_prompt` line to `pde_decision` in `COMPAT_FIXTURE`
(additive — does not break the lock) and assert `resolve_pde()` resolves from it.

---

## 5. Guardrails (`eros-engine-core`)

**Design decision — the LLM owns the ghost decision; the rules keep only the hard-safety
vetoes.** Today `ghost::decide` has four protection layers plus a score-threshold test.
The score-threshold layer (the `(1-intrigue)*0.4 + …` formula vs `0.65` / `0.85`) is the
crude "when to ghost" heuristic this spec replaces — it is **deliberately ceded to the
LLM**. The guardrails therefore preserve only the **hard-safety protections** (new
relationship / anti-streak / cooldown). This means the LLM *can* ghost in cases the old
score would have replied — that is the intended behaviour (goal #3), not a regression.
This is **hard-safety only**, not "the LLM can never ghost where the rules would reply."

Extract the hard-safety layers (sans score) from `ghost.rs` as a pure predicate the rule
engine and the LLM-guard path both call:

```rust
/// True when a ghost is permitted by the HARD-SAFETY protections (the score
/// layer is intentionally excluded — the LLM decides ghost-worthiness).
pub fn ghost_permitted(a: &Affinity, s: GhostSignals) -> bool;
//   false if: a.ghost_streak >= 2  (from &Affinity)
//          OR s.message_count < 10  OR  s.hours_since_last_ghost < 1.0  (from GhostSignals)
```

**Field sourcing (matches `ghost::decide` exactly):** `ghost_streak` is read from
`&Affinity` (`a.ghost_streak`, `ghost.rs:34`), while `message_count` /
`hours_since_last_ghost` come from `GhostSignals` (which carries only those two —
`ghost.rs:9`). Do not move `ghost_streak` onto `GhostSignals`.

`ghost::decide` is refactored to call `ghost_permitted` for its protection layers and then
apply the score test itself, so the rule fallback keeps its full (protection + score)
behaviour unchanged while the LLM path uses `ghost_permitted` alone.

Tip is **not** a parameter — handled at the call site: the rule path (`pde::decide`) forces
tip → reply before ghost logic, and on the LLM path tip turns skip the judge entirely (§6),
so a tipped turn never reaches `ghost_permitted`.

**Guardrails only downgrade toward `ReplyText`; never upgrade.** Applied to the judge's
proposed action:

| Judge proposes | Condition | Acted action |
| --- | --- | --- |
| `ghost` | `pde_ghosting_enabled() == false` (§4.1 kill-switch) | `ReplyText` |
| `ghost` | `ghost_permitted == false` (new relationship / streak / cooldown) | `ReplyText` |
| `ghost` | `ghost_permitted == true` | `Ghost` |
| `reply_image` / `reply_text_image` | `tasks.chat_image_generation` not configured/wired (always, today) | `ReplyText` |
| `reply_text` | — | `ReplyText` |

**The `ghosting` kill-switch (§4.1) is enforced path-wide, not just here.** Because it must
also suppress a *rule-fallback* or *pure-rule-engine* ghost (ratified scope), it is applied
as a **final step** on the computed `plan` regardless of source (§6): if
`!pde_ghosting_enabled() && plan.action_type == Ghost`, degrade to `ReplyText` (keeping any
`context_hints`). The table row above is the LLM-path view of that same check.

The judge can never ghost past the **hard-safety** vetoes (new relationship / streak /
cooldown), and can never produce an image reply before the executor exists. Within those
vetoes the judge — not the score formula — decides ghost-worthiness (§5 design decision).

---

## 6. Stream wiring (`stream.rs::run_stream`)

Today: build `input` → `pde::decide(&input)` → `match plan.action_type`.

New (runs **every turn**, **before** vision / input-filter / prompt assembly, so a `ghost`
verdict short-circuits all of them):

```text
build DecisionInput `input`
transcript = fetch recent history ONCE                         // shared by judge + input-filter
hints = []                                                     // sanitized LLM inner_state, if any; [] on rule paths
(plan, run) =
  if tip turn OR resolve_pde() == None:
      (pde::decide(&input), None)                            // rule engine (today's behaviour); no judge
  else:
      run = run_pde_decision(p, ctx(transcript, input))      // §3.2 — status-bearing
      plan = match run.status:
        Ok => {
          action = guardrails(run.verdict.action, &input)    // §5 — ghost_permitted + image degrade
          hints  = sanitize(run.verdict.inner_state)         // §3.3; [] if empty after sanitize
          pde::plan_for(&input, action, hints)               // §2.3 — Neutral style, heuristic deltas inside
        }
        _  => pde::decide(&input)                            // fail-open to rule engine
      (plan, Some(run))

// Ghosting kill-switch (§4.1) — FINAL gate, every path (LLM / fallback / pure-rule / tip).
// Use the in-scope `hints` (NOT plan.context_hints): a plan_for(Ghost) plan carries no
// hints, but an LLM ghost still produced an inner_state we keep on the forced reply — same
// as the hard-safety guardrail downgrade (which keeps hints because guardrails returned
// ReplyText before plan_for). On rule paths `hints` is [] anyway.
if !pde_ghosting_enabled() && plan.action_type == Ghost:
    plan = pde::plan_for(&input, ReplyText, hints)

// Audit (§8, best-effort) — only when the judge ran; logs the FINAL acted action:
if let Some(run) = run:
    spawn audit_row(status = run.status,
                    proposed_action = run.verdict.map(|v| v.action),   // judge's raw proposal; None unless status==Ok
                    action = plan.action_type,                          // post-kill-switch → reply_text if suppressed
                    payload = run.verdict.as_json().or(run.raw),        // verdict JSON when Ok, else raw model text
                    model = run.model, usage = run.usage, generation_id = run.generation_id)

match plan.action_type {
    Ghost                                  => …unchanged ghost arm…,   // skips vision + input-filter
    ReplyText | ReplyImage | ReplyTextImage => Reply path,            // explicit arms — NOT the `_` catch-all
    Proactive                              => …unchanged…,
}
```

- **Judge runs first.** A `ghost` verdict routes to the (unchanged) Ghost arm, which never
  reaches `run_vision` / `run_input_filter` (those live inside the Reply arm) — so a ghost
  pays only the judge call, not vision + input-filter + chat.
- **One shared history fetch.** The judge's `ctx` and the input filter today each fetch
  recent history separately. Fetch it **once** up front and pass it to both, removing the
  duplicate round-trip the input-filter comment already flags.
- **Latency budget (opted-in reply turn).** judge LLM → input-filter LLM → chat LLM,
  serial, before first token. The judge adds **one** blocking round-trip over today; vision
  / input-filter are unchanged. State this explicitly; the user accepted it (every-turn
  judge) given the short-circuit + shared-fetch mitigations. (A probability/trigger gate
  remains a config-level option if cost ever bites, but is not enabled by default.)
- **Tip turns skip the judge** — the rule path handles tip → reply + `tip_personality`
  tone; matches `input_filter`'s tip-skip (vision also does not run for a valid tip turn,
  which carries no image).
- **`context_hints` is existing plumbing** — `build_prompt` already renders it as
  `[inner_state]`. No new prompt parameters; the dormant channel just gets populated (with
  sanitized text, §3.3).
- **Explicit `match` arms.** `ReplyImage` / `ReplyTextImage` get explicit `=> Reply path`
  arms — they must **not** fall into the `Proactive` `_` catch-all (which would emit a
  Final-only frame, no reply). Today they are already degraded to `ReplyText` by the
  guardrail, so the arms are belt-and-suspenders; they become real branches when the
  executor ships.

---

## 7. Replay & persistence (no new replay-critical state)

`replay_stream` serves the **persisted outcome**, not the decision:

- Ghost: the `ghost_decision` boolean on the user row → `replay_stream(ghost=true)` emits
  Ghost frames.
- Reply: the stored assistant content rows → replayed verbatim (content, model, usage,
  generation_id).

The PDE verdict (inner_state, proposed action) only shapes the **prompt** at generation
time; once the assistant text exists (or the ghost flag is set), the outcome is fully
**replayable without it**. (Note: replay was never byte-for-byte wire-identical even today
— ghost replay mints a fresh ULID and the replayed `Final` carries `tier=None` /
`prompt_injected=None` / `filtered=false` / `retries=0`; that pre-existing behaviour is
unchanged and unaffected by this spec.) `run_stream` only runs on a fresh idempotent
insert, so the judge never re-runs on a retry.

**Therefore no verdict persistence is required for correctness.** (This supersedes the
earlier brainstorming note about writing the verdict to the user row — it is unnecessary.)
`companion_decision_events` (§8) is purely best-effort audit.

---

## 8. `companion_decision_events` audit table

Modelled on `companion_insights_events` (migration 0025) — append-only, **best-effort
telemetry** (≈ one row per judge run), `run_id` join key, `status` CHECK, `payload` JSONB,
OpenRouter audit trio, Supabase lockdown.

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

- **Best-effort telemetry, ≈ one row per judge run** — only on turns where the judge runs
  (feature on, non-tip). The write is fire-and-forget (§8.4), so under shutdown / backpressure
  a row **may be dropped**; this table is telemetry, **not** a guaranteed ledger. Do not
  build correctness on its completeness. Feature-off / tip turns make no LLM call and write
  **no row** (faithful to `companion_insights_events`' "one row per OpenRouter call").
  Pure-rule decisions are deterministic and already replayable, so they need no audit.
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

Write the row **after the final acted `plan` is computed** (i.e. after the `ghosting`
kill-switch gate, §6, so `action` is the FINAL acted action) — for **every** judge run
regardless of `status` (so `Ok`, `Empty`, `ParseError`, `Timeout`, `Error` are all
captured, including the fail-open-to-rule path) — via `tokio::spawn` (best-effort,
warn-on-error). The spawned task owns a clone of the data it needs (`run_id`, ids, status,
final acted `action`, `proposed_action`, payload, audit trio) plus the pool.

- **Why inline, not post_process**: a `Ghost` turn does **not** run post_process (the Ghost
  arm only marks + records + emits frames). Ghost is the most important decision to audit,
  so the write must sit where both `Ghost` and reply paths pass — i.e. the decision site.
- **Why fire-and-forget**: the PDE runs before generation (blocking); an awaited INSERT
  would add to pre-first-token latency. The cost is that a row can be lost on shutdown /
  backpressure — acceptable for telemetry (§8.2), and consistent with the warn-on-error
  insight-event writes.

---

## 9. `affinity_deltas` / `energy_cost`

- `affinity_deltas`: on the LLM path they come from the same `predict_reply_deltas`
  heuristic — but the LLM path calls it **via `pde::plan_for`** (§2.3), never directly
  (the function and the `ENERGY_COST_*` constants stay private to `pde.rs`). The judge does
  **not** produce affinity deltas — that overlaps `affinity_evaluation`. So
  `merge_deltas(plan.affinity_deltas, llm_deltas)` in `post_process.rs:158` is unaffected,
  and `post_process`'s `run_eval` gate (`plan.action_type == ReplyText`, post-rename) still
  fires correctly because guarded image actions are degraded to `ReplyText` (§2.3 invariant).
- `energy_cost`: vestigial; `pde::plan_for` sets it from the existing rule constant. Out
  of scope to remove.

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
- `docs/model-config.md` + `.zh`: document `[tasks.pde_decision].filter_prompt`, the four
  actions, and the `ghosting` kill-switch (default `true`).
- `README.md` + `.zh`: update any "PDE = rule-based" phrasing and the table list.
- `COMPAT_FIXTURE` (`model_config.rs`): add `filter_prompt` to `pde_decision` (additive;
  optionally `ghosting` too).

---

## 12. Testing

- **core**: `ghost_permitted` unit tests (msg<10 / streak / cooldown → false; clear case →
  true), reading `ghost_streak` from `Affinity`; `ghost::decide` still applies the score
  layer after the refactor (its existing tests stay green); `pde::plan_for` builds the
  expected `ActionPlan` (Neutral style, heuristic deltas, hints → context_hints); existing
  `pde::decide` tests stay green after the `ReplyText` rename; `ActionType::is_text_reply`
  truth table.
- **model_config**: `resolve_pde` (task absent → None; blank `filter_prompt` → None; set →
  `Some`); `pde_ghosting_enabled` (absent → true; `false` → false; task missing → true);
  compat fixture asserts the new `filter_prompt`.
- **server**:
  - `parse_pde_verdict` (all four actions, missing/extra fields, junk → None).
  - `run_pde_decision` status mapping: `Ok` / `Empty` / `ParseError` (raw kept) /
    `Timeout` / `Error` each produce the right `PdeDecisionRun.status`.
  - Guardrail application: tip → ReplyText; msg<10 vetoes ghost; ghost honoured when
    permitted; `reply_image`/`reply_text_image` → ReplyText today; fail-open → rule.
  - **`inner_state` sanitization** (§3.3): hostile inputs — injected `[output]` /
    `[iron_rules]` / `---` section markers, overlong text, control chars — leave the
    rendered prompt structurally intact.
  - **Invariant test**: a guarded plan never carries an un-degraded image action; the
    `match` never hits the `_` catch-all for a reply.
  - **`ghosting = false` kill-switch** (§4.1): suppresses ghost on all three paths, each →
    `ReplyText`, with the correct audit shape per path:
    - LLM-Ok `ghost` verdict → row with `status=ok`, `proposed_action=ghost`,
      `action=reply_text`; **the LLM's sanitized `inner_state` is preserved** on the
      forced reply (hints not lost).
    - LLM-failure rule fallback (`status != ok`) yielding a rule ghost → row with
      `proposed_action=NULL` (no verdict), `action=reply_text`.
    - Pure-rule engine (LLM off) / tip → ghost suppressed → **no audit row** (no judge ran).
  - One stream E2E: judge → ghost (short-circuits vision/input-filter), and inner_state
    injected into the prompt on a reply turn.
- **store**: `DecisionEventRepo::record` round-trips a row (run_id, status, payload, audit
  trio) and a `parse_error` row (raw text payload, NULL `proposed_action`), mirroring the
  insight-event test.

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
| `crates/eros-engine-core/src/types.rs` | `ActionType`: `Reply`→`ReplyText`; add `ReplyImage`, `ReplyTextImage`; **keep `Proactive`**; **drop the unused `Serialize`/`Deserialize` derive**; add `ActionType::is_text_reply()`. |
| `crates/eros-engine-core/src/ghost.rs` | Extract `ghost_permitted(a: &Affinity, s: GhostSignals) -> bool` (hard-safety layers only, no score; `ghost_streak` from `a`); `decide` calls it then applies the score test. |
| `crates/eros-engine-core/src/pde.rs` | Rename `Reply`→`ReplyText`; add `pub fn plan_for(input, action, hints) -> ActionPlan` (keeps `predict_reply_deltas` / `ghost_affinity_deltas` + `ENERGY_COST_*` private). |
| `crates/eros-engine-llm/src/model_config.rs` | `ResolvedPde` + `resolve_pde()`; `ghosting: Option<bool>` on `TaskConfig` + `pde_ghosting_enabled()` (default true); add `filter_prompt` to `COMPAT_FIXTURE` + tests. |
| `crates/eros-engine-server/src/pipeline/stream.rs` | `PdeVerdict` / `PdeStatus` / `PdeDecisionRun`, `parse_pde_verdict`, `run_pde_decision` (status-bearing); `inner_state` sanitizer (§3.3); `run_stream` decision flow (judge-first, shared history fetch, calls `pde::plan_for`); **path-wide `ghosting` kill-switch final gate** (§4.1); **explicit `ReplyText`/`ReplyImage`/`ReplyTextImage` match arms** (not `_`); best-effort `tokio::spawn` audit write for every status (logs final acted action); `ActionType::Reply`→`ReplyText`. |
| `crates/eros-engine-server/src/pipeline/post_process.rs` | `ActionType::Reply`→`ReplyText` (rename only; image variants never reach here per §2.3 invariant). |
| `crates/eros-engine-store/src/decision.rs` | **New** — `DecisionEventInsert` + `DecisionEventRepo::record`. |
| `crates/eros-engine-store/src/lib.rs` | `pub mod decision;`. |
| `crates/eros-engine-store/migrations/0028_companion_decision_events.sql` | **New** — table + indexes + Supabase lockdown. |
| `examples/model_config.toml` | `[tasks.pde_decision]`: live comment + commented sample `filter_prompt` (with the `inner_state`-must-be-mood-not-instructions clause) + document `ghosting = false` (default `true`) as the downstream safety switch. |
| `docs/architecture.md` / `.zh`, `docs/model-config.md` / `.zh`, `README.md` / `.zh` | PDE description + schema/flow updates. |

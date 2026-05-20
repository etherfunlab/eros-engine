# Six-axis affinity activation (engine) — Design

**Status**: design, pending implementation plan
**Date**: 2026-05-20
**Repo**: `eros-engine` (OSS, AGPL-3.0)
**Related**:
- `2026-05-20-affinity-event-delta-design.md` — the `/bff/v1/comp/affinity/{sid}/event` surface this feeds.
- `2026-05-20-affinity-source-migration-design.md` (eros-engine-web) — the FE consumer. This spec is its backend half.

---

## 0. Background & goal

The six-axis affinity vector (`warmth`, `trust`, `intrigue`, `intimacy`, `patience`, `tension`) is meant to move as a user chats, drive the relationship-stage label, and be visible per-turn in the FE "好感度变化" strip. In practice it barely moves, and **`intimacy` / `warmth` / `trust` never move at all** during normal chat.

### Goal

1. Make all six axes actually respond to conversation, so meters move and stage labels change.
2. Users should *perceptibly* see affinity change while chatting — target **a clear shift every ~3–5 strong turns** — without it feeling crude.
3. Stay decoupled: no coupling between the message SSE stream and affinity computation (confirmed direction — see the FE spec §8).
4. Engine-only change. Zero FE change required; the FE migration spec already consumes what this produces.

---

## 1. Root cause (current state)

The chat pipeline's only affinity-delta source is the deterministic `predict_reply_deltas` in `eros-engine-core/src/pde.rs`. It writes **three** axes only:

```rust
d.intrigue += 0.02   // user message ≥ 30 chars
d.patience += 0.02 / -0.02 / -0.05   // long / short / stale
d.tension  += 0.03   // stale > 24h
```

It **never writes `warmth`, `trust`, or `intimacy`** — they stay at `AffinityDeltas::default()` (`0.0`). `Affinity::apply_deltas` then computes `x += (1 − ema_inertia) × delta`, so for those three axes `x += gain × 0 = 0` — frozen regardless of `ema_inertia`. The "EMA" knob is therefore irrelevant to the frozen axes; it is in reality a **linear delta gain**, not an exponential moving average.

The intended LLM layer was never built: `pde.rs` says *"Phase 2: rules only. Phase 6 adds the LLM fallback path."* Phase 6 never landed. A `[tasks.pde_decision]` slot exists in `model_config.toml` but is explicitly *"not consumed by current code."*

---

## 2. Design overview

Keep the deterministic rules for the **behavioral** signals they already handle (timing, message length, ghosting). Add an **LLM evaluator** for the **semantic** signals (what was actually said, emotional depth, flirtation). Sum the two contributions and persist a single event per turn through the existing `persist_with_event` path. The FE's existing baseline-and-poll on `/event` picks it up.

```
User turn ─▶ pde::decide (request path, sync)
               └─ rule deltas: patience + timing bumps to intrigue/tension     [behavioral]
                         │  (carried on plan.affinity_deltas, unchanged)
SSE reply streams ─▶ done ─▶ FE captures baseline event_id, then polls /event (existing)
                         │
           post_process (tokio::spawn, async — existing) :
               ├─ load current affinity (+ time_decay)                  [existing]
               ├─ NEW affinity LLM eval (haiku) ─▶ llm deltas:
               │     warmth, trust, intimacy  + content bumps to intrigue/tension   [semantic]
               ├─ combined = rule_deltas + llm_deltas   (per-axis sum, clamped)
               └─ persist_with_event(combined, gain) ─ ONE event row (new event_id) [existing]
                         │  updates engine.companion_affinity (absolute vector)
                         │  inserts companion_affinity_events (effective_deltas)
                         ▼
           FE poll sees new event_id within ~2s (budget 5s) ─▶ renders effective_deltas
           FE detail panel reads absolute vector from Supabase on open
```

No SSE protocol change. No new public engine endpoint. No FE change.

---

## 3. Axis ownership (hybrid by signal type)

| axis     | range     | rule (behavioral)              | LLM (semantic)            |
|----------|-----------|--------------------------------|---------------------------|
| warmth   | −1 .. 1   | —                              | ✓ cold/warm/hostility     |
| trust    | 0 .. 1    | —                              | ✓ self-disclosure, consistency |
| intrigue | 0 .. 1    | ✓ long msg / question / new topic | ✓ content novelty       |
| intimacy | 0 .. 1    | —                              | ✓ emotional / physical closeness |
| patience | 0 .. 1    | ✓ timing / short reply / ghost | —                         |
| tension  | 0 .. 1    | ✓ stale bump                   | ✓ flirtation / conflict   |

- **Rule-only**: `patience`. (Pure pacing/tolerance — no LLM needed.)
- **LLM-only**: `warmth`, `trust`, `intimacy`.
- **Both (summed)**: `intrigue`, `tension`. Rules give the timing nudge; LLM gives the content nudge.

The existing `predict_reply_deltas` already produces exactly the rule half — **no change to its axis coverage is required.** Magnitudes stay as-is for v1 (they are intentionally small behavioral nudges; the LLM is the primary mover). They remain tunable later.

---

## 4. The LLM evaluator (the new piece)

### 4.1 Placement & lifecycle

- Runs inside `post_process` (`pipeline/post_process.rs`), as a sibling fire-and-forget task to the existing `extract_insights` / `write_turn` LLM work, joined via the existing `tokio::join!`.
- Runs **only on `Reply` turns** with a non-empty user message **and** a non-empty produced assistant message. Other actions are unchanged:
  - `Proactive` (no user message) → rules only, no eval.
  - `Ghost` → all-zero, unchanged.
  - **Gift — two distinct paths, both unchanged in v1:** (1) the `/comp/chat/{sid}/event/gift` route applies *client-supplied* deltas directly; (2) the pipeline `GiftReaction` action currently applies PDE default (zero) deltas (`pde.rs:36`). The LLM eval is **not** wired to gift turns in v1 (a follow-up could evaluate gift reactions).
- Never blocks the chat response — it is already after the SSE stream completed.
- **Optional cost gate:** skip the eval on trivially short user turns (e.g. `< SHORT_MSG_CHARS`), where there's nothing semantic to score — saves a haiku call on "k"/"ok"-type turns. Rules still apply. Tunable; off → eval every reply turn.

### 4.2 Wiring

The evaluator needs the turn's `user_msg` + `assistant_msg` + current `Affinity` + persona name:

- `user_msg` — from `event` (already in `post_process::run`).
- `assistant_msg` — **`produced` is a `Vec<ProducedMessage>`** (the streaming path can emit a multi-message burst for one user turn). Join the burst into one assistant text (`produced.iter().map(|m| m.full_text).join("\n")`); run **one eval per turn**, write **one combined event**. Empty join → no eval.
- `Affinity` — loaded in `persist_affinity` (existing).
- **persona** — **not** currently passed to `post_process::run` (it has `instance_id`, not the persona). Load it in the affinity future via `PersonaRepo::load_companion(instance_id)` (one extra indexed query, fine in a background task) and pass the persona name to the prompt. Persona name only; the system prompt is not needed.

Restructure the affinity future so it:

1. computes `llm_deltas` (Reply turns only; else `AffinityDeltas::default()`),
2. sums onto `plan.affinity_deltas` (the rule deltas already passed in),
3. calls `persist_affinity` with the combined deltas **and the evaluator `reason`**.

`persist_affinity` keeps its overall shape (load → time_decay → `persist_with_event`). Its inputs change: the combined deltas, and the `context` JSONB now carries `{ "affinity_reason": ... }` instead of today's hardcoded `json!({})` (§6).

### 4.3 Model task

New `[tasks.affinity_evaluation]` in `examples/model_config.toml`:

```toml
[tasks.affinity_evaluation]
model = "anthropic/claude-haiku-4.5"
fallback = ["google/gemini-3.1-flash-lite", "deepseek/deepseek-v4-flash"]
temperature = 0.3
max_tokens = 250
```

Resolved via the existing `ModelConfig::resolve("affinity_evaluation", None)`. `pde_decision` keeps its reserved meaning (not repurposed).

**Deployment caveat (don't ship only `examples/`):** the server loads its config from `MODEL_CONFIG_PATH` (`main.rs:230`; prod = `/etc/eros-engine/model_config.toml` in the Docker image), and `resolve()` on a **missing** task does not error — it warns and silently falls back to `defaults.fallback_model` → `x-ai/grok-4-mini` (`model_config.rs:107`). So the affinity eval would silently run on the wrong model if the *deployed* config lacks the task. The new `[tasks.affinity_evaluation]` block must be added to the **deployed** config (and the Dockerfile/image source), not just `examples/`. Consider promoting the "unknown task" `warn!` to a startup assertion for tasks the code depends on.

### 4.4 Prompt (new fn in `prompt.rs`)

`affinity_eval_prompt(persona_name, affinity, user_msg, assistant_msg)` → a single user-role prompt that:

- States the persona and the six axes with one-line definitions and current values.
- Provides this turn's exchange (user message + AI reply).
- Instructs: output **only** the LLM-owned axes (`warmth`, `trust`, `intimacy`, `intrigue`, `tension`) as per-turn *changes* (not absolute values), each a small float; `patience` is omitted (rule-owned). Magnitude should reflect how significant the turn was: near-zero for small talk, larger for genuine emotional/flirtatious moments. Bound guidance: roughly ±0.15 per axis.
- Demands strict JSON only.

Output contract:

```json
{ "warmth": 0.08, "trust": 0.03, "intimacy": 0.06, "intrigue": 0.02, "tension": -0.01, "reason": "短句，用于日志/debug" }
```

### 4.5 Parse, clamp, failure

- Parse with the existing `find_json_block` helper (already in `post_process.rs`), then `serde` into a deltas struct.
- **Clamp each LLM axis to `[-0.15, 0.15]`** before summing (safety guardrail against a misbehaving model — independent of the pacing gain).
- Any failure (LLM error, timeout, non-JSON, missing fields) → LLM contribution = `AffinityDeltas::default()`. The rule deltas still persist. The affinity write **never** fails because the evaluator failed.
- `patience` from the LLM is ignored even if present.

---

## 5. Merge & pacing

- **Combine**: `combined[axis] = rule_delta[axis] + clamped_llm_delta[axis]` per axis.
- **Gain (pacing knob)**: `persist_with_event` applies `apply_deltas` → `x += (1 − ema_inertia) × combined`, then clamps to each axis's valid range. Keep `ema_inertia` as the **single global pacing lever** (env `EMA_INERTIA`, no redeploy).
- **Per-axis safety cap**: ±0.15 raw (LLM) — a guardrail, *not* the pacing lever. The two jobs are deliberately separate.

### Chosen numbers (v1)

| knob | value | effect |
|------|-------|--------|
| LLM raw cap / axis | ±0.15 | ceiling per turn before gain |
| `ema_inertia` (gain) | 0.5 (gain 0.5) | effective ≤ 0.075/turn on a hot axis |
| result | — | **~4 strong turns ≈ +0.3** on an active axis → meter visibly fills, can cross a stage threshold |

This satisfies the "clear shift every ~3–5 turns" target while staying "earned". `DEMO_EMA_INERTIA` (demo sessions) stays a separate, faster value as today.

The pre-EMA `combined` is stored verbatim in the event row's `deltas`; the post-EMA, post-clamp change is stored in `effective_deltas` (existing behavior of `persist_with_event`). The FE renders `effective_deltas`.

---

## 6. Persistence & event semantics

- **One combined event per turn** — no protocol or event-log change. `persist_with_event` already:
  - updates `engine.companion_affinity` (absolute vector, read by the FE from Supabase),
  - re-infers `relationship_label` (now meaningful since `warmth`/`intimacy`/`trust` move → stage labels actually change),
  - inserts one `companion_affinity_events` row with pre-EMA `deltas` + post-EMA `effective_deltas` + `context`.
- **Store the evaluator's `reason`** in the event `context` JSONB (e.g. `{ "affinity_reason": "..." }`) for debugging. This replaces today's `json!({})` context on message turns.
- The FE's existing baseline-and-poll (`affinityPoll.ts`, budget 12 × 400 ms ≈ 5 s) catches the new `event_id` after the spawned post-process writes it (~1–2 s). No timing change needed.

### 6.1 `deltas` vs `effective_deltas` — no batching

`persist_with_event` stores two values per event:

- **`deltas`** = the **raw combined request** for this turn (rule + LLM, *before* gain & clamp) — "what we asked for".
- **`effective_deltas`** = the **actual per-turn change** applied to the stored vector = `after − before` = `(1 − ema_inertia) × delta`, minus any 0/1-ceiling loss — "what really moved".

There is **no accumulate-until-threshold batching** anywhere in the engine. Each event row is exactly one turn's increment. **Accumulation lives entirely in the absolute `companion_affinity` vector** (the running sum of `effective_deltas` over all turns).

The "clear shift every ~3–5 turns" target therefore emerges from **two layers, not batching**:

1. **Continuous** — each strong turn writes a real `effective_deltas` increment (~0.075 on a hot axis at v1 pacing), comfortably above the FE's 0.005 skip floor, so the per-turn strip renders every meaningful turn.
2. **The "big moment"** — `infer_label()` re-runs every turn inside `persist_with_event`; when the accumulated absolute vector crosses a stage threshold (e.g. `warmth ≥ 0.7 && intimacy ≥ 0.4 && tension ≥ 0.3 → Romantic`), `relationship_label` flips and the FE fires a stage-transition badge. At ~0.075/turn a threshold is crossed roughly every few turns — that is the periodic "大变化", driven by accumulation crossing a line, not by artificial batching.

A silent-accumulate-then-reveal model (meters dead for N turns, then one jump) is explicitly **not** adopted: it feels unresponsive between reveals, and the stage-transition layer already supplies the drama on top of continuous feedback.

### 6.2 Concurrency: prevent lost updates (required by this change)

Affinity persistence is a **read-modify-write** with the read (`load_or_create`, `post_process.rs:146`) **outside** the write transaction, and `post_process` is `tokio::spawn`ed and **not awaited** (`stream.rs:437`, `mod.rs:198`). Two turns whose post-process tasks overlap both read the same vector, both apply their deltas to that stale snapshot, and the second `UPDATE` wins → one turn's increment is silently lost.

Today this is harmless because deltas are ~0. **This change makes it a real correctness bug**: deltas become meaningful *and* the ~1–2 s LLM eval widens the overlap window from milliseconds to seconds, so a user sending a follow-up within a couple seconds reliably triggers it — silently corrupting the core IP metric.

**Requirement:** the read-modify-write must be serialized per session. Do the current-value read **inside** the `persist_with_event` transaction under a row lock (`SELECT ... FOR UPDATE` on the `companion_affinity` row), apply deltas to the freshly-locked values, then `UPDATE` + insert the event in the same tx. Exact handling of `apply_time_decay` relative to the locked read is an implementation detail for the plan (decay must be computed from the locked row, not a pre-read snapshot). This converts overlapping same-session writes into a serialized sequence — no lost increments, monotonic event ordering.

(The FE's `client_msg_id`→event echo for perfect turn↔event causality remains out of scope — see the FE spec's accepted residual edge.)

---

## 7. Scope

**In:**
- `Reply`-turn six-axis movement via hybrid rules + LLM.
- New `affinity_evaluation` model task + prompt + parse/clamp/merge, added to the **deployed** config (§4.3).
- Persona-name load in the affinity future; multi-message burst join (§4.2).
- Store the evaluator reason in event context (§6).
- **Row-level locking in `persist_with_event` to prevent lost updates (§6.2).**

**Out (later / not now):**
- Time-decay for `warmth`/`trust`/`intimacy` (relationship cooling on neglect). `apply_time_decay` stays as-is (intrigue/patience/tension only). Note as a follow-up.
- Per-turn LLM eval for `Proactive` and `GiftReaction` turns.
- Two-phase UI (instant rules → delayed LLM as two events). The FE poll returns on the first new `event_id`; a single combined event is the deliberate v1 choice.
- Any SSE/stream coupling (explicitly rejected — FE spec §8).
- Tuning rule magnitudes (kept as-is for v1).
- Renaming the misleading `ema_inertia` / "EMA" identifiers to "gain" — the math is documented (§1, §5) but a rename would break the deployed `EMA_INERTIA` fly secret and struct/param names; deferred.

---

## 8. Frontend consumers (context — no change required here)

Per `2026-05-20-affinity-source-migration-design.md`:
- **Per-turn delta** ("好感度变化" strip) ← `/bff/v1/comp/affinity/{sid}/event` `effective_deltas`. The FE **skips any delta where every axis `|v| < 0.005`** — v1 pacing (≤0.075 on hot axes) clears this comfortably; flat turns correctly render nothing.
- **Absolute vector** (radar + stage pill) ← Supabase `engine.companion_affinity` directly (lazy, on detail-panel open).
- The header **IntimacyBar** is `agent_training_level` (insight-driven), independent of this work.

Because both FE surfaces are fed by `persist_with_event`, writing real combined deltas there is sufficient — no engine API or FE code change.

---

## 9. Error handling & observability

- Evaluator failure → rule-only deltas; write still proceeds (§4.5).
- Log evaluator usage via the existing `log_openrouter_usage("affinity_evaluation", Some(session_id), &resp)`.
- Log the parsed `reason` at `debug` and persist it in event `context` for after-the-fact inspection.
- Existing `tracing::warn!` on affinity persist failure is retained.

---

## 10. Testing

- **core** (`affinity.rs`): existing `apply_deltas` / clamp / label tests cover the math. Add a focused test that a combined (rule + llm) delta sums then gains/clamps as expected (pure function, no LLM).
- **post_process** (`post_process.rs`): unit-test the evaluator JSON parse + per-axis clamp + "patience ignored" + failure-returns-default, deterministically (mirrors the existing `parse_facts` / `find_json_block` tests). The live LLM call itself is not unit-tested (consistent with `extract_facts`).
- **store** (`affinity.rs`): existing `persist_with_event` tests already assert pre/post-EMA `deltas`/`effective_deltas` and clamping. **Add a concurrency test** (`sqlx::test`): two overlapping `persist_with_event` calls on the same session each apply their increment (final = sum of both, no lost update) — guards the §6.2 locking.
- **prompt** (`prompt.rs`): snapshot/format test that `affinity_eval_prompt` includes the six current values and the turn exchange.
- **Stage labels**: a test that a turn pushing `warmth`/`intimacy` over thresholds flips `relationship_label` (via `infer_label`, already covered) — confirms the now-live axes drive labels.

---

## 11. Open questions

None blocking. Rule-magnitude tuning, warmth/trust/intimacy decay, and Proactive-turn evaluation are explicitly deferred (§7).

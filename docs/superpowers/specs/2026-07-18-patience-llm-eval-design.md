# eros-engine — LLM-evaluated patience (absolute read + rule delta)

**Status**: design, pending implementation plan
**Target release**: `0.8.x` dev track. **No migration.**
**Scope**: bring the `patience` affinity axis into the per-turn LLM evaluation so it
moves at a meaningful pace. Today `patience` is **rule-owned** — the affinity
evaluator is told not to touch it, and it drifts only by tiny deterministic nudges
(±0.02 / ±0.05 raw, then halved by EMA), so it crawls. This spec has the existing
`affinity_evaluation` LLM emit an **absolute** patience level, combines it with the
PDE rule delta, and writes the result **directly** (bypassing EMA and the per-axis
caps for patience only). No new LLM round-trip; no config flag.

---

## 0. Background

### 0.1 patience today

- **Range** `[0, 1]`, seed `0.5` (migration 0029). One of six affinity axes.
- **Rule-owned.** `affinity_eval_prompt` states "patience 由规则维护，请勿评估"; the
  evaluator's JSON schema omits patience; `parse_affinity_eval` force-zeroes it;
  `merge_deltas` therefore keeps the rule value only.
- **Per-turn rule delta** (`pde.rs::predict_reply_deltas`): long user msg (≥30 chars)
  `+0.02`, very short (≤3) `−0.02`, stale gap (>24h) `−0.05`. Ghost turns compute a
  *separate* `ghost_affinity_deltas()` (patience `−0.05`) onto the `ActionPlan`, but
  `persist_affinity` routes Ghost to `record_ghost` instead of `persist_with_event`,
  which discards it — so patience is **not** actually moved on a Ghost turn (see §1.4).
- **EMA** (`Affinity::apply_deltas`, default `ema_inertia=0.5`): the combined delta is
  halved before landing, then clamped to `[0,1]`.
- **Time decay** (`apply_time_decay`): patience **recovers** `+0.005/day` when idle —
  the only axis that drifts upward.

Net: max per-turn change ≈ `0.05 × 0.5 = 0.025`. Going 0.5 → 0.9 takes ~16 turns.
This is the "太慢" the change targets.

### 0.2 What patience feeds

- **Ghost score** — `(1−intrigue)·0.4 + (1−patience)·0.4 + tension·0.2` (`ghost.rs`).
  patience is 40% of the score. When the **LLM PDE judge** is on, the score-threshold
  layer is skipped (the judge decides ghost-worthiness); the deterministic
  `ghost::decide` uses the score only when the judge is off/failed. Hard protections
  (msg-count floor, anti-streak, cooldown) always hold.
- **Prompt attitude directive** (`prompt.rs`, only when the patience scope is active):
  `<0.3` → curter/shorter replies; `>0.7` → patient, willing to chat.
- **Not** part of the Bond/Chemistry derived lines (rule-owned, excluded by design).

### 0.3 Where the two LLM touchpoints sit

1. **Pre-response PDE judge** (opt-in, `stream.rs`): decides action / ghost / inner_state.
2. **Post-response `affinity_evaluation`** (`post_process.rs`): the haiku call that
   produces the six-axis semantic deltas. Runs on every substantive reply turn.

patience is updated at the **end** of a turn and read at the **start** of the next —
so updating it in the post-response evaluator (2) is time-consistent with today and
requires **no new round-trip**. This spec uses touchpoint (2).

---

## 1. The mechanism

### 1.1 LLM emits an absolute patience level

The `affinity_evaluation` call gains one field: `patience`, an **absolute** value in
`[0, 1]`, one decimal place (`0.0 / 0.1 / … / 1.0` — ten steps). The other five axes
stay **deltas**, exactly as today.

On receipt the engine **snaps** the model's `patience` to the nearest `0.1` and clamps
to `[0, 1]` (so `0.83` → `0.8`, a robust guard against off-grid output). Call the
snapped absolute read `L`.

`patience` is `Option<f64>` in the parse struct: **absent → `None`** (old prompt, parse
failure, or the model omitting it) → the turn falls back to rule-only behaviour (§1.4).

### 1.2 Combine with the rule delta

`R` = the PDE's per-turn patience rule delta (`plan.affinity_deltas.patience`,
unchanged). The turn's patience **target** is:

```
patience_target = clamp(L + R, 0, 1)
```

- `L` is on the 0.1 grid; `R` is a small off-grid nudge. The **final target is not
  re-snapped** — snapping it would round `R` away and defeat "再加规则 delta". The 0.1
  quantisation constrains the LLM read only, not the stored value.

### 1.3 Write it directly (bypass EMA + caps for patience)

In `persist_with_event`, after the row is locked and time-decayed and the pre-delta
baseline snapshotted:

1. `apply_deltas(deltas, ema_inertia)` runs **unchanged** — it applies all six axes
   through EMA + the ±0.4 / −0.6 asymmetric caps (patience included, harmlessly, since
   it is overwritten next).
2. When `patience_target.is_some()`, **patience is overwritten** with `patience_target`
   directly — no EMA, no cap (the EMA'd rule application from step 1 is discarded, so
   there is no double count). This is the "全量落地" behaviour: patience lands on the
   LLM read (± rule nudge) this turn. `apply_deltas` itself is **not** modified to skip
   patience.

**Race-safety.** `L` (an absolute LLM read of the snapshot) and `R` (a rule delta) are
both **independent of the current patience value**. `patience_target = L + R` is an
absolute quantity, so setting it needs no read-modify-write on patience and a
concurrent turn cannot make it drift — unlike a delta computed as `L − current`, which
this design deliberately avoids.

`effective_deltas.patience` is still recorded as `new − before` (now potentially a
large single-turn value, e.g. `+0.4` — expected, and surfaced on the debug/BFF event).

### 1.4 Fallback (no LLM patience read this turn)

When `patience_target` is `None` — Proactive, short-user-msg skip, empty-assistant
skip, eval timeout/error, or the model omitting `patience` — patience takes the
**existing** path: add `R` through EMA + clamp. Fully backward-compatible; this is why
**no config flag is needed** (prompt and parser ship together in one version, so there
is no version skew to gate).

Ghost is **not** part of this fallback. `eval_skip_reason` marks Ghost turns `"ghost"`
(no eval call is attempted, mirroring today), but that is moot for patience: Ghost
persists via `record_ghost`, not `persist_with_event`, which discards the computed
`ghost_affinity_deltas()` entirely. So patience is untouched by any delta or EMA on a
Ghost turn — only `ghost_streak` / `total_ghosts` / `last_ghost_at` move.

### 1.5 Data threading (keeps core untouched)

The absolute read travels as a **separate `Option<f64>`**, *not* inside
`AffinityDeltas.patience`:

- `AffinityDeltas.patience` continues to carry the **rule delta** (`llm_deltas.patience`
  stays `0`), so `merge_deltas` semantics are unchanged and `Affinity::apply_deltas`
  needs **no change**.
- `parse_affinity_eval → (AffinityDeltas, Option<f64> /*patience_abs*/, String)`.
- `evaluate_affinity` returns `patience_abs`.
- `fut_affinity` computes `patience_target = patience_abs.map(|l| clamp(l + R, 0, 1))`
  and passes it to `persist_affinity → persist_with_event(..., patience_target)`.
- `persist_with_event` applies step (2) of §1.3 only when `patience_target.is_some()`.

---

## 2. Prompt change

`affinity_eval_prompt` (`prompt.rs`) — a hardcoded Rust `format!`, so it ships with the
parser change (no config drift):

- Remove "patience … 由规则维护，请勿评估" from the axis list.
- Reframe patience as an **absolute** read with drivers, e.g.: *patience 耐心请给一个
  【绝对值】(0~1，每 0.1 一档)，代表你现在对这个用户还有多少耐心 / 愿意继续搭理的程度
  —— 不是变化量。用户投入、认真、有来有回、被尊重 → 高；敷衍、重复、命令式、越界、
  晾着不理、粗鲁 → 低。其它五个维度仍然是【变化量】。*
- JSON schema adds `patience` (absolute) alongside the five deltas:
  `{"warmth":0.0,"trust":0.0,"intrigue":0.0,"intimacy":0.0,"patience":0.5,"tension":0.0,"reason":"…"}`.

The prompt still shows all six **current** values for context (unchanged).

---

## 3. Unchanged invariants

- patience stays **out** of the Bond/Chemistry lines and their labels.
- Time-decay recovery `+0.005/day` **stays** (idle / proactive-only turns).
- Hard ghost protections and the ghost-score formula are **unchanged** — only the
  *value* of patience moves faster, which (by intent) makes the deterministic ghost
  path and the attitude directive more responsive. The user has accepted the increased
  ghost jitter this implies.
- Non-eval turns keep today's rule-delta-through-EMA behaviour.

---

## 4. Blast radius

| File | Change |
| --- | --- |
| `eros-engine-server/src/prompt.rs` | `affinity_eval_prompt`: patience → absolute + drivers; schema adds `patience`. |
| `eros-engine-server/src/pipeline/post_process.rs` | `LlmAffinityEval.patience: Option<f64>`; `parse_affinity_eval` snaps+clamps, returns `Option<f64>`; `evaluate_affinity` passthrough; `persist_affinity`/`fut_affinity` compute + thread `patience_target`. |
| `eros-engine-store/src/affinity.rs` | `persist_with_event(..., patience_target: Option<f64>)`; after `apply_deltas`, `set` patience when `Some`. |
| `docs/affinity-model.md` + `.zh.md` | Rewrite the "patience is rule-owned / not evaluated" sections; document absolute-read + rule-delta + direct write. |

**Reversed / updated tests**
- `post_process.rs::parse_affinity_eval_ignores_patience_field` → now patience **is**
  read as an absolute (snapped); rewrite.
- `post_process.rs::merge_deltas_..._patience_from_rule_only` → still valid (merge is
  unchanged; llm patience stays 0); keep, possibly rename for clarity.
- `prompt.rs` "patience is rule-owned and must not be in the JSON output schema" →
  invert: assert patience **is** in the schema and rendered as an absolute.
- New tests: snap-to-0.1 + clamp; `patience_target = clamp(L+R)`; `None` → rule-only
  fallback; `persist_with_event` sets patience directly (a store-level `sqlx::test`
  asserting a big single-turn jump lands unclamped-by-EMA and rule delta still nudges).

No migration. No new config key.

---

## 5. Open question (resolved)

- **Final-value snapping** — resolved: **do not** re-snap `L + R`; snap the LLM read
  `L` only. (Snapping the final would erase the ±0.02 rule nudge.)

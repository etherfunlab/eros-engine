# Affinity model

[English](affinity-model.md) · [中文](affinity-model.zh.md)

Affinity is a six-dimensional vector that changes on every text-channel,
non-`product_qa` chat turn and folds into two derived lines — **Bond**
(friendship axis) and **Chemistry** (romance axis). Voice-channel and
`product_qa` turns never write an affinity event. Each line has tiers and
labels. The engine is the single source of truth for scores, labels, and
per-turn label transitions.

## The six base axes

| Axis | Range | Default seed | What it shapes |
|------|-------|--------------|----------------|
| `warmth` | −1.0 ↔ 1.0 | `0.1` | Tone, address. Negative = guarded/hostile; positive = warm/affectionate. Shared into both Bond and Chemistry (floored at 0 when folding). |
| `trust` | 0.0 ↔ 1.0 | `0.0` | Topic depth, willingness to disclose self. Bond axis. |
| `intrigue` | 0.0 ↔ 1.0 | `0.0` | Curiosity, follow-up questions, anti-ghost driver. Bond axis. |
| `intimacy` | 0.0 ↔ 1.0 | `0.0` | Inside jokes, nicknames, callbacks to earlier details. Chemistry axis. |
| `patience` | 0.0 ↔ 1.0 | `0.5` | Tolerance for short / low-effort messages; ghost-threshold input. When the LLM has an absolute read for the turn (0–1, 0.1 steps), it is combined with a rule delta and written directly; otherwise the rule delta alone applies 1:1 (see below). Always `[0,1]`-clamped. Excluded from both lines. |
| `tension` | 0.0 ↔ 1.0 | `0.0` | Push-pull, playful friction, tsundere affordance. Chemistry axis. |

`warmth` is the only axis that can go negative. The other five are bounded to
`[0, 1]`. All six axes are clamped on every update.

The **default seed** values above apply only to new rows (sessions that start
after migration `0029`). Existing rows are unaffected.

### Graded writes

The evaluator reports per-axis *grades* rather than numeric deltas; the engine
converts them to scores, damps and gates them (see
[Write pipeline](#write-pipeline-affinity-30)), and applies the committed
delta 1:1:

```
new_value = clamp(old_value + committed_delta)
```

A committed delta means exactly what it says — damping is the pipeline's tier
decay, applied before the write rather than to it. Sessions opened with
`metadata.is_demo` multiply positive judge scores by `AFFINITY_DEMO_BOOST`
(default `1.4`) so demo meters move visibly within a short demo.

### Time decay

Three axes drift with real time when there is no activity. Decay is computed
lazily on each load from `updated_at`:

```
days_elapsed = (now − updated_at) / 1 day

intrigue = clamp(intrigue − 0.01  × days_elapsed, 0.0, 1.0)
patience = clamp(patience + 0.005 × days_elapsed, 0.0, 1.0)
tension  = clamp(tension  − 0.005 × days_elapsed, 0.0, 1.0)
```

`warmth`, `trust`, and `intimacy` do not decay — they are "deep" dimensions.

### Patience: LLM absolute read + rule delta

`patience` is not a graded axis. Each turn's `affinity_evaluation` call (the
same LLM call that grades the other five axes — no new round-trip) also emits
an **absolute** `patience` read (`0.0`–`1.0`, in `0.1` steps, representing
how much patience remains for this user right now, not a change). The engine rounds
the model's read to the nearest `0.1` and clamps to `[0, 1]` — call this `L`.

The PDE still computes the reply/proactive-turn rule delta `R` as before
(`predict_reply_deltas`: long user message `+0.02` / very short `−0.02` / stale gap
>24h `−0.05`) — unchanged.

The turn's target is `patience_target = clamp(L + R, 0, 1)`; the sum is **not**
re-rounded to the `0.1` grid (the grid constrains the LLM read only, so `R` can nudge
the result off-grid). On persist, the write pipeline runs first as usual —
but `patience` passes through it untouched: its rule delta is never decayed,
penalised, or threshold-gated, it applies 1:1 and clamps. Patience is then
**overwritten directly** with `patience_target` (still `[0,1]`-clamped).
Because both `L` and `R` are independent of the currently stored value, this
write is race-safe with no read-modify-write needed.

**Fallback:** when there is no LLM patience read this turn — Proactive, a short user
message, an empty assistant reply, `no_persona_or_affinity` (persona load fails or no
affinity row exists), or the eval call erroring, timing out, or the model omitting the
`patience` field — `patience_target` is `None` and the rule delta `R` alone is
applied (1:1, clamped).

**Ghost is a separate path, not a fallback.** A Ghost turn never reaches
`persist_with_event` — `persist_affinity` dispatches it to `record_ghost` instead,
which takes no grades or deltas, never runs the write pipeline, and only bumps
`ghost_streak` / `total_ghosts` / `last_ghost_at` (it writes an all-zero
`effective_deltas`). The PDE's `ghost_affinity_deltas()` (patience `−0.05`,
tension `+0.05` — a function separate from `predict_reply_deltas`) is computed onto
the `ActionPlan` but discarded at persist time. So on a Ghost turn `patience` does
not move at all — only the ghost counters change.

## The two derived lines

The six axes produce two composite scores. `warm_pos` is `warmth.max(0.0)` —
floored at zero, not shifted, so a neutral or cold session contributes nothing:

```
bond      = (warm_pos + trust   + intrigue) / 3    ∈ [0, 1]
chemistry = (warm_pos + intimacy + tension)  / 3    ∈ [0, 1]
```

`warmth` feeds both lines: cold replies reduce both Bond and Chemistry.
`patience` is excluded from both — it is maintained by an LLM absolute read + rule
delta and written directly; both lines still omit patience (by design).

With the default seed (`warmth 0.1`, `trust/intrigue/tension 0`), a fresh
session starts at bond ≈ chemistry ≈ 0.033 — both in tier 1 (stranger).

> **Naming note:** `AffinityScope::bond()/chemistry()` (used for
> prompt-injection scoping, `length_score`) use a *different* axis grouping —
> that is an older, separate split that is intentionally left alone to avoid
> reply-length regressions. The `bond_score`/`chemistry_score` derived here are
> independent.

## Tiers

Each line has **five tiers** with widening score gaps (each step costs more)
until a narrow apex tier 5:

| Tier | Score range | Gap |
|------|-----------|-----|
| 1 | `[0.00, 0.15)` | 0.15 |
| 2 | `[0.15, 0.35)` | 0.20 |
| 3 | `[0.35, 0.62)` | 0.27 |
| 4 | `[0.62, 0.90)` | 0.28 |
| 5 | `[0.90, 1.00]` | 0.10 |

The API reports each line's score as-is: `AffinitySnapshot.bond` /
`.chemistry` are the real stored composites, 0..1, with no display curve
(affinity 3.0 deleted the old tier-band bar projection). The easy-early /
grind-at-the-top pacing the projection used to fake at render time is now
real: the write-side tier decay (see
[Write pipeline](#write-pipeline-affinity-30)) damps positive gains by the
line's own tier, so higher tiers genuinely take more turns to cross. A
frontend that wants a per-tier progress bar derives it from the score and the
tier bounds above.

All tier thresholds are tunable constants.

## Tiered labels

There are two independent sets of five labels, one per line (serialized
snake_case):

| Line | Tier 1 | Tier 2 | Tier 3 | Tier 4 | Tier 5 |
|------|--------|--------|--------|--------|--------|
| **Bond** | `acquaintance` | `friend` | `close_friend` | `confidant` | `soulmate` |
| **Chemistry** | `spark` | `flirtation` | `crush` | `lover` | `beloved` |

`bond_label` and `chemistry_label` are always one of their respective five
values — they never emit `stranger`. The `stranger` state is conveyed only by
the legacy field (see below).

## Legacy `relationship_label`

The legacy field keeps its old name set for backward compatibility with
existing consumers. It is now a pure function of the two raw scores (replacing
the old ad-hoc `infer_label` heuristic):

```
legacy_relationship_label(bond, chemistry):
  if tier(bond) == 1 AND tier(chemistry) == 1  →  stranger
  let higher = (chemistry > bond) ? Chemistry : Bond   // tie → Bond
  match higher:
    Bond                                         →  friend
    Chemistry if tier(chemistry) in {1, 2}       →  slow_burn
    Chemistry if tier(chemistry) in {3, 4, 5}    →  romantic
```

`frenemy` is retired from emission but remains parseable in the enum for
historical rows. `stranger` is now the explicit "both tier 1" case — it no
longer requires all five old threshold conditions to miss.

## Evaluator protocol: grades, not numbers

**Grades (affinity 3.0).** The evaluator never outputs numeric deltas. For
each of the five graded axes (`warmth` / `trust` / `intrigue` / `intimacy` /
`tension`) it reports an integer **grade** `0`–`4` plus a **direction**:

```json
{
  "warmth":   {"grade": 0, "direction": "up"},
  "trust":    {"grade": 1, "direction": "up"},
  "intrigue": {"grade": 0, "direction": "up"},
  "intimacy": {"grade": 0, "direction": "up"},
  "tension":  {"grade": 2, "direction": "down"},
  "patience": 0.5,
  "reason": "…"
}
```

- **Grade rubric:** `0` = nothing happened (chitchat, acknowledgements — the
  overwhelmingly common verdict); `1` = a small but real movement; `2` = a
  clear push or a clear hurt; `3` = a rare significant moment (genuine
  self-disclosure, vulnerability, flirtation that lands; an overt offense or
  being ignored); `4` = a milestone — the turn that redefines the
  relationship (extremely rare).
- **Direction** is `"up"` / `"down"`; negative moments (coldness,
  perfunctory/repetitive replies, boredom, boundary-crossing, conflict, being
  ignored) are prompted to fire readily.
- `patience` stays an absolute 0–1 read in 0.1 steps (see above), not a grade.

The engine folds `{grade, direction}` into a signed integer `−4..+4`. Models
are reliable ordinal raters and unreliable calibrated arithmetic — so the
judge picks buckets and the engine owns every number.

**Malformed verdicts reject wholesale.** Unparseable JSON, or any malformed
axis — a non-integer or out-of-range grade, an unknown direction — rejects
the whole verdict in `parse_affinity_eval`: all-zero grades, no patience
read, empty reason. The turn's rule deltas still persist, so an evaluator
failure never loses the affinity event. (An omitted axis or `null` grade is
not malformed — it reads as grade 0; a quoted integer like `"grade": "2"` is
salvaged.)

**Single-turn envelope.** With default tuning, grade `+4` converts to `+0.20`
and grade `−4` to `−0.30` per axis. The asymmetry — a bad turn costs more than
a good turn gains — is `AFFINITY_NEG_FACTOR`.

**Banded input.** The per-turn payload shows the evaluator its six current
axis reads as coarse bands (冷/低/中/高, cut at 0.35 / 0.65 like the patience
bands; 冷 = negative warmth), never raw floats — the judge that reports
buckets is not shown the numbers, which would re-anchor it on the arithmetic
the graded protocol removed.

**Register and `reason` hygiene.** The evaluator prompt is written in the
character's own first-person voice ("you are this character; how did this turn
change how you feel about him?"), not as a third-person analytical judge, and
is sent as a static `system` instruction plus a per-turn `user` payload. Its
`reason` rules forbid system vocabulary (AI/assistant/model, refusal, policy)
and forbid endorsing a canned refusal that reached the reply. This is
load-bearing: `reason` is persisted to `companion_affinity_events.context` and
re-injected into later system prompts as `[emotional_context]`, so an
evaluator that rationalises a refusal writes that stance into persona state.
The prompt is engine-owned and deliberately not configurable — see
`docs/superpowers/specs/2026-08-02-affinity-eval-hygiene-design.md`.

## Write pipeline (affinity 3.0)

The judge reports grades; the engine owns every number. Each verdict runs
through four engine-side stages (`grade_turn` in
`eros-engine-core/src/affinity.rs`), computed against the pre-turn snapshot
and applied under the affinity row lock:

```
grade → raw score → tier decay → cross-line penalty → threshold gate → clamp
```

**1. Conversion.** A signed grade `g` converts to a raw score `r`:

```
r = g × AFFINITY_GRADE_UNIT                        (positive; × AFFINITY_DEMO_BOOST on demo sessions)
r = g × AFFINITY_GRADE_UNIT × AFFINITY_NEG_FACTOR  (negative)
```

Defaults `0.05` / `1.5`: grade `+4` = `+0.20`, grade `−4` = `−0.30` — the 2.0
envelope. The PDE's rule nudges (e.g. intrigue `+0.02` on a long user
message) join the raw score pre-decay.

**2. Tier decay (positive only).** Positive raw is multiplied by the own
line's tier factor, `AFFINITY_TIER_DECAY` (default `1.0, 0.70, 0.45, 0.25,
0.10` for tiers 1–5). `trust`/`intrigue` read Bond's tier;
`intimacy`/`tension` read Chemistry's; `warmth` (shared into both lines)
reads the further line's tier (max of both). Negative raw is **never**
decayed — losses stay full price at any tier. This is where the easy-early /
grind-at-the-top pacing lives now that the read-side bar projection is gone.

**3. Cross-line penalty.** When the judge touched an axis exclusive to one
line (grade ≠ 0), the *other* line's height taxes the move:

```
penalty = κ × ((y − y₀)⁺ / (1 − y₀))²
  y  = the OTHER line's score
  κ  = AFFINITY_CROSS_PENALTY        (default 0.05)
  y₀ = AFFINITY_CROSS_PENALTY_START  (default 0.35)
```

High Bond makes Chemistry harder to grow and vice versa: a small positive
push can net negative (the friend-zone wall), and a negative push loses even
more. `warmth` is exempt (it feeds both lines), and rule-only turns (judge
grade 0) are exempt — the pipeline charges events, not rent.

**4. Threshold gate.** Each axis keeps a signed accumulator. The turn's real
score joins it, and the whole balance commits only once
`|accumulated| ≥ AFFINITY_DELTA_THRESHOLD` (default `0` = commit every turn);
below the threshold it buffers in `companion_affinity.pending_deltas` (JSONB,
migration `0043`) and the axis does not move this turn.

Committed deltas then apply 1:1 and clamp to each axis's range (`[-1,1]` for
`warmth`, `[0,1]` for the rest). `patience` bypasses all four stages — its
rule delta passes through untouched and the absolute overwrite happens after
(see above).

### Tuning knobs

Server-side env vars, each falling back per-knob to a default; the defaults
reproduce the 2.0 effective single-turn envelope:

| Env var | Default | Meaning |
|---------|---------|---------|
| `AFFINITY_GRADE_UNIT` | `0.05` | Raw score per grade step |
| `AFFINITY_NEG_FACTOR` | `1.5` | Extra multiplier on negative raw — keeps "slow up, fast down" |
| `AFFINITY_TIER_DECAY` | `1.0,0.70,0.45,0.25,0.10` | Positive-delta damping per tier 1–5 (comma-separated; anything but exactly 5 finite non-negative values keeps the whole default table — factors above 1 amplify and are allowed) |
| `AFFINITY_CROSS_PENALTY` | `0.05` | Cross-line penalty ceiling κ |
| `AFFINITY_CROSS_PENALTY_START` | `0.35` | Counterpart score where the penalty ramp starts (y₀) |
| `AFFINITY_DELTA_THRESHOLD` | `0.0` | Commit threshold θ; `0` commits every turn |
| `AFFINITY_DEMO_BOOST` | `1.4` | Multiplier on the judge's positive raw (rule nudges unaffected) for `metadata.is_demo` sessions |

Every scalar is domain-checked at boot — non-finite or out-of-domain values
(negative unit/factor/penalty/threshold/boost, a penalty start outside
`[0, 1)`) keep the default and log a warning, so an env typo degrades to
defaults instead of reaching the pipeline.

## Persistence

### Generated columns

Migration `0029` adds `bond` and `chemistry` as Postgres `GENERATED ALWAYS …
STORED` columns on `engine.companion_affinity`. The DB recomputes them from the
six axes on every row insert or update, so they cannot drift. Existing rows
auto-populate at migration time (no backfill, no engine write code):

```sql
bond      GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (GREATEST(warmth,0) + trust    + intrigue) / 3))) STORED
chemistry GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (GREATEST(warmth,0) + intimacy + tension)  / 3))) STORED
```

Tier labels live only in the core read layer; the API returns the stored
composite itself — there is no separate display value.

### Lowered default seed

The new-row column defaults (also migration `0029`) are set so a fresh session
starts at bond ≈ chemistry ≈ 0.033 — tier 1 on both lines, legacy `stranger`.
Existing rows are unaffected.

### Pending deltas (threshold gate)

Migration `0043` adds `pending_deltas JSONB` on `engine.companion_affinity` —
the per-axis balance the threshold gate is still holding back. Written by the
graded message path only; `NULL` (every pre-3.0 row, and rows never gated)
reads as all-zero.

### Event rows

Each delta turn appends one row to `engine.companion_affinity_events`:

- `deltas` — the turn's **raw scores**: grade conversion (demo boost
  included) plus rule nudges, pre-decay.
- `effective_deltas` — the **applied** per-axis change, `after − before`. It
  captures tier decay, the cross-line penalty, gating, the patience
  overwrite, and clamping; all-zero on a turn the gate buffered.
- `context` — `affinity_reason` (the evaluator's `reason`),
  `eval_skip_reason` when no eval ran, the judge's verbatim signed `grades`,
  and the gate's `pending_after` balance.

### Per-turn label changes

Migration `0029` also adds `label_changes JSONB` on
`engine.companion_affinity_events`. After each turn, the engine compares tiers
before and after the delta, scoped to the same decay window as
`effective_deltas`:

```
label_changes = {
  bond:      { from: "<tier_key>", to: "<tier_key>" }  // if bond tier changed
  chemistry: { from: "<tier_key>", to: "<tier_key>" }  // if chemistry tier changed
}
// NULL when neither tier moved this turn
```

`from`/`to` are tier keys (e.g. `"acquaintance"`, `"friend"`). The legacy
`relationship_label` transition is omitted because it is derivable. Decay-only
tier drift is not recorded as a discrete event; the absolute snapshot remains
available.

## API surfaces

### `AffinitySnapshot`

Returned by `GET /comp/affinity/{session_id}` (debug, gated by
`EXPOSE_AFFINITY_DEBUG`). The snapshot includes:

```json
{
  "warmth": 0.42,
  "trust": 0.08,
  "intrigue": 0.12,
  "intimacy": 0.05,
  "patience": 0.55,
  "tension": 0.04,
  "bond": 0.21,
  "chemistry": 0.17,
  "bond_label": "friend",
  "chemistry_label": "flirtation",
  "ghost_streak": 0,
  "total_ghosts": 0,
  "relationship_label": "friend",
  "updated_at": "2026-06-30T12:00:00.000000Z"
}
```

- `bond` / `chemistry` — the real stored composite scores (0–1); no display
  curve is applied.
- `bond_label` / `chemistry_label` — one of the 10 tier keys above.
- `relationship_label` — legacy mapped value (`stranger / friend / slow_burn / romantic`).

### BFF `/bff/v1/comp/affinity/{session_id}/event`

This endpoint returns the per-turn affinity delta and is not gated by
`EXPOSE_AFFINITY_DEBUG`. In addition to the existing `effective_deltas`
(per-axis applied change, `after − before`), the event now carries:

```json
{
  "session_id": "…",
  "event": {
    "event_id": "…",
    "event_type": "message",
    "effective_deltas": {
      "warmth": 0.06, "trust": 0.02, "intrigue": 0.0,
      "intimacy": 0.0, "patience": 0.0, "tension": -0.02
    },
    "effective_deltas_computed": {
      "bond": 0.027,
      "chemistry": 0.013
    },
    "label_changes": {
      "bond": { "from": "acquaintance", "to": "friend" }
    },
    "created_at": "…"
  }
}
```

- `effective_deltas_computed` — the exact per-turn bond/chemistry delta,
  computed at persist time from the floored before/after scores and stored on
  the event row (`companion_affinity_events.effective_line_deltas`). Values
  are composite-score units — the same 0..1 scale as the snapshot's
  `bond`/`chemistry` — suitable for a per-turn "+X bond / +Y chemistry"
  pulse. `null` / absent on pre-migration rows.
- `label_changes` — engine-authoritative tier transition for this turn; `null`
  (or absent) when no tier moved. The frontend stops computing transitions
  itself.

Both fields are also mirrored on debug
`GET /comp/affinity/{session_id}/event` entries.

## Source

- `crates/eros-engine-core/src/affinity.rs` — types, `grade_turn` write pipeline, time decay, bond/chemistry scores, tiers, labels, diff_labels
- `crates/eros-engine-store/src/affinity.rs` — `AffinityRepo` (persist_with_event, record_ghost), migrations 0029/0043
- `crates/eros-engine-server/src/pipeline/post_process.rs` — LLM evaluation, grade parsing
- `crates/eros-engine-server/src/prompt.rs` — affinity → attitude directive + eval prompt
- `crates/eros-engine-server/src/routes/dto.rs` — `AffinitySnapshot` (composite scores + labels)
- `crates/eros-engine-server/src/routes/bff/affinity.rs` — BFF event surface
- `crates/eros-engine-server/src/routes/debug.rs` — debug event log

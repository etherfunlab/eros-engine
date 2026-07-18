# Affinity model

[English](affinity-model.md) · [中文](affinity-model.zh.md)

Affinity is a six-dimensional vector that changes on every chat turn and folds
into two derived lines — **Bond** (friendship axis) and **Chemistry** (romance
axis). Each line has tiers and labels. The engine is the single source of truth
for scores, labels, and per-turn label transitions.

## The six base axes

| Axis | Range | Default seed | What it shapes |
|------|-------|--------------|----------------|
| `warmth` | −1.0 ↔ 1.0 | `0.1` | Tone, address. Negative = guarded/hostile; positive = warm/affectionate. Shared into both Bond and Chemistry (floored at 0 when folding). |
| `trust` | 0.0 ↔ 1.0 | `0.0` | Topic depth, willingness to disclose self. Bond axis. |
| `intrigue` | 0.0 ↔ 1.0 | `0.0` | Curiosity, follow-up questions, anti-ghost driver. Bond axis. |
| `intimacy` | 0.0 ↔ 1.0 | `0.0` | Inside jokes, nicknames, callbacks to earlier details. Chemistry axis. |
| `patience` | 0.0 ↔ 1.0 | `0.5` | Tolerance for short / low-effort messages; ghost-threshold input. The LLM gives an absolute read each turn (0–1, 0.1 steps) + a rule delta, written directly (bypasses EMA; still `[0,1]`-clamped). Excluded from both lines. |
| `tension` | 0.0 ↔ 1.0 | `0.0` | Push-pull, playful friction, tsundere affordance. Chemistry axis. |

`warmth` is the only axis that can go negative. The other five are bounded to
`[0, 1]`. All six axes are clamped on every update.

The **default seed** values above apply only to new rows (sessions that start
after migration `0029`). Existing rows are unaffected.

### EMA smoothing

LLM-evaluated deltas are applied through an exponential moving average to
prevent lurches:

```
new_value = clamp(old_value + (1 − ema_inertia) × delta)
```

Default `ema_inertia = 0.5` (configurable via `EMA_INERTIA`). At that default,
a delta of `+0.5` moves the axis by `+0.25` for the turn.

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

`patience` is no longer a purely rule-owned axis. Each turn's `affinity_evaluation`
call (the same LLM call that produces the other five axes — no new round-trip) now
also emits an **absolute** `patience` read (`0.0`–`1.0`, in `0.1` steps, representing
how much patience remains for this user right now, not a change). The engine rounds
the model's read to the nearest `0.1` and clamps to `[0, 1]` — call this `L`.

The PDE still computes the reply/proactive-turn rule delta `R` as before
(`predict_reply_deltas`: long user message `+0.02` / very short `−0.02` / stale gap
>24h `−0.05`) — unchanged.

The turn's target is `patience_target = clamp(L + R, 0, 1)`; the sum is **not**
re-rounded to the `0.1` grid (the grid constrains the LLM read only, so `R` can nudge
the result off-grid). The ±0.4/−0.6 asymmetric caps are applied earlier, in
`parse_affinity_eval`, to the five LLM delta axes only — patience is never subject to
them. On persist, `apply_deltas` still runs first as usual (all six axes go through
EMA smoothing + the `[0,1]` clamp), and patience is then **overwritten directly** with
`patience_target` — bypassing EMA smoothing (still `[0,1]`-clamped). Because both `L`
and `R` are independent of the currently stored value, this write is race-safe with no
read-modify-write needed.

**Fallback:** when there is no LLM patience read this turn — Proactive, a short user
message, an empty assistant reply, `no_persona_or_affinity` (persona load fails or no
affinity row exists), or the eval call erroring, timing out, or the model omitting the
`patience` field — `patience_target` is `None` and patience takes the **old** path: `R`
is added through EMA and clamped.

**Ghost is a separate path, not a fallback.** A Ghost turn never reaches
`persist_with_event` — `persist_affinity` dispatches it to `record_ghost` instead,
which takes no deltas, never runs `apply_deltas`/EMA, and only bumps `ghost_streak` /
`total_ghosts` / `last_ghost_at` (it writes an all-zero `effective_deltas`). The
PDE's `ghost_affinity_deltas()` (patience `−0.05`, tension `+0.05` — a function
separate from `predict_reply_deltas`) is computed onto the `ActionPlan` but discarded
at persist time. So on a Ghost turn `patience` is moved by neither a delta nor EMA —
only the ghost counters change.

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

## Tiers and bar curve

Each line has **five tiers** with widening raw-score gaps (each step costs more)
until a narrow apex tier 5:

| Tier | Raw range | Gap |
|------|-----------|-----|
| 1 | `[0.00, 0.15)` | 0.15 |
| 2 | `[0.15, 0.35)` | 0.20 |
| 3 | `[0.35, 0.62)` | 0.27 |
| 4 | `[0.62, 0.90)` | 0.28 |
| 5 | `[0.90, 1.00]` | 0.10 |

**Bar value (0–1, rendered by the frontend):** tiers 1–4 fill 25% / 25% / 25% /
20% of the bar and tier 5 fills the top 5%, linear within each band:

```
bar(raw) = band_lo(tier) + (raw − tier_lo) / (tier_hi − tier_lo) × band_width(tier)
  Tier 1: 0.00 + (raw − 0.00) / 0.15 × 0.25  →  [0.00, 0.25)
  Tier 2: 0.25 + (raw − 0.15) / 0.20 × 0.25  →  [0.25, 0.50)
  Tier 3: 0.50 + (raw − 0.35) / 0.27 × 0.25  →  [0.50, 0.75)
  Tier 4: 0.75 + (raw − 0.62) / 0.28 × 0.20  →  [0.75, 0.95)
  Tier 5: 0.95 + (raw − 0.90) / 0.10 × 0.05  →  [0.95, 1.00]
clamped to [0, 1]
```

Because higher tiers span more raw affinity, the bar fills quickly at first and
slows near 100% — easy first two tiers, grind at the top — without a literal
`exp()`. A fixed per-turn raw delta also moves the bar *less* in higher tiers.
Tier 5 is a deliberately narrow 5% apex band, making the ceiling read as rare
while still leaving enough room for the bar to move across its 0.10 raw span
(no lv4→lv5 damping).

All thresholds and bands are tunable constants.

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

## Eval distribution and asymmetric cap

**Cap (asymmetric).** The evaluator's raw per-axis output is clamped
asymmetrically in `parse_affinity_eval`:

```
POS_CAP = +0.4    NEG_CAP = −0.6
effective_delta = raw.clamp(NEG_CAP, POS_CAP)
```

With EMA blend 0.5 (`ema_inertia = 0.5`), the per-turn axis maxima are
**+0.2** (gain) and **−0.3** (loss) — the asymmetric cap lets a bad turn cost
more than a good turn gains.

**Distribution (prompt-shaped).** The evaluator is guided toward this output
shape:

- **Most turns: exactly `0`** — ordinary chitchat and acknowledgements score
  nothing.
- **Rare positive** — only on genuine relationship-advancing moments (real
  warmth, self-disclosure, vulnerability, flirtation that lands); may be large
  (up to ≈ +0.4 per axis).
- **Readier negative** — fires for coldness, perfunctory/repetitive replies,
  boredom, boundary-crossing, conflict, or being ignored; may be larger
  (down to ≈ −0.6 per axis).

EMA smoothing and time decay are **unchanged** — only the cap and prompt
guidance changed.

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

The bar curve and tier labels live only in the core read layer. The raw
composite stored in the DB and the API-level bar value are distinct.

### Lowered default seed

The new-row column defaults (also migration `0029`) are set so a fresh session
starts at bond ≈ chemistry ≈ 0.033 — tier 1 on both lines, legacy `stranger`.
Existing rows are unaffected.

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
  "bond": 0.32,
  "chemistry": 0.28,
  "bond_label": "friend",
  "chemistry_label": "flirtation",
  "ghost_streak": 0,
  "total_ghosts": 0,
  "relationship_label": "friend",
  "updated_at": "2026-06-30T12:00:00.000000Z"
}
```

- `bond` / `chemistry` — bar values (0–1, curve-applied), not raw composites.
- `bond_label` / `chemistry_label` — one of the 10 tier keys above.
- `relationship_label` — legacy mapped value (`stranger / friend / slow_burn / romantic`).

### BFF `/bff/v1/comp/affinity/{session_id}/event`

This endpoint returns the per-turn affinity delta and is not gated by
`EXPOSE_AFFINITY_DEBUG`. In addition to the existing `effective_deltas`
(per-axis, post-EMA), the event now carries:

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
  the event row (`companion_affinity_events.effective_line_deltas`). Values are
  raw-composite units (not bar-percent), suitable for a per-turn "+X bond / +Y
  chemistry" pulse.
  `null` / absent on pre-migration rows.
- `label_changes` — engine-authoritative tier transition for this turn; `null`
  (or absent) when no tier moved. The frontend stops computing transitions
  itself.

Both fields are also mirrored on debug
`GET /comp/affinity/{session_id}/event` entries.

## Source

- `crates/eros-engine-core/src/affinity.rs` — types, EMA, time decay, bond/chemistry scores, tiers, bar, labels, diff_labels
- `crates/eros-engine-store/src/affinity.rs` — `AffinityRepo` (persist_with_event, record_ghost), migration 0029
- `crates/eros-engine-server/src/pipeline/post_process.rs` — LLM evaluation, asymmetric clamp
- `crates/eros-engine-server/src/prompt.rs` — affinity → attitude directive + eval prompt
- `crates/eros-engine-server/src/routes/dto.rs` — `AffinitySnapshot` (bar + labels)
- `crates/eros-engine-server/src/routes/bff/affinity.rs` — BFF event surface
- `crates/eros-engine-server/src/routes/debug.rs` — debug event log

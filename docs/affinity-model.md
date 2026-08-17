# Affinity model

[English](affinity-model.md) · [中文](affinity-model.zh.md)

Affinity is a six-dimensional vector that changes on every text-channel,
non-`product_qa` chat turn. Four **line axes** fold into two derived lines —
**Bond** (friendship) and **Chemistry** (romance) — and the two **endpoint
axes** (`warmth`, `patience`) are *derived quantities*: the judge reports a
coarse absolute level, and the engine folds it into a continuous value using
the counterpart line score. Voice-channel and `product_qa` turns never write
an affinity event. Each line has tiers and labels. The engine is the single
source of truth for scores, labels, and per-turn label transitions.

## The six base axes

| Axis | Range | Default | What it shapes |
|------|-------|---------|----------------|
| `warmth` | 0.0 ↔ 1.0 | ≈ `0.244` (derived) | Tone, address. **Derived endpoint** (4.0): `warmth = max(base(level)·B(chemistry), φ·chemistry) × decay`. |
| `trust` | 0.0 ↔ 1.0 | `0.0` | Topic depth, willingness to disclose self. Bond axis. |
| `intrigue` | 0.0 ↔ 1.0 | `0.0` | Curiosity, follow-up questions, anti-ghost driver. Bond axis. |
| `intimacy` | 0.0 ↔ 1.0 | `0.0` | Inside jokes, nicknames, callbacks to earlier details. Chemistry axis. |
| `patience` | 0.0 ↔ 1.0 | ≈ `0.244` (derived) | Tolerance for short / low-effort messages; ghost-threshold input. **Derived endpoint** (4.0): `patience = max(base(level)·B(bond), φ·bond) × decay`. |
| `tension` | 0.0 ↔ 1.0 | `0.0` | Push-pull, playful friction, tsundere affordance. Chemistry axis. |

All six axes are bounded to `[0, 1]` and clamped on every update. The
authoritative facts per row are the four line axes, the two judge **levels**
(`warmth_grade` / `patience_grade`, `1..=3`, migration `0048`), and
`updated_at`; the stored `warmth`/`patience` values are a materialized cache
of the derivation, refreshed wherever time decay runs.

### Graded writes (line axes)

The evaluator reports per-axis *grades* rather than numeric deltas; the engine
converts them to scores, damps and gates them (see
[Write pipeline](#write-pipeline-affinity-40)), and applies the committed
delta 1:1:

```
new_value = clamp(old_value + committed_delta)
```

A committed delta means exactly what it says — damping is the pipeline's tier
decay, applied before the write rather than to it. Sessions opened with
`metadata.is_demo` multiply positive judge scores by `AFFINITY_DEMO_BOOST`
(default `1.4`) so demo meters move visibly within a short demo.

### Time decay

Two line axes drift with real time when there is no activity. Decay is
computed lazily on each load from `updated_at`:

```
days_elapsed = (now − updated_at) / 1 day

intrigue = clamp(intrigue − 0.01  × days_elapsed, 0.0, 1.0)
tension  = clamp(tension  − 0.005 × days_elapsed, 0.0, 1.0)
```

`trust` and `intimacy` do not decay — they are "deep" dimensions. The old
`patience` upward drift is retired: absence handling for the two endpoints is
the multiplicative decay inside the derivation (below), which **cools** rather
than heals.

## Derived endpoints (affinity 4.0)

`warmth` and `patience` are no longer accumulated state. Each judged turn the
evaluator reports one absolute **level** per endpoint — `1` cold/impatient,
`2` baseline (the overwhelmingly common verdict), `3` clearly warm/invested —
and the engine derives the continuous value:

```
base(level)  = (level − 1) / 3                          ∈ {0, 1/3, 2/3}
B(x)         = 1 + λ·(x − 0.35)                         λ = (1.5−1)/(1−0.35) = 10/13
decay(Δt)    = max(FLOOR, 1 − RATE·days)                Δt since updated_at

warmth   = clamp01( max(base(w_level)·B(chemistry), φ·chemistry) × decay )
patience = clamp01( max(base(p_level)·B(bond),      φ·bond)      × decay )
```

The coupling direction is **amplification, not correlation**: deeper
chemistry warms expression, deeper bond funds patience. Low bond × high
chemistry natively produces the tsundere register (impatient but warm); high
bond × low chemistry the old-friend register (patient but cool) — no prompt
special-casing.

Every constant is anchored, not invented:

- **Pivot `0.35` = tier-2 upper bound** (the same constant): the boost turns
  positive the moment the counterpart line enters tier 3; below it the value
  is damped under its base. `0.35`/`0.65` are also the band cuts used for the
  judge's input and the patience bands.
- **`B(1) = 1.5`** makes `⅔ × 1.5 = 1.0`: a full level times a full
  counterpart line lands exactly at the ceiling.
- **Floor `φ = 0.2`** (`AFFINITY_FLOOR_RATIO`): a level-1 verdict reads
  `φ·counterpart` instead of an absolute zero — a deep relationship going
  cold one turn keeps an ember (`0.18` at counterpart `0.9`), a stranger
  reads ~0. Since `φ·x ≤ 0.2 < 0.244 = ⅓·B(0)`, the floor can only ever act
  on level 1 — it never overrides a non-cold verdict.
- **Decay** (`AFFINITY_TIME_DECAY_RATE` `0.02`/day,
  `AFFINITY_TIME_DECAY_FLOOR` `0.5`): 7 days → ×0.86, 25+ days → ×0.5.
  Absence cools but never zeroes — and an old relationship keeps a floor
  through the boost (bond `0.9` at full decay still yields patience ≈ `0.48`).

Reachable values at `decay = 1`: level 1 → `(0, 0.2]` (continuous in the
counterpart), level 2 → `[0.244, 0.5]`, level 3 → `[0.487, 1.0]`. The level
picks the band; the counterpart line picks the position inside it.

**Per-turn deltas still exist.** `effective_deltas.warmth` / `.patience` are
the derivation's `after − before` across the turn, measured against the
post-decay snapshot — the absence gap is never attributed to the turn.

**Skipped turns hold.** When the eval is skipped or fails
(`eval_skip_reason`), the stored levels hold and the endpoints are simply
re-derived with the current lines and decay. The old rule-delta fallback
(`±0.02` message-length nudges, stale `−0.05`) is retired — the stale rule is
absorbed by decay.

**Ghost is a separate path.** A Ghost turn never reaches
`persist_with_event` — it only bumps `ghost_streak` / `total_ghosts` /
`last_ghost_at`. The PDE's ghost deltas touch `tension` only.

## The two derived lines

The four line axes produce two composite scores. As of 4.0 the lines share
nothing:

```
bond      = (trust    + intrigue) / 2    ∈ [0, 1]
chemistry = (intimacy + tension)  / 2    ∈ [0, 1]
```

`bond` is friendship — trust plus continued interest. `chemistry` is romance —
closeness plus charge. The endpoints are excluded from both lines by
construction (they are the lines' *outputs*).

With the default seed (all line axes `0`), a fresh session starts at
bond = chemistry = 0 — both in tier 1 (stranger) — and both endpoints at the
level-2 damped base ≈ `0.244`.

> **Naming note:** `AffinityScope::bond()/chemistry()` (used for
> prompt-injection scoping, `length_score`) use a *different* axis grouping —
> that is the 1.0-era split, intentionally left alone to avoid reply-length
> regressions. Structurally it grouped `warmth` with `intimacy`/`tension` and
> `patience` with `trust`/`intrigue` — the same families 4.0's coupling makes
> explicit — but its two *names* are crossed relative to the 2.0+ lines,
> which is one reason the scope must never steer the derivation.

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
`.chemistry` are the real stored composites, 0..1, with no display curve.
The easy-early / grind-at-the-top pacing is real: the write-side tier decay
damps positive gains by the line's own tier, so higher tiers genuinely take
more turns to cross. A frontend that wants a per-tier progress bar derives it
from the score and the tier bounds above.

All tier thresholds are tunable constants.

## Tiered labels

There are two independent sets of five labels, one per line (serialized
snake_case):

| Line | Tier 1 | Tier 2 | Tier 3 | Tier 4 | Tier 5 |
|------|--------|--------|--------|--------|--------|
| **Bond** | `acquaintance` | `friend` | `close_friend` | `confidant` | `soulmate` |
| **Chemistry** | `spark` | `flirtation` | `crush` | `lover` | `beloved` |

`bond_label` and `chemistry_label` are always one of their respective five
values. A relationship that has not started anywhere reads as
`acquaintance` + `spark`, both tier 1 — there is no separate "stranger"
state, and as of 4.1 no separate legacy label carrying one.

## Tier numbers are stored

Every write persists `tier_index`'s own result to
`companion_affinity.bond_tier` / `.chem_tier`, so a SQL consumer that cannot
call the engine still gets the authoritative tier instead of re-deriving it
from the score against a copied threshold table. Those columns are write-only
from the engine's side: engine code holds the score and calls `bond_tier()`.

The thresholds live in exactly one place (`tier_index`), which is why adding a
tier is a change to that function plus a backfill — the table's shape does not
encode how many tiers exist.

## Evaluator protocol: fully ordinal

**The judge never outputs a continuous number anywhere.** For each of the
four line axes (`trust` / `intrigue` / `intimacy` / `tension`) it reports an
integer **grade** `0`–`4` plus a **direction**; for each endpoint it reports
an absolute **level** `1`–`3`:

```json
{
  "warmth":   2,
  "trust":    {"grade": 1, "direction": "up"},
  "intrigue": {"grade": 0, "direction": "up"},
  "intimacy": {"grade": 0, "direction": "up"},
  "tension":  {"grade": 2, "direction": "down"},
  "patience": 2,
  "reason": "…"
}
```

- **Grade rubric:** `0` = nothing happened (chitchat, acknowledgements — the
  overwhelmingly common verdict); `1` = a small but real movement; `2` = a
  clear push or a clear hurt; `3` = a rare significant moment; `4` = a
  milestone (extremely rare).
- **Direction** is `"up"` / `"down"`; negative moments are prompted to fire
  readily.
- **Level rubric:** `1` = cold / impatient (visibly frosty, dismissive,
  offended); `2` = baseline — the overwhelmingly common verdict; `3` =
  clearly warm / invested. The level is a *state read for this turn*, not a
  delta.

Models are reliable ordinal raters and unreliable calibrated arithmetic — so
the judge picks buckets and the engine owns every number. The continuous
`warmth`/`patience` distribution users see is folded out of the discrete
levels by the derivation above; 4.0 removed the one remaining continuous
output (the old 0.1-step patience read, which ceiling-packed in production).

**Malformed verdicts reject wholesale.** Unparseable JSON, any malformed
axis — a non-integer or out-of-range grade, an unknown direction — or any
malformed level rejects the whole verdict in `parse_affinity_eval`: all-zero
grades, no level reads, empty reason. The turn's rule deltas still persist,
so an evaluator failure never loses the affinity event. (An omitted axis or
`null` grade reads as grade 0; an omitted or `null` level reads as "hold the
stored level"; quoted integers like `"grade": "2"` or `"warmth": "3"` are
salvaged.)

**Banded input, endpoints excluded.** The per-turn payload shows the
evaluator the four current *line-axis* reads as coarse bands (低/中/高, cut at
0.35 / 0.65), never raw floats. The current `warmth`/`patience` values are
deliberately **not** injected: an absolute level read is valuable precisely
because it is stateless — showing the previous value would anchor the judge
and reproduce the inflation the redesign removes.

**Register and `reason` hygiene.** The evaluator prompt is written in the
character's own first-person voice, not as a third-person analytical judge,
and is sent as a static `system` instruction plus a per-turn `user` payload.
Its `reason` rules forbid system vocabulary (AI/assistant/model, refusal,
policy) and forbid endorsing a canned refusal that reached the reply. This is
load-bearing: `reason` is persisted to `companion_affinity_events.context`
and re-injected into later system prompts as `[emotional_context]`. The
prompt is engine-owned and deliberately not configurable — see
`docs/superpowers/specs/2026-08-02-affinity-eval-hygiene-design.md`.

## Write pipeline (affinity 4.0)

The judge reports grades; the engine owns every number. Each verdict runs
through four engine-side stages (`grade_turn` in
`eros-engine-core/src/affinity.rs`), computed against the pre-turn snapshot
and applied under the affinity row lock. The endpoints never enter this
pipeline.

```
grade → raw score → tier decay → cross-line penalty → threshold gate → clamp
                                                    → endpoint derivation
```

**1. Conversion, per line.** A signed grade `g` converts at its line's unit:

```
r = g × u_line                        (positive; × AFFINITY_DEMO_BOOST on demo sessions)
r = g × u_line × AFFINITY_NEG_FACTOR  (negative)

u_line = AFFINITY_GRADE_UNIT_BOND  (trust / intrigue,   default 0.0786)
       | AFFINITY_GRADE_UNIT_CHEM  (intimacy / tension, default 0.0266)
```

The ~3× spread between the two units is the judge's measured grading
asymmetry (tension reaches grade ≥2 on roughly half of turns while trust is
graded 0 on ~80%), written down where it can be argued with instead of hidden
in a grade remap. The PDE's rule nudges (e.g. intrigue `+0.02` on a long user
message) join the raw score pre-decay.

**2. Tier decay (positive only).** Positive raw is multiplied by the own
line's tier factor, `AFFINITY_TIER_DECAY` (default `1.0, 0.70, 0.45, 0.25,
0.10` for tiers 1–5). `trust`/`intrigue` read Bond's tier;
`intimacy`/`tension` read Chemistry's. Negative raw is **never** decayed —
losses stay full price at any tier.

**3. Cross-line penalty.** The *other* line's height taxes the move — in
proportion to the grade actually applied, with the ceiling defined as a
multiple of the line's own unit:

```
penalty = κ_line × ((y − y₀)⁺ / (1 − y₀))² × (|g| / 4)
  y      = the OTHER line's score
  κ_line = AFFINITY_CROSS_PENALTY_RATIO × u_line   (ratio default 5/6)
  y₀     = AFFINITY_CROSS_PENALTY_START            (default 0.35)
```

High Bond makes Chemistry harder to grow and vice versa; a grade of `0`
charges nothing — the pipeline charges events, not rent. Ignoring rule
nudges, the term factorises:

```
g > 0:  ρ = g·u · (D_k − ratio·φ(y)/4)
g < 0:  ρ = g·u · (λ⁻ + ratio·φ(y)/4)      (the negative part is never decayed)
```

Neither bracket contains `g` **or `u`**: the outcome cannot change sign
between grades at a fixed position, and the break-even position
`φ(y*) = 4·D_k/ratio` is identical for both lines regardless of their units —
tying κ to the unit is what keeps per-line units from silently moving the
double-high wall. At defaults only own tier 5 has a real break-even
(counterpart ≈ `0.761`); past it every grade nets negative, uniformly.

**4. Threshold gate.** Each line axis keeps a signed accumulator. The turn's
real score joins it, and the whole balance commits only once
`|accumulated| ≥ AFFINITY_DELTA_THRESHOLD` (default `0` = commit every turn);
below the threshold it buffers in `companion_affinity.pending_deltas`.

Committed deltas then apply 1:1 and clamp to `[0,1]`. Afterwards the judge's
levels (when read this turn) overwrite the stored levels, and both endpoints
are re-derived from the post-turn lines.

### Tuning knobs

Server-side env vars, each falling back per-knob to a default:

| Env var | Default | Meaning |
|---------|---------|---------|
| `AFFINITY_GRADE_UNIT_BOND` | `0.0786` | Raw score per grade step, trust/intrigue |
| `AFFINITY_GRADE_UNIT_CHEM` | `0.0266` | Raw score per grade step, intimacy/tension |
| `AFFINITY_NEG_FACTOR` | `1.5` | Extra multiplier on negative raw — keeps "slow up, fast down" |
| `AFFINITY_TIER_DECAY` | `1.0,0.70,0.45,0.25,0.10` | Positive-delta damping per tier 1–5 (comma-separated; anything but exactly 5 finite non-negative values keeps the whole default table) |
| `AFFINITY_CROSS_PENALTY_RATIO` | `0.8333` | κ_line = ratio × u_line — the break-even stays unit-invariant |
| `AFFINITY_CROSS_PENALTY_START` | `0.35` | Counterpart score where the penalty ramp starts (y₀) |
| `AFFINITY_DELTA_THRESHOLD` | `0.0` | Commit threshold θ; `0` commits every turn |
| `AFFINITY_DEMO_BOOST` | `1.4` | Multiplier on the judge's positive raw for `metadata.is_demo` sessions |
| `AFFINITY_FLOOR_RATIO` | `0.2` | Endpoint floor φ; domain-capped at `0.24` so it can never override a non-cold verdict |
| `AFFINITY_TIME_DECAY_RATE` | `0.02` | Endpoint absence decay per day |
| `AFFINITY_TIME_DECAY_FLOOR` | `0.5` | Endpoint absence decay floor |

Every scalar is domain-checked at boot — non-finite or out-of-domain values
keep the default and log a warning, so an env typo degrades to defaults
instead of reaching the pipeline.

The endpoint anchors — the `0.35` pivot (= tier-2 upper bound) and
`B_MAX = 1.5` — are **code constants**, not knobs: they are structural
commitments (the exact-ceiling property `⅔ × 1.5 = 1`), and env-tuning them
could break invariants the derivation relies on.

## Scope steering: retired

Affinity 3.1's write-side scope steering (`ScopeMode`, the bond boost and the
chemistry grade ladder) is retired in 4.0. `affinity_scope` is read-side only
again — it gates prompt injection and `length_score`, and touches nothing on
the write path. The endpoint derivation must never read the scope either:
B(x) already transmits every line change (including any pace steering) to the
endpoints, so a derivation layer that also read the scope would land the same
request on the same endpoint twice; and the scope's 1.0-era names are crossed
relative to the 2.0+ lines. `companion_affinity_events.context` stops
carrying `scope_mode` / `effective_grades` — the absence of
`effective_grades` on new rows is the retirement's cleanest verification.

## Persistence

### Generated columns

Migration `0048` redefines `bond` and `chemistry` as Postgres `GENERATED
ALWAYS … STORED` columns on `engine.companion_affinity` (drop + re-add:
Postgres cannot alter a generation expression in place). The DB recomputes
them from the line axes on every row insert or update, so they cannot drift:

```sql
bond      GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (trust    + intrigue) / 2))) STORED
chemistry GENERATED ALWAYS AS (LEAST(1, GREATEST(0, (intimacy + tension)  / 2))) STORED
```

No axis data is migrated: the composites are redefined over what the judge
actually granted. The label churn this causes on live rows was measured and
accepted in the design spec (median two turns to undo).

### Endpoint levels

Migration `0048` also adds `warmth_grade` / `patience_grade` (`SMALLINT NOT
NULL DEFAULT 2`, range-checked `1..=3`) — the authoritative judge levels —
and backfills the `warmth`/`patience` cache columns with the level-2
derivation over each row's lines. New-row defaults put both endpoints at
≈ `0.244` (a stranger starts with limited patience — deliberate).

### Pending deltas (threshold gate)

`pending_deltas JSONB` on `engine.companion_affinity` holds the per-axis
balance the threshold gate is still holding back (line axes only as of 4.0; a
stale `warmth` key from older rows is ignored and drains naturally). `NULL`
reads as all-zero.

### Event rows

Each delta turn appends one row to `engine.companion_affinity_events`:

- `deltas` — the turn's **raw scores** on the line axes (grade conversion
  plus rule nudges, pre-decay); `warmth`/`patience` are always `0.0` here.
- `effective_deltas` — the **applied** per-axis change, `after − before`.
  For the line axes it captures tier decay, the penalty, gating, and
  clamping; for the endpoints it *is* the per-turn derivation delta.
- `context` — `affinity_reason`, `eval_skip_reason` when no eval ran, the
  judge's verbatim signed `grades`, the gate's `pending_after`, and the 4.0
  endpoint audit: `warmth_grade`/`patience_grade` (only when read this turn),
  `boost_warmth`/`boost_patience` (the B values in force), `decay_factor`,
  and `units` (the per-line units in force). `cross_penalty_assessed` joins
  them whenever a turn was taxed.

### Per-turn label changes

`label_changes JSONB` on `engine.companion_affinity_events` records the
engine-authoritative tier transition for the turn:

```
label_changes = {
  bond:      { from: "<tier_key>", to: "<tier_key>" }  // if bond tier changed
  chemistry: { from: "<tier_key>", to: "<tier_key>" }  // if chemistry tier changed
}
// NULL when neither tier moved this turn
```

## API surfaces

### `AffinitySnapshot`

Returned by `GET /bff/v1/comp/affinity/{session_id}`, refreshed at read time
(`apply_time_decay` + `refresh_endpoints`). The snapshot includes:

```json
{
  "warmth": 0.52,
  "trust": 0.08,
  "intrigue": 0.12,
  "intimacy": 0.05,
  "patience": 0.27,
  "tension": 0.04,
  "bond": 0.10,
  "chemistry": 0.045,
  "bond_tier": 1,
  "chem_tier": 1,
  "bond_label": "acquaintance",
  "chemistry_label": "spark",
  "ghost_streak": 0,
  "total_ghosts": 0,
  "updated_at": "2026-08-16T12:00:00.000000Z"
}
```

- `warmth` / `patience` — the derived endpoint values (0–1, no negatives as
  of 4.0).
- `bond` / `chemistry` — the real stored composite scores (0–1); no display
  curve is applied.
- `bond_tier` / `chem_tier` — the 1..=5 tier index, `tier_index`'s own result.
  Persisted to the row's `bond_tier` / `chem_tier` columns for SQL consumers;
  clients read one of the two rather than re-deriving from the score.
- `bond_label` / `chemistry_label` — one of the 10 tier keys above.

### BFF `/bff/v1/comp/affinity/{session_id}/event`

This endpoint returns the per-turn affinity delta. In addition to
`effective_deltas` (per-axis applied
change, `after − before` — for `warmth`/`patience` this is the per-turn
derivation delta), the event carries:

```json
{
  "session_id": "…",
  "event": {
    "event_id": "…",
    "event_type": "message",
    "effective_deltas": {
      "warmth": 0.06, "trust": 0.02, "intrigue": 0.0,
      "intimacy": 0.0, "patience": 0.01, "tension": -0.02
    },
    "effective_deltas_computed": {
      "bond": 0.01,
      "chemistry": -0.01
    },
    "label_changes": {
      "bond": { "from": "acquaintance", "to": "friend" }
    },
    "created_at": "…"
  }
}
```

- `effective_deltas_computed` — the exact per-turn bond/chemistry delta,
  computed at persist time from the before/after scores and stored on the
  event row (`companion_affinity_events.effective_line_deltas`). `null` /
  absent on pre-migration rows.
- `label_changes` — engine-authoritative tier transition for this turn;
  `null` (or absent) when no tier moved.

Both fields are stored on the event row, so a direct query against
`engine.companion_affinity_events` sees them alongside `state_before` /
`state_after`.

## Source

- `crates/eros-engine-core/src/affinity.rs` — types, `grade_turn` write pipeline, endpoint derivation, time decay, bond/chemistry scores, tiers, labels, diff_labels
- `crates/eros-engine-store/src/affinity.rs` — `AffinityRepo` (persist_with_event, record_ghost), migration 0048
- `crates/eros-engine-server/src/pipeline/post_process.rs` — LLM evaluation, grade/level parsing
- `crates/eros-engine-server/src/prompt.rs` — affinity → attitude directive + eval prompt
- `crates/eros-engine-server/src/routes/dto.rs` — `AffinitySnapshot` (composite scores + labels)
- `crates/eros-engine-server/src/routes/bff/affinity.rs` — BFF affinity surface (value + event)
- Design spec: `docs/superpowers/specs/2026-08-16-affinity-40-design.md`

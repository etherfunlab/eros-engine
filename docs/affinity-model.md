# Affinity model

[English](affinity-model.md) · [中文](affinity-model.zh.md)

A six-dimensional vector that mutates with every chat turn. It's the load-bearing piece that makes the persona feel like a person and not a chatbot.

## The six dimensions

| Field | Range | Default | What it shapes |
|-------|-------|---------|----------------|
| `warmth` | −1.0 ↔ 1.0 | `0.3` | Tone, address. Negative = guarded, hostile; positive = warm, affectionate. |
| `trust` | 0.0 ↔ 1.0 | `0.2` | Topic depth, willingness to disclose self. |
| `intrigue` | 0.0 ↔ 1.0 | `0.5` | Curiosity, follow-up questions, anti-ghost driver. |
| `intimacy` | 0.0 ↔ 1.0 | `0.0` | Inside jokes, nicknames, callbacks to earlier details. |
| `patience` | 0.0 ↔ 1.0 | `0.5` | Tolerance for short / low-effort messages; ghost-threshold input. |
| `tension` | 0.0 ↔ 1.0 | `0.1` | Push-pull, playful friction, "tsundere" affordance. |

`warmth` is the only dimension that can go negative. The other five are bounded `[0, 1]`. All six are clamped on every update.

## EMA smoothing

LLM-evaluated deltas are applied through exponential moving average to avoid lurches:

```
new_value = clamp(old_value + (1 − ema_inertia) × delta)
```

Default `ema_inertia = 0.8` (configurable via `EMA_INERTIA`). With the default, an LLM-suggested delta of `+0.5` only moves the value by `+0.1` on this turn — the rest catches up over subsequent turns if the same direction holds.

```rust
// From crates/eros-engine-core/src/affinity.rs
pub fn apply_deltas(&mut self, d: &AffinityDeltas, ema_inertia: f64) {
    let blend = 1.0 - ema_inertia;
    self.warmth   = clamp(self.warmth   + blend * d.warmth,   -1.0, 1.0);
    self.trust    = clamp(self.trust    + blend * d.trust,     0.0, 1.0);
    // … same for intrigue, intimacy, patience, tension
    self.updated_at = Utc::now();
}
```

### Worked example

Initial `warmth = 0.3`. LLM evaluates this turn's delta as `+0.5`. Default inertia.

```
new_warmth = clamp(0.3 + (1 − 0.8) × 0.5)
           = clamp(0.3 + 0.10)
           = 0.40
```

After three consecutive `+0.5` deltas (still under default inertia), warmth has moved 0.3 → 0.4 → 0.5 → 0.6. The persona warms up over four turns instead of jumping in one.

## Time decay

Three of the six dimensions drift on real time when no one's around. Decay is computed lazily — on every load, by reading `updated_at`:

```
days_elapsed = (now − updated_at) / 1 day

intrigue = clamp(intrigue − 0.01  × days_elapsed,  0.0, 1.0)
patience = clamp(patience + 0.005 × days_elapsed,  0.0, 1.0)
tension  = clamp(tension  − 0.005 × days_elapsed,  0.0, 1.0)
```

`warmth`, `trust`, and `intimacy` don't decay — they're "deep" dimensions. Once you've earned trust, walking away for a week shouldn't reset it; the persona just becomes a little less curious and a little more forgiving in the meantime.

10 days of silence:
- `intrigue` drops by `0.10`
- `patience` recovers by `0.05`
- `tension` softens by `0.05`

## Relationship labels

Five labels emerge from threshold rules; they are not user-selectable. The match is priority-ordered (first hit wins):

| Label | Condition |
|-------|-----------|
| `romantic` | `warmth ≥ 0.7` AND `tension ≥ 0.3` AND `intimacy ≥ 0.4` |
| `friend` | `warmth ≥ 0.7` AND `trust ≥ 0.6` AND `tension < 0.2` |
| `frenemy` | `warmth < 0.4` AND `tension ≥ 0.6` AND `intrigue ≥ 0.5` |
| `slow_burn` | `intrigue ≥ 0.6` AND `tension ≥ 0.4` AND `intimacy < 0.4` |
| `stranger` | none of the above |

The label feeds back into the persona's system prompt — `prompt.rs` rewrites the attitude directive based on the current label. The user never sees the label; they feel its consequences in the persona's tone.

## Persistence

One row per chat session in `engine.companion_affinity` (1:1 via `session_id UNIQUE FK`). Every mutation also appends to `engine.companion_affinity_events`:

| `event_type` | When |
|--------------|------|
| `message` | Reply succeeded; deltas evaluated by LLM |
| `ghost` | Ghost decision; ghost_streak/total_ghosts incremented (no deltas) |
| `gift` | Gift event delivered; deltas from request body |
| `time_decay` | Reserved (currently unused — decay is applied lazily on load) |

Events are append-only and never edited. Full history is queryable for analysis, audit, or reconstructing how a relationship evolved.

## Source

- `crates/eros-engine-core/src/affinity.rs` — types, EMA, time decay, label inference (10 unit tests)
- `crates/eros-engine-store/src/affinity.rs` — `AffinityRepo` (persist_with_event, record_ghost)
- `crates/eros-engine-server/src/pipeline/post_process.rs` — LLM evaluation of per-turn deltas
- `crates/eros-engine-server/src/prompt.rs` — affinity → attitude directive

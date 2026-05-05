# Ghost mechanics

[English](ghost-mechanics.md) · [中文](ghost-mechanics.zh.md)

The persona deciding **not** to reply this turn. Deterministic — no LLM call. The single mechanic that does the most work to make the chat feel like talking to a person who has their own state.

## Why ghosting matters

Most LLM chat UIs reply to everything. This trains users to write low-effort messages — there's no consequence. eros-engine's persona has finite patience and finite curiosity, modelled in the affinity vector, and turns silent when both are low. That silence does two things at once:

1. Pushes the user to put more in (real conversation, not stenography to the bot).
2. Makes the relationship feel non-trivial — you can be ghosted, which means you can also earn replies.

## The score

```
ghost_score = (1 − intrigue) × 0.4
            + (1 − patience) × 0.4
            + tension       × 0.2
```

- High score = the persona is bored, fed up, or in a friction phase. Likely to ghost.
- Score is in `[0, 1]`.

`intrigue` and `patience` carry equal weight (0.4 each); `tension` is a smaller modifier (0.2). Implementation:

```rust
// crates/eros-engine-core/src/ghost.rs
pub fn score(a: &Affinity) -> f64 {
    (1.0 - a.intrigue) * 0.4 + (1.0 - a.patience) * 0.4 + a.tension * 0.2
}
```

## Four protection layers

Score alone doesn't decide. Four rules run in priority order before the threshold check:

```
1. message_count < 10            → never ghost
                                    (relationship still nascent)

2. ghost_streak ≥ 2              → never ghost twice in a row
                                    (avoid the "she's gone" cliff)

3. last_ghost < 1h ago           → cooldown
                                    (if I just ghosted you, give it a beat)

4. otherwise:
     base threshold     = 0.65
     if recent ghost:
       threshold = 0.85          (raise the bar after a ghost)
     ghost iff score > threshold
```

Implementation:

```rust
pub fn decide(a: &Affinity, s: GhostSignals) -> GhostDecision {
    if s.message_count < 10 { return GhostDecision::Reply; }
    if a.ghost_streak >= 2 { return GhostDecision::Reply; }
    if matches!(s.hours_since_last_ghost, Some(h) if h < 1.0) {
        return GhostDecision::Reply;
    }
    let threshold = if s.hours_since_last_ghost.is_some() { 0.85 } else { 0.65 };
    if score(a) > threshold {
        GhostDecision::Ghost
    } else {
        GhostDecision::Reply
    }
}
```

## Worked examples

### Example 1: clear ghost

`intrigue=0.1, patience=0.1, tension=0.5`, message_count=50, no recent ghost.

```
score = (1−0.1)×0.4 + (1−0.1)×0.4 + 0.5×0.2
      = 0.36 + 0.36 + 0.10
      = 0.82
```

`0.82 > 0.65` → **Ghost**.

### Example 2: blocked by cooldown

Same affinity as above, but `last_ghost = 30 minutes ago`. Cooldown rule (rule 3) fires before threshold check → **Reply**.

### Example 3: high score, post-ghost protection

`intrigue=0.05, patience=0.05, tension=0.0`, last_ghost=2h ago. ghost_streak=1.

```
score = (1−0.05)×0.4 + (1−0.05)×0.4 + 0×0.2
      = 0.38 + 0.38 + 0
      = 0.76
```

Recent ghost → threshold raised to `0.85`. `0.76 ≤ 0.85` → **Reply** (but a short, dry one — the affinity is still bad, the persona is just choosing to engage minimally rather than disappear).

### Example 4: nascent relationship

`intrigue=0, patience=0, tension=1.0`, message_count=5.

`score = (1)×0.4 + (1)×0.4 + 1×0.2 = 1.0` — would ghost in any other context. But message_count<10 (rule 1) → **Reply**. New relationships always get a reply, regardless of how unpleasant the user has been.

## Tuning intuition

If the persona ghosts too aggressively → raise base threshold (0.70+) or weight `tension` higher.
If the persona never ghosts → check that LLM affinity-evaluation is actually moving `intrigue` and `patience` down on bad turns. The defaults assume a working evaluator pushing those metrics around.

## What ghosting is not

- It's **not** an error response. The HTTP route still returns 200. The body has `reply: null` (or the engine's chosen "no reply" shape).
- It's **not** an LLM call gone wrong. The decision is pure Rust; the LLM never gets asked.
- It's **not** silent forever. Time-decay restores `patience` and softens `tension`; eventually the persona will reply again to the next message.

## Source

- `crates/eros-engine-core/src/ghost.rs` — score + decide (7 unit tests)
- `crates/eros-engine-server/src/pipeline/handlers.rs::GhostHandler` — handler that returns no chat request
- `crates/eros-engine-store/src/affinity.rs::record_ghost` — persistence (increments streak, total_ghosts, last_ghost_at)

# Ghost mechanics

[English](ghost-mechanics.md) · [中文](ghost-mechanics.zh.md)

The persona deciding **not** to reply this turn. By default the decision is deterministic and makes no LLM call. With the opt-in LLM PDE judge (`[tasks.pde_decision].filter_prompt`) configured, the judge proposes the turn's action instead; the scoring below stays on as the fallback, plus a hard-safety veto the judge can never override. The single mechanic that does the most work to make the chat feel like talking to a person who has their own state.

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
     if this session has ever ghosted:
       threshold = 0.85          (bar stays raised for the rest
                                  of the session)
     ghost iff score > threshold
```

Implementation:

```rust
// Rules 1-3 live in their own fn: the LLM PDE path reuses them as a veto,
// without the score threshold (the judge decides ghost-worthiness itself).
pub fn ghost_permitted(a: &Affinity, s: GhostSignals) -> bool {
    if s.message_count < 10 { return false; }
    if a.ghost_streak >= 2 { return false; }
    if matches!(s.hours_since_last_ghost, Some(h) if h < 1.0) { return false; }
    true
}

pub fn decide(a: &Affinity, s: GhostSignals) -> GhostDecision {
    if !ghost_permitted(a, s) { return GhostDecision::Reply; }
    let threshold = if s.hours_since_last_ghost.is_some() { 0.85 } else { 0.65 };
    if score(a) > threshold {
        GhostDecision::Ghost
    } else {
        GhostDecision::Reply
    }
}
```

The raised bar does not decay: the branch checks `hours_since_last_ghost.is_some()`, `last_ghost_at` is only ever set and never cleared, and the affinity row is 1:1 with a chat session — so once a session has ghosted at all, 0.85 is the threshold for the rest of that session (outside the 1-hour cooldown, where rule 3 forces a reply anyway).

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

The session has ghosted before (`last_ghost_at` is set) → threshold is `0.85`. `0.76 ≤ 0.85` → **Reply** (but a short, dry one — the affinity is still bad, the persona is just choosing to engage minimally rather than disappear).

### Example 4: nascent relationship

`intrigue=0, patience=0, tension=1.0`, message_count=5.

`score = (1)×0.4 + (1)×0.4 + 1×0.2 = 1.0` — would ghost in any other context. But message_count<10 (rule 1) → **Reply**. New relationships always get a reply, regardless of how unpleasant the user has been.

## Tuning intuition

If the persona ghosts too aggressively → raise base threshold (0.70+) or weight `tension` higher.
If the persona never ghosts → check that LLM affinity-evaluation is actually moving `intrigue` and `patience` down on bad turns. The defaults assume a working evaluator pushing those metrics around.

## What ghosting is not

- It's **not** an error response. The HTTP route still returns 200. Because the engine is SSE-streaming, a ghost turn emits three frames and then closes the stream: `meta(action_type=ghost, model=null)` → `done(usage=null, generation_id=null)` → `final`. No `delta` frame is emitted and no LLM is called.
- It's **not** an LLM call gone wrong. With the default rule engine the decision is pure Rust and the LLM never gets asked. With the opt-in LLM PDE judge configured the judge proposes the action, but `ghost_permitted` still vetoes any ghost the hard-safety rules forbid, and the `ghosting` switch can force every ghost verdict back to `reply_text` — see [model-config.md](model-config.md).
- It's **not** the only way a turn goes silent. A reply whose text resolves empty gets there by a different route: the model returned an empty completion, or `apply_output_regex` stripped an artifact-only reply to nothing (the fail-safe there was deliberately removed). That turn is persisted as an ordinary assistant reply row with empty content, surfaces as `done(ghost_fallback=true)` with `metadata.fallback_reason` of `empty_completion` or `regex_strip`, and does **not** touch `ghost_streak` / `total_ghosts` / `last_ghost_at` — the persona decided nothing; the reply just came back empty. One exception: a turn that promises a photo (`reply_text_image`) is **not** tagged, on either route. Its empty text half is an image-only reply — the trailing `image_request` is the payload the user actually receives — so it reports `ghost_fallback=false`, persists no `fallback_reason`, and is treated as a served reply (it *does* reset `ghost_streak`, like any other reply).
- It's **not** silent forever. Time-decay restores `patience` and softens `tension`; eventually the persona will reply again to the next message.

## Source

- `crates/eros-engine-core/src/ghost.rs` — score + ghost_permitted + decide (12 unit tests)
- `crates/eros-engine-server/src/pipeline/stream.rs::run_stream` — the `ActionType::Ghost` arm: stamps the row and records the ghost, building no chat request
- `crates/eros-engine-store/src/affinity.rs::record_ghost` — persistence (increments streak, total_ghosts, last_ghost_at), plus a zero-delta `companion_affinity_events` row with `event_type='ghost'`
- `crates/eros-engine-store/src/chat.rs::mark_user_message_ghosted` — sets `chat_messages.ghost_decision = true` on the user row, so replay can tell a ghost outcome from a still-generating turn

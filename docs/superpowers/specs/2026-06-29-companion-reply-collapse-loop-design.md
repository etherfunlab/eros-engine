# eros-engine — fix companion reply-collapse feedback loop (#113)

**Status**: design (ready for implementation plan)
**Target release**: `0.6.x` dev track. **No schema migration.**
**Scope**: the root cause of [#113](https://github.com/etherfunlab/eros-engine/issues/113) —
a companion persona's replies collapsing to a single near-identical line that
recurs for days (e.g. the `我看着…` gaze template). This spec cuts the
self-reinforcing prompt-feedback loop at its source and adds recall hygiene, then
retires the now-redundant symptom patch in iron-rule ⑨. Follow-up to the
[2026-06-13 reply-quality spec](2026-06-13-companion-reply-quality-design.md)
(Spec 2 of the issue-#84 follow-up): that work landed but this failure mode
survived it.

**Explicitly deferred** (fast-follows, NOT in this spec):
- The anti-repetition-guard *upgrade* (#113 proposed-fix 3): full-sentence /
  motif-level detection beyond `overused_openings`' 4-char opening, and an
  optional post-generation near-duplicate check + single regeneration in the
  stream path.
- The repetition metric/alert (#113 proposed-fix 4): observability, a separate
  concern.

---

## 0. Background & root cause (verified against source)

The companion chat model's own output is fed back into its next prompt through two
channels:

1. **Short-term** — bounded, *not* the driver:
   - the real chat history (`HISTORY_WINDOW = 20` messages, `handlers.rs:41`,
     sent as actual `user`/`assistant` messages via `assemble_chat_request`), and
   - a 3-pair `[recent_conversation]` re-render in the system prompt
     (`fetch_recent_turn_pairs(…, 3)`, `handlers.rs:721`; rendered at
     `prompt.rs:519`).
   Both scroll out; they keep the model coherent and are necessary.

2. **Long-term — the key driver.** Every turn, `write_turn`
   (`post_process.rs:299`) persists the assistant's prose **verbatim** to
   `engine.companion_memories` as a Relationship-layer row:
   ```rust
   let rel_content = format!("用户：{user_msg}\nAI：{assistant_msg}"); // category = NULL
   ```
   On later turns, `recall_memory_with_embedding` (`handlers.rs:316`,
   `RELATIONSHIP_RECALL_K = 3`) pulls those rows back by cosine similarity, and
   `build_prompt` injects them into `[shared_memories]` **verbatim** — only a
   `"- "` prefix, no dedup, no self-output filter (`prompt.rs:423`, `:542`). So
   the persona's own boilerplate becomes a recallable "memory." Each re-emission
   writes *another* near-duplicate copy → recall gets even more likely to surface
   it → positive feedback that persists across sessions/days.

There are **two writers** to `companion_memories`; only the first is the polluter:
- `write_turn` (`post_process.rs`) — verbatim turns, `category = NULL`: the
  Relationship row above (`用户：…\nAI：…`) plus a Profile-layer row holding the
  bare `user_msg`. Eager, every turn.
- dreaming-lite (`dreaming.rs`) — a session-end sweeper that runs the
  `memory_extraction` LLM and writes **categorized** facts
  (`fact/preference/event/emotion/relation`, `normalise_category` `dreaming.rs:329`)
  to the **Profile layer only**. Never stores assistant prose.

**Why the v0.6.1 guards miss it** (all confirmed):
- `frequency_penalty` / `presence_penalty` (set per-task, `model_config.rs`)
  penalize repetition only *within one completion* — they cannot penalize a phrase
  already present in the **prompt**.
- `repetition::overused_openings` (`repetition.rs`) fingerprints only the first
  `OPENING_CHARS = 4` chars of the first sentence; a fixed vocative opening with
  the gaze template in the *second* clause defeats it, and it is a *soft*
  `[avoid_repetition]` directive the weakest fallback model ignores.
- Iron-rule ⑨ (`prompt.rs:557`) is opening-scoped, satisfied-on-the-letter once
  the gaze verb moves past the first clause.
- Recall is **unguarded** — no dedup, no "don't recall my own recent prose"
  filter.

---

## 1. Change 1 — write side: store the user's turn only (`post_process.rs`)

Stop persisting the persona's prose as Relationship-layer memory — the actual
polluter. The user's own utterance is the legitimate relationship signal and
cannot self-reinforce the model's output.

- Extract the content format into a pure helper so it is unit-testable without a
  DB / Voyage mock (no `write_turn` tests exist today):
  ```rust
  /// Relationship-layer memory content for a turn. Stores only the user's
  /// utterance — never the assistant's prose, which would feed back into the
  /// model's own prompt via recall (see #113).
  fn relationship_memory_content(user_msg: &str) -> String {
      format!("用户：{user_msg}")
  }
  ```
  Keep the `用户：` label so a recalled line still reads as *what the user said*.
- At `post_process.rs:299`, replace the inline `format!("用户：{u}\nAI：{a}")` with
  `relationship_memory_content(user_msg)`.
- Profile-layer write (bare `user_msg`, `post_process.rs:323`) and dreaming-lite
  are unchanged — neither ever stored assistant prose.
- The user-turn memory is written **once per turn**, not once per produced
  message: `write_turn` (which no longer takes the assistant text) is called from
  a single guarded `should_write_user_turn(user_msg, &produced)` check. A
  multi-message assistant burst would otherwise insert identical duplicate
  `用户：{u}` rows (caught in review — codex `[P2]`).

**Result:** after this change, **no writer persists assistant prose into
`companion_memories`.** The long-term channel can no longer carry the model's own
boilerplate forward.

## 2. Change 2 — legacy rows: filter the transcript format at recall (`store/memory.rs`)

New writes are clean, but a deployment's DB already holds `category = NULL`
verbatim `用户：…\nAI：…` rows. Recall self-suppression (Change 3) only catches
rows matching the persona's *recent* output, so a dormant old `我看着…` row that is
cosine-near the query could still surface and re-seed the loop. Neutralize the
legacy format non-destructively, at recall time:

- In the **Relationship** branch of `MemoryRepo::search` (the `Some(instance_id)`
  path, `memory.rs`), add to the `WHERE`:
  ```sql
  AND content NOT LIKE E'%\nAI：%'
  ```
  Placed in the `WHERE` so it is **filter-before-`LIMIT`** — the query still
  returns up to `RELATIONSHIP_RECALL_K = 3` clean rows. New user-only rows (no
  `\nAI：`) pass; the Profile (`None`) path is untouched.
- Non-destructive, reversible, runs safely on every deployment (no migration, no
  data loss). It soft-quarantines the polluted legacy format; downstream may purge
  later if it wishes (out of scope for the engine, per OSS boundary).

## 3. Change 3 — recall hygiene: dedup recalled memories

New pure, unit-testable module `crates/eros-engine-server/src/memory_hygiene.rs`,
registered in `crates/eros-engine-server/src/main.rs` (alongside `repetition`; the
server crate is a binary, no `lib.rs`):

```rust
/// Dedup recalled memories before they are injected into the prompt: drop any
/// recalled item that duplicates another (normalized equality, or length-guarded
/// containment). Pure, order-preserving, dependency-free (no embeddings).
pub fn prune_recalled(
    profile_groups: Vec<(String, Vec<String>)>,
    relationship_facts: Vec<String>,
) -> (Vec<(String, Vec<String>)>, Vec<String>);
```

- **Normalize** each candidate: strip a leading `用户：` / `AI：` speaker label,
  collapse internal whitespace, trim, lowercase ASCII (CJK unaffected;
  char-boundary-safe, mirroring `repetition.rs`).
- **Dedup:** across the whole injected set (profile items first, then relationship
  facts), keep the first occurrence and drop later items whose normalized form
  **exactly equals** an already-kept one. Main real-world case: the same turn's
  Relationship `用户：{u}` vs Profile raw `{u}` recalled together. Profile-first
  ordering means a fact present in both layers is kept in `[user_profile]` and
  dropped from `[shared_memories]`. (Equality, **not** containment — a richer
  memory that merely contains a shorter fact, e.g. `用户：喜欢吃意大利面，但是对海鲜过敏`
  vs a base `喜欢吃意大利面`, is kept; codex `[P2]`.)
- **Wire-up** (`handlers.rs`, just before `build_prompt`): pass the recall result
  through `prune_recalled` and shadow `profile_groups`/`relationship_facts` with
  the pruned values. **Pure; no new DB calls.**

Note: self-output suppression (dropping recalled items that echo the persona's
recent assistant output) was considered and **dropped during review** — Change 1
\+ Change 2 already ensure no assistant-authored text reaches recall, so
suppression had no legitimate target and only risked pruning legitimate user
facts the assistant had echoed (codex `[P2]`). Dedup is the remaining,
always-safe hygiene.

## 4. Change 4 — retire the symptom patch in iron-rule ⑨ (`prompt.rs`)

Iron-rule ⑨ conflates two things; the loop fix makes only the first redundant:
1. a **#113-specific symptom callout** — the `（如「我看着…」「我盯着…」）`
   gaze-template enumeration; and
2. a **general engage-first principle** — `先接住对方刚说的话，针对那句话回应，
   而不是自说自话` — which targets the *base-model* opening-gaze tic the 2026-06-13
   spec measured (63% open with `我`, 40% `我`+gaze), a structural tendency that
   existed before any verbatim recurrence and is **not** addressed by the loop fix.

Rewrite ⑨ to drop (1) and keep (2). `prompt.rs:557`:

- **Before:**
  `⑨ 别开口就自述动作或凝视（如「我看着…」「我盯着…」）；先接住对方刚说的话，针对那句话回应，而不是自说自话。`
- **After:**
  `⑨ 别开口就自述动作或凝视；先接住对方刚说的话，针对那句话回应，而不是自说自话。`

Only the parenthetical enumeration is removed; `别开口就自述动作或凝视` and the
engage-first clause remain. Sibling style directives from the same 2026-06-13 spec
— ⑩ ellipsis restraint (`prompt.rs:558`) and ⑪ Chinese first-person-opening
(`:559`), plus the Japanese ③ — target base-model style tics unrelated to the
loop and **stay unchanged**.

---

## 5. Backward compatibility & boundary

- **No schema migration**, no config change, no API surface change, **no new LLM
  calls**. The new relationship write format is forward-only; legacy rows are
  soft-quarantined at recall.
- OSS-clean: no product identity, names, or URLs introduced. Iron-rule ⑨ already
  lives in engine code; the rewrite stays generic.
- Existing `build_prompt` ordering / cache-prefix invariant tests are unaffected
  (⑨'s edit is text inside the already-volatile iron-rules block; no const in the
  stable cache prefix changes).

## 6. Out of scope (deferred fast-follows)

- **#113 fix 3 — anti-repetition upgrade.** Extend repetition detection from
  opening-only to full-sentence / motif level (repeated substrings anywhere, plus
  retrieved-context overlap), and optionally a post-generation near-duplicate
  check + single regeneration in the stream path. Heavier (touches the stream
  path / latency); its own spec.
- **#113 fix 4 — metric/alert.** Per session/persona, count repeated assistant
  n-grams + exact reply hashes over a rolling window; alert when a phrase recurs
  in both `chat_messages` and `companion_memories`. Observability; its own spec.
- Backfill/purge of legacy `category = NULL` rows in any deployment's DB —
  downstream concern (engine ships only the non-destructive recall filter).

## 7. Testing

- **`post_process.rs`**: `relationship_memory_content` returns `用户：{u}` with no
  `AI：` segment (pure unit test).
- **`store/memory.rs`** (`sqlx::test`): relationship `search` excludes a legacy
  `用户：…\nAI：…` row and returns a clean user-only row for the same instance;
  filter-before-`LIMIT` still yields up to K rows; the Profile (`None`) path is
  unaffected by the new clause.
- **`memory_hygiene.rs`** (pure unit tests): dedups exact duplicates and the
  cross-layer `用户：{u}` / `{u}` pair; does **not** drop a richer memory that
  merely contains a shorter fact (equality-only dedup); preserves order; handles
  empty inputs.
- **`post_process.rs`** (pure unit tests): `should_write_user_turn` returns true
  only when the user message is non-empty and the burst has ≥1 non-empty produced
  message — a single decision per turn (multi-message burst ⇒ one write).
- **`handlers.rs`**: the `prune_recalled` wire-up is verified by compile + the full
  server suite (its dedup logic is unit-tested in `memory_hygiene.rs`).
- **`prompt.rs`**: existing `build_prompt_renders_anti_templating_directives`
  (`:1646`) stays green (it asserts `别开口就自述动作或凝视`, which is retained);
  add an assertion that the gaze enumeration is gone (e.g. with empty
  `avoid_patterns`, `!s.contains("我盯着…")`). `build_prompt_renders_avoid_repetition_when_present`
  (`:1261`) is unaffected (its `我看着你`/`我盯着你` come from the `[avoid_repetition]`
  block, not ⑨).
- **Pre-PR gate**: `fmt` / `clippy` / `test` / `openapi` (no API change expected —
  run `openapi` to confirm no drift).

## 8. File-touch summary

| File | Change |
| --- | --- |
| `crates/eros-engine-server/src/pipeline/post_process.rs` | relationship content = user-only via pure `relationship_memory_content`; write once per turn via `should_write_user_turn` (drop dead `assistant_msg` param) |
| `crates/eros-engine-store/src/memory.rs` | relationship `search` (`Some(instance_id)`) excludes legacy `\nAI：` rows (+ test) |
| `crates/eros-engine-server/src/memory_hygiene.rs` | **new** pure module `prune_recalled` (dedup) + tests |
| `crates/eros-engine-server/src/main.rs` | register `memory_hygiene` mod (server is a binary, no `lib.rs`) |
| `crates/eros-engine-server/src/pipeline/handlers.rs` | pass the recall result through `prune_recalled` before `build_prompt` |
| `crates/eros-engine-server/src/prompt.rs` | rewrite iron-rule ⑨ — drop the `（如「我看着…」「我盯着…」）` gaze enumeration, keep the engage-first clause (+ test assertion) |

## 9. Open decisions — all resolved

- Scope: **root-cause write fix + recall hygiene**; anti-rep upgrade (#3) and
  metric (#4) deferred.
- Write fix: **user-turn-only** relationship row (`用户：{u}`), drop the `AI：{a}`
  half. (Not full extraction, not dropping the relationship write entirely.)
- Legacy rows: **non-destructive recall-time SQL filter** (exclude the
  `用户：…\nAI：…` transcript shape). No migration.
- Recall hygiene: **dedup only**, pure module; no new DB calls, no embeddings.
  (Self-output suppression was removed during review — redundant once Changes 1+2
  cut the loop, and it risked pruning user facts the assistant echoed; codex `[P2]`.)
- Write cadence: the user-turn memory is written **once per turn**
  (`should_write_user_turn`), not per produced message — avoids duplicate rows on
  multi-message bursts (codex `[P2]`).
- Iron-rule ⑨: **rewrite** (drop the gaze enumeration, keep engage-first), not
  delete — the base-model opening-gaze tic is not loop-driven.

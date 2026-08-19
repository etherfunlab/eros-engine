# Echo cancellation for injected chat history — Design

- **Date:** 2026-08-19
- **Status:** Approved
- **Type:** Engine change — no migration, no schema change, no API change. One
  pure function, one call site, one env flag.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the text chat turn only. The release number and
  its timing are the owner's call, not this document's.

## 1. Motivation

A text turn injects the session's last 20 `chat_messages` rows into the prompt
(`HISTORY_WINDOW`, `handlers.rs`). Whatever is in that window is what the model
conditions on — including its own earlier output.

When a model repeats itself, the repetition is written to `chat_messages` and
comes back in the next turn's window. The model now sees the same string twice,
which raises the probability it emits it a third time. That copy is stored too.
The loop is closed through the database.

The sliding window already bounds the damage: a repeated string eventually
scrolls out, so a single bad generation no longer poisons the rest of the
session. It does not stop the loop while the string is still inside the window,
and it does not stop the loop from filling the window faster than the window
scrolls.

This matters more for an OSS engine than for any single deployment. **We do not
choose the model our consumers run.** A downstream operator can point this
engine at a 24B roleplay finetune, a frontier model, or a local quantised
build, and the engine's history-injection mechanism is identical in all three
cases. If the mechanism amplifies whatever repetition the model produces, then
the engine's quality floor is set by the worst model anyone plugs into it. The
mechanism must not amplify. That is the entire goal here — not to make models
stop repeating, which is not achievable (§3.2), but to stop the engine from
feeding the repetition back.

## 2. Design principles applied

1. **Fix the mechanism, not the model.** Repetition is a property of the
   models. Amplification is a property of our history injection. Only the
   second one is ours to fix.
2. **The window only shrinks; it is never refilled.** Dropping a duplicate does
   not trigger a wider query to backfill the slot. A turn whose window
   collapses from 20 rows to 4 gets 4 rows. Backfilling would make the drop
   invisible and reintroduce whatever sits just outside the window.
3. **Deduplicate on what the model actually reads.** The key is the exact
   string handed to the provider, not the `content` column it was derived
   from (§4.2). Anything else can drift from what is really injected.
4. **The user's current message is never dropped.** A rule that can silently
   delete the thing the user just typed is not a quality mechanism (§4.3).
5. **Byte-exact only.** No trimming, no normalisation, no similarity
   threshold, no length floor. A near-duplicate is a different string and stays.
6. **Read-only on stored data.** Nothing is deleted, rewritten, or flagged in
   `chat_messages`. The rule applies at injection time and nowhere else.

## 3. Evidence

### 3.1 Production measurement

Measured on one production deployment over 2026-08-05 → 2026-08-18: 11,171
messages across 415 sessions. Restricted to injectable rows — `channel IS NULL`,
role in (`user`, `gift_user`, `assistant`), non-empty content — that is 5,051
user rows and 4,951 assistant rows. "Duplicate" below means byte-identical
content to an earlier injectable row **within 20 rows of it**, i.e. inside a
single injection window.

Duplicates by direction:

| earlier row → duplicate row | rows | sessions | median len | ≥30 chars |
|---|---:|---:|---:|---:|
| assistant → assistant (model repeating itself) | 75 | 30 | 29 | 45 |
| user → user (resend, or short interjection) | 159 | 45 | 2 | 38 |
| user → assistant (model parroting the user) | 15 | 15 | 2 | 0 |

Assistant self-repetition (≥10 chars) by model:

| model | assistant rows | self-repeats | rate |
|---|---:|---:|---:|
| nousresearch/hermes-4-70b | 26 | 4 | 15.38% |
| z-ai/glm-4.7-flash | 255 | 11 | 4.31% |
| thedrummer/cydonia-24b-v4.1 | 28 | 1 | 3.57% |
| sao10k/l3.3-euryale-70b | 763 | 25 | 3.28% |
| x-ai/grok-4.20 | 814 | 11 | 1.35% |
| deepseek/deepseek-v4-flash-0731 | 975 | 8 | 0.82% |
| gemma-4-uncensored@venice | 2090 | 12 | 0.57% |

**Seven models out of seven.** Different vendors, different sizes, different
training pipelines, a 27× spread in rate — and no zeroes. Every model in
production repeats itself inside the injection window.

Amplification is observable, not hypothetical. Within a single 20-row window,
the maximum number of byte-identical copies of one ≥10-char string reached
**8** on the user side and **6** on the assistant side. In the worst turn
measured, **16 of the 20 window slots** were duplicates.

Applying the rule in §4 retroactively to all 5,051 user turns: **675 turns
(13.36%)** would have at least one message dropped, median 2 dropped, maximum
16, across 64 of 415 sessions.

### 3.2 Literature

The production numbers match what the literature says is a property of the
model class rather than of any particular model.

*Repetition In Repetition Out: Towards Understanding Neural Text Degeneration
from the Data Perspective* (NeurIPS 2023, [arXiv:2310.10226](https://arxiv.org/html/2310.10226))
evaluates encoder-decoder Transformers, decoder-only Transformers, and LSTMs.
All models trained by MLE exhibit severe repetition, with no clear indication
that any architecture suffers less. The same work identifies the
**self-reinforcement effect**: sentence-level repetition is self-reinforcing —
the more times a sentence appears in the context, the higher the probability of
emitting it again.

The mechanism is the ordinary autoregressive one ([Raschka, *Why LLMs get stuck
in repetition loops*](https://sebastianraschka.com/faq/docs/repetition-loops-generation.html)):
every generated token joins the context for the next prediction, so once a
phrase is present, the updated context assigns more probability to another
copy. In our case the loop is longer — it runs through `chat_messages` and back
into the next turn's window — but it is the same positive feedback, and the
probability rises monotonically with the number of copies already in context.

Two consequences for an OSS engine:

- **Model choice cannot solve this.** The lowest measured rate in §3.1 is
  0.57%, not zero, and the engine does not control which model a consumer runs.
- **Leaving duplicates in the window is the amplifier.** Since the probability
  is monotone in the count of copies present, removing copies from the context
  is a direct intervention on the feedback term, not a cosmetic filter.

One honest counterweight: deliberately duplicating an *entire prompt* before
generation has been reported to improve accuracy on short factual and
classification tasks ([arXiv:2512.14982](https://arxiv.org/pdf/2512.14982)).
That is a controlled, whole-prompt technique aimed at contextual grounding. It
is not the same phenomenon as duplicates accumulating unintentionally inside a
multi-turn roleplay history, and it is not evidence that accumulated duplicates
help.

## 4. The rule

### 4.1 Statement

Within the messages a single text turn would inject, any **non-empty** string
that appears **more than once** is dropped in **all** of its occurrences —
except an occurrence that is the current turn's own user message, which is
always kept.

Dropped slots are not backfilled.

```
window:  a  b  a  c  d              injected:  b  c  d
window:  a  b  c  a(current)        injected:  b  c  a(current)
window:  a(current)                 injected:  a(current)
```

### 4.2 What "identical" means

The key is the **model-facing string** — the exact `content` that
`assemble_chat_request` puts on the wire — not `chat_messages.content`.

This is load-bearing. In the measured deployment, 1,111 of 5,301 assistant rows
(21%) — all assistant rows this time, including the 350 with empty `content`
that §3.1 excludes — carry a `metadata.image` object, and injection appends
`[你给对方发送了一张照片：{caption}]` to their text. Consequences of keying on
the model-facing string rather than the column:

- Two rows with the same caption text but **different photos** produce
  different injected strings and are both kept. Keying on `content` would
  collapse them and delete a photo the persona sent.
- 85 assistant rows have empty `content` but a photo marker, so their injected
  text is non-empty. Keying on `content` would classify them as empty and
  exempt them from the rule entirely.
- The same reasoning applies to user rows: `model_facing_user_text` folds the
  `metadata.vision` preamble in, so two captionless photos with different
  descriptions stay distinct.

Comparison is **byte-exact**. `trim` is used only to decide whether a string is
empty. Empty strings never participate: they are neither counted nor dropped,
and are injected exactly as they are today. This spec does not change how empty
rows are handled.

**The key does not include the role.** A user line and an assistant line with
the same text are the same key, and both are dropped. This is deliberate: the
`user → assistant` parroting case is real (15 occurrences in §3.1), and it is
the clearest instance of the model conditioning on the window's contents. The
current-turn exemption keeps this from deleting what the user just said.

### 4.3 Why the current turn is exempt

The current user message sits in the same window — the pipeline injects it as
the last history row rather than appending it separately. Without an exemption,
the rule deletes it whenever the user repeats themselves, and the model
receives a turn with no user input at all.

This is not an edge case: **159 turns (3.15%)** in §3.1 would have lost the
user's own message, 66 of them ≥10 characters. The exemption is identified by
`user_message_id`, not by position.

## 5. Where it lives

Three functions, all in the server crate. The split exists so the deduplication
key cannot drift from what is actually injected: only the layer that knows what
reaches the model can compute it.

```
model_facing_history(Vec<ChatMessage>) -> Vec<Injected>
    Drops channel-marked rows and unknown roles, folds user/gift_user to
    "user", materialises the model-facing text (image preamble, photo
    marker). Injected { id: Uuid, role: String, text: String }.
    Extracted from the current body of assemble_chat_request.

repetition::cancel_echo(Vec<Injected>, current_id: Uuid)
    -> (Vec<Injected>, EchoStats)
    Pure, no I/O. Keeps a row when any of: text.trim() is empty; text
    occurs once; id == current_id. Drops the rest, preserving order.

assemble_chat_request(resolved, system_prompt, Vec<Injected>, audit)
    -> ChatRequest
    Unchanged except that it now receives already-materialised messages.
```

`cancel_echo` goes in `crates/eros-engine-server/src/repetition.rs`, which
already holds the prompt-side half of this problem: `overused_openings` mines
over-used sentence openings from recent assistant turns so the prompt can
discourage them. That is the same problem addressed before generation; this is
the same problem addressed in the context. They are complementary and neither
replaces the other.

Wiring:

| Location | Change |
|---|---|
| `handlers.rs` | `build_reply_request` calls `model_facing_history`, then `cancel_echo` when the flag is on |
| `handlers.rs` | `model_facing_assistant_text` takes `&ChatMessage` instead of an owned value (the key is computed without consuming the row) |
| `state.rs` | `ServerConfig` gains `chat_echo_cancellation_disabled: bool`, read as `state.config.…` — the polarity and the location match the `world.disabled` / `dreaming_voice_disabled` flags already there |

The only production call site is `build_reply_request`. Gift and tip turns
reach the model through it as `gift_user` rows and are covered. The voice path
builds its own messages from its own 8-row window (`voice.rs`) and is untouched.

## 6. Configuration

```
CHAT_ECHO_CANCELLATION_DISABLED    unset or anything but "1"/"true"  → ON
                                   "1" or "true"                     → OFF
```

Parsed with the existing `parse_bool_flag`, matching all eight `*_DISABLED`
flags already in the engine: unset means the feature runs, and an operator who
wants it off says so explicitly.

**The flag is permanent, not a rollout kill-switch.** Echo cancellation is a
standing property of the text turn. The flag exists so a downstream operator
whose model or product does not want it can opt out, and so it can be turned
off in an incident without shipping an image. It is documented in
`.env.example` and `docs/deploying.md` alongside the other operational flags,
and it is not scheduled for removal.

## 7. Observability

One `INFO` line per turn, emitted **only** when at least one message was
dropped. Turns with no duplicates log nothing.

```
INFO echo cancellation: duplicate history messages dropped
     dropped=8 kept=12 groups=2 max_occ=5 session_id=…
```

`groups` is the number of distinct duplicated strings; `max_occ` is the largest
copy count in the window — the amplification reading. No message content and no
content hash is logged: production sessions are real conversations.

Nothing is written to the database. The counts are derivable from
`chat_messages` with a window query at any time (that is how §3.1 was
produced), so a second stored copy would be a derived fact with two homes.

## 8. Testing

Unit tests on `cancel_echo` (pure, no database):

1. `a b a c d` → `b c d`
2. `a b c a(current)` → `b c a(current)`
3. a lone `a(current)` → kept
4. empty-text rows are neither counted nor dropped
5. same text under different roles → both dropped
6. same `content` with different photo captions → both kept
7. `dropped` / `kept` / `groups` / `max_occ` are correct

Plus a `handlers.rs` test that the flag off reproduces today's injection
exactly. Existing `assemble_chat_request` tests are updated to the new call
form; assertions are not weakened.

## 9. Non-goals

- **The voice path.** Its own window, its own assembly, unchanged.
- **The `[recent_conversation]` block.** The system prompt separately renders
  the last 3 turn pairs from their own query, and that block is not
  deduplicated. When both copies of a duplicated string fall inside those 3
  pairs, the model still sees both — in the system prompt rather than in the
  message list. This is a strict improvement on the previous behaviour (that
  shape put 4 copies in context; it now puts 2, never more), not a regression.
  Closing it means deduplicating across two separately-queried prompt regions
  on the latency path, which is its own design decision and gets its own spec.
- **A minimum-length threshold.** Measured: adding an 8-character floor leaves
  the assistant-side impact bit-for-bit unchanged (226 turns either way), so
  the knob only exempts short user interjections. Rejected as a knob that buys
  nothing on the side the mechanism exists for, and that cannot be falsified
  from data.
- **Near-duplicate or normalised matching.** Byte-exact only.
- **Any change to stored data, schema, or the API contract.**
- **Merging with `overused_openings`.** Different half of the problem.
- **Stopping models from repeating.** Out of scope and not achievable (§3.2).

## 10. Rejected alternatives

- **Drop every copy including the current turn.** The literal reading of the
  rule. Rejected on measurement: 3.15% of turns would reach the model with no
  user input (§4.3).
- **Keep the last occurrence of each duplicate group.** The conventional
  dedupe-keep-last. Rejected because it keeps stale repeated content in the
  window whenever the last copy is not the current turn — it prunes the count
  but not the amplifier.
- **Assistant rows only.** Narrower (226 turns instead of 675) and avoids
  touching user input, but leaves the user-side duplicates in the window, and
  those are part of what the model conditions on.
- **Deduplicate in the store layer.** The model-facing string does not exist
  until the server crate builds it, so the store cannot compute the key (§4.2).
- **Widen the query and backfill dropped slots.** Rejected by principle 2: it
  hides the drop and pulls in older rows the window had already aged out.

## 11. Risks

- **A persona that legitimately repeats a line loses it.** A catchphrase said
  twice in 20 messages is dropped from both slots. Accepted: this is the same
  string the model is conditioning on, and the prompt-side
  `[avoid_repetition]` block already treats recurring openings as something to
  discourage.
- **A user's short interjection disappears from context.** "嗯" sent three
  times in a window is injected zero times unless one of them is the current
  turn. Accepted; the alternative is a length threshold, rejected in §9.
- **Windows can get small.** Worst measured case: 4 of 20 rows survive. That
  window is 16 copies of the same string; four distinct messages carry more
  information than that.

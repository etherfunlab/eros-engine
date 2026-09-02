# Echo cancellation plus — noise cancellation for injected chat history — Design

- **Date:** 2026-09-02
- **Status:** Draft — ready for review
- **Type:** Engine change. No migration, no schema change. One new prompt
  block reading an existing table, three deletions, one behavioural change to
  history injection. Public API unchanged.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the text chat turn only. Release number and
  timing are the owner's call, not this document's.
- **Supersedes in part:** `2026-08-19-chat-echo-cancellation-design.md`
  (that rule survives; §4.1 here removes a mechanism shipped alongside it).

## 1. Motivation

The 2026-08-19 spec fixed one amplification path: a byte-identical string
appearing twice in the injected window. Production measurement (§3) shows the
path that actually degrades replies is a different one, and that the
prompt-side countermeasure shipped for it is inverted.

Three findings drive this document.

**Openings are contagious, and the contagion compounds with conversation
depth.** The probability that an assistant turn reuses an opening from its own
last six turns rises from 4.7% early in a session to 35.5% deep in one — a
7.6× increase that three separate controls confirm is not survivorship bias
and not the mechanical effect of the window filling up.

**The carrier is interjections, not sentence templates.** Turns whose first
sentence is a single character account for 23.5% of all turns, repeat at
66.1%, and contribute 58.2% of all repetition measured. Turns whose first
sentence runs four characters or longer repeat at 11.5% — at the chance floor.
The disease is `唔` / `啊` / `嗯啊`, not a句式 the model has locked onto.

**Telling the model not to repeat makes it repeat.** The `[avoid_repetition]`
block lists the over-used openings verbatim in the prompt. Naming a string in
context does not remove it from context; it adds it. Measured within a single
message: openings the block named are reused at 30.8%, openings it did not name
at 9.5% — an 8.6× preference after normalising for the number of distinct
openings, p=7.7e-25.

The framing that follows from this is **noise cancellation, not
deduplication**. In an assistant history row, *what was said* is signal and
*how it was said* is noise. Those two are separable in position: the opening
sentence carries almost all of the style and almost none of the information.
The engine should strip the carrier rather than detect duplicates.

## 2. Design principles applied

1. **Demonstration beats description.** A pattern shown in context outranks any
   instruction describing a constraint on it. This is why §4.1 deletes the
   prompt-side countermeasure and why every remaining lever here is mechanical.
2. **Signal and noise are separable by position, not by similarity.** Semantic
   similarity is blind to this problem: two utterances with the same meaning in
   different words score high, the same template carrying different meanings
   scores low. Embedding-based deduplication was evaluated and rejected (§11).
3. **Assistant history is the contagious half; user history is not.** The
   model imitates its own turns and responds to the user's. Every removal here
   is on the assistant side.
4. **The window only shrinks; it is never refilled.** Carried over verbatim
   from the 2026-08-19 spec §2.2.
5. **The current turn and the one before it are never subject to any rule
   here.** Carried over and widened from 2026-08-19 §2.4. The current turn is
   part of the fetched window (`handlers.rs:710`), and the previous exchange is
   the only carrier of reference resolution, so the window count in §4.6 counts
   rows *older than both* — never all rows.
6. **A durable fact belongs to one table.** Character state moves to
   `character_insights`, which is already written every turn; nothing here
   duplicates it into a second store.
7. **Quality is the whole justification.** Cost is not (§3.4).

## 3. Evidence

Measured on one production deployment over 2026-08-26 → 2026-09-02: 2,251
assistant turns across 103 sessions and 23 personas.

Opening definition throughout: split the text on `。！？\n…!?~`, take the first
non-empty trimmed segment, take its first 4 characters
(`repetition.rs:36-48`). `is_repeat` = that opening equals the opening of any
of the previous 6 assistant turns in the same session.

### 3.1 Contagion

By assistant-turn index within the session:

| turn index | 2–5 | 6–10 | 11–20 | 21–50 | 51+ |
|---|---|---|---|---|---|
| `is_repeat` | 4.7% | 18.9% | 22.6% | 34.7% | 35.5% |

Saturates near 35%. Three controls: shuffling openings gives a chance floor
that does **not** rise with index (2.6% → 7.2%); a fixed comparison window of 1
reproduces the rise (2.2% → 22.7%); restricting to the same 35 deep sessions
throughout reproduces it (6.5% → 34.7%).

### 3.2 The carrier

| first-sentence length | share of turns | `is_repeat` | share of all repetition |
|---|---|---|---|
| 1 char | 23.5% | 66.1% | 58.2% |
| ≤2 chars | — | — | 71.5% |
| ≥4 chars | 64.1% | 11.5% | — |

### 3.3 `[avoid_repetition]` is inverted

The block lists over-used openings verbatim; an opening occurring once in the
window is never listed. Comparing, within the same message, reuse of a **named**
opening against reuse of an **unnamed** one: 30.8% vs 9.5% raw, 27.5% vs 3.2%
normalised by distinct-opening count — **8.6×**, p=7.7e-25. Dose response: when
one opening occupies 4 or more of the last 6 turns, the next turn reuses that
named opening 78.9% of the time.

### 3.4 Cost is not a reason

The reductions in §4 remove an estimated **14.5%** of injected characters,
measured across all 1,534 billable `chat_companion` generations in the window
with no extrapolation.

That is not a meaningful saving, for a reason the same measurement makes plain:
**78.4% of input spend sits on one model carrying 31.9% of generations.** Input
cost on this workload is a routing question. Shrinking the history window moves
a percentage of the smaller term; changing which model serves a tier moves the
larger one. Quality is the whole justification for this document (§2.7).

Two measurement notes worth carrying forward, because both cost a revision to
learn:

- **The denominator is per generation, not per turn.** 2,234 assistant turns
  are backed by only 1,534 chat generations: image-only turns raise no chat
  call, retries raise more than one. Extrapolating by turn count overstates
  input cost by 36%.
- **Tokens per character must be fitted, not assumed.** Fitted twice from
  independent token sources — the providers' billing APIs and
  `llm_generations.usage` — the rates agree closely and land near
  0.56 (grok) / 0.70–0.73 (deepseek) / 0.84 (gemma). The 1.3 tokens-per-CJK-
  character figure used in early estimates overstates savings by ~2.2×.

### 3.5 What did *not* hold

- **Turn-level echo cancellation is not needed.** The existing
  message-level rule fires on 10.0% of turns and drops rows that are 94.9%
  user rows, but the resulting concentration is small: assistant share of the
  window rises 50.0% → 55.6%, longest consecutive assistant run 1 → 3 (max 4),
  and **zero** turns in seven days reached ≥80% assistant share or a run ≥5.
  218 of 226 duplicated strings occur exactly twice. Users do not send long
  runs of byte-identical messages.
- **Repetition is not driven by image requests.** Assistant turns
  following a dropped duplicate user row carry an image 21.6% of the time,
  *below* the 27.8% baseline.
- **Genome richness does not predict repetition.** Spearman ρ across four
  denominators — total, total at N≥11, `art_metadata` anchors only,
  `system_prompt` only — lands at +0.033 / +0.071 / +0.027 / +0.066. Bucketing
  by anchor length alone is non-monotonic (61.5% / 14.9% / 29.7%): noise.
  **Genome thickness must not be used as a gate.**

### 3.6 Coverage of the replacement layer

`character_insights` is healthy: 79.4% of active instances have a row, median
7 of 10 fields populated, extraction `parse_error` 0.05%/0.50%, 97.4% updated
within the window.

The gap is shaped by activity, not by persona:

| 7-day assistant turns | has row | median fields |
|---|---|---|
| 1–10 (**55.7% of active instances**) | 63.0% | 3/10 |
| 11–50 | 100% | 6.5/10 |
| 51–200 | 100% | 8.5/10 |
| 200+ | 100% | 10/10 |

Per-field fill among instances that have a row: `current_situation` 87.0%,
`desires` 74.0%, `location` 71.4%, `vulnerabilities` 68.8%, `habits` 59.7%,
`personal_values` 55.8%, `likes` 54.5%, `occupation` 49.4%, `dislikes` 40.3%,
`relationships` 31.2%.

**Fill is chained, not independent.** Over *all* active instances —
missing rows counted as zero, so these are lower than the figures above —
`current_situation` (69.1%) ⊃ `location` (56.7%) ⊃ `occupation` (39.2%) ⊃
`relationships` (24.7%), close to a Guttman scale: every instance carrying
`relationships` carries the other three as well.

**Instance share and traffic share diverge, and only traffic describes what
users experience.** For the four fields §4.5 injects:

| non-empty of the four | instances | % instances | % turns |
|---|---|---|---|
| 0 | 28 | 28.9% | 8.7% |
| 3 | 27 | 27.8% | 41.7% |
| 4 | 17 | 17.5% | 36.4% |

Across all ten fields the median is **5** by instance, while ≥5 / ≥6 / ≥7 / ≥8
non-empty covers **85.0% / 78.5% / 68.7% / 57.3% of turns**. Instances with
nothing at all are 21.6% of instances but only 2.2% of turns; fully-populated
ones are 15.5% of instances and 30.8% of turns. By activity bucket the
all-four rate runs 10.9% → 17.2% → 41.7% → 100%.

The rungs that keep the most history therefore serve the least traffic, which
is what makes the §4.6 fallback cheap.

## 4. The changes

### 4.1 Delete the `[avoid_repetition]` chain

Evidence: §3.3. The mechanism is not merely ineffective; it re-injects the
strings it is trying to suppress and is measurably preferred by the model.

Removed:

- `repetition::overused_openings` and `OPENING_CHARS` / `MAX_OUTPUT`
  (`repetition.rs:25-28`, `:50-77`).
- `ChatRepo::recent_assistant_contents` (`chat.rs:574-597`) and its call site
  (`handlers.rs:811-822`) — one fewer DB round trip per turn.
- The `avoid_patterns` parameter of `build_prompt` (`prompt.rs:527`) and the
  block it renders (`prompt.rs:656-664`).

`opening_of` and `SENTENCE_DELIMS` (`repetition.rs:32-48`) **survive** — §4.3
reuses the sentence split.

### 4.2 Delete the `[recent_conversation]` block

`prompt.rs:767-775` re-renders the last three `(user, assistant)` pairs
verbatim into the system prompt, sourced from `fetch_recent_turn_pairs`
(`handlers.rs:807` → `chat.rs:468-513`). It is assembled inside `build_prompt`
(`handlers.rs:848`), which runs *before* `apply_echo_cancellation`
(`handlers.rs:877`).

Two consequences make it unconditionally wrong under this design:

1. The six most recent messages appear **twice** in every prompt — once here,
   once in the history array — doubling exactly the turns the model already
   over-weights.
2. Anything §4.3 strips from the history array survives intact here, defeating
   the strip.

Removed: the block, the `recent_turns` parameter (`prompt.rs:524`), the fetch,
and `ChatRepo`'s supporting query — a second DB round trip removed per turn.

### 4.3 Strip the leading sentence from assistant rows in injected history

Applied in `model_facing_assistant_text` (`handlers.rs:118-142`) or as a step
immediately after it, so it lands on the exact string the provider receives and
before `cancel_echo` keys on it.

**Rule.** For each `assistant` row in the injected window, split on the
delimiter set, drop the first non-empty trimmed segment and its trailing
delimiter, and inject the remainder trimmed. `user` and `gift_user` rows are
untouched.

**Empty result.** If nothing remains after stripping, **drop the row**. This
must be explicit: `cancel_echo` passes empty strings through unchanged
(`repetition.rs:112`, `:126`) and `assemble_chat_request` copies `m.text`
straight into `ChatMessage.content` (`handlers.rs:302-307`), so an emptied row
would otherwise reach the provider as `{"role":"assistant","content":""}`,
which some providers reject. Expected frequency: 4.0% of turns overall, 11.3%
within the ≤40-character bucket.

**Cost.** Median first sentence is 17.6% of the message, and the segments
removed are disproportionately the single-character interjections that carry
58.2% of measured repetition (§3.2). The strip is cheap and well-targeted.

**Delimiter set: the existing `SENTENCE_DELIMS`, unchanged.** `。！？\n…!?~`
(`repetition.rs:32`), reused verbatim from `opening_of` so detection and
removal cannot drift apart.

A narrower set was weighed — `\n`, `。`, `...`/`…`, plus whitespace — and
rejected. Adding whitespace is near-inert in CJK prose, firing only on
mixed-script text and space-separated stage directions. Dropping `！？~` is the
substantive difference: it makes the strip *more* aggressive, because a
`？`-terminated opening no longer ends a segment. `怎么了？我在呢。` splits at
`？` under the existing set and injects `我在呢。`; under the narrower set the
first segment is the whole message and the row is dropped outright.

**That extra aggression is not itself an objection.** `！？~`-terminated short
sentences are frequent in companion text, and §3.2 says they are largely the
interjections carrying the repetition. Nothing is preserved by keeping them:
the model regenerates such openings unprompted, so their presence in the
injected history supplies no capability, only reinforcement. Deleting more of
them would be fine.

The narrower set is rejected because **the cut it makes is not principled**.
Whether a row survives comes down to which terminator the model happened to
use: `怎么了？我在呢。` loses the entire row, `怎么了。我在呢。` keeps
`我在呢。` — same content, opposite outcome, decided by one character whose
distribution varies across the model rotation and across personas. A rule that
behaves differently for `deepseek` than for `grok` on identical content is not
a rule we can reason about from the read-out in §8. The existing set cuts at
the first sentence boundary whatever the terminator is, so it reads the same
everywhere.

If a more aggressive strip is wanted later, the principled lever is removing a
second sentence or imposing a character budget on the injected assistant row —
not narrowing the delimiter set, which buys aggression and pays for it in
determinism.

### 4.4 `[emotional_context]` 5 → 1

`handlers.rs:827` passes 5 to `AffinityRepo::recent_emotional_reasons`; change
to 1. Two dependents:

- `emotional_context.reverse()` (`handlers.rs:837`) becomes a no-op and should
  go with it.
- The block header reads `（最近几轮的情感走向…）` (`prompt.rs:676`) and must
  be reworded for a single row.

Rationale: four prompt blocks already describe affect (`[mood]`, `[feelings]`,
`[inner_state]`, `[emotional_context]`). Multiple descriptions of one thing
read as a pattern, not as emphasis.

### 4.5 Inject `character_insights`

A new volatile block carrying the character's relationship-scoped state, read
by primary key via `CharacterInsightRepo::load` (`character_insight.rs:200`) on
`instance_id`. One PK read, no new table, no migration.

**Fields injected:** `current_situation`, `occupation`, `location`,
`relationships`. Fill rates in §3.6 — `occupation` and `relationships` are
under half even among populated rows, so the block must render whatever subset
is present and omit itself entirely when none is.

**Fields deliberately not injected, and why:**

- `habits`, `personal_values` — these are facets of *who she is*, whose source
  of truth is `persona_genomes`. Migration 0047 excluded `appearance` /
  `background` / `personality_traits` for exactly this reason: an extractor
  that only ever sees turn text can produce nothing but
  paraphrase-with-embellishment, and "the drift reads back as fact". That
  reasoning applies unchanged to these two; they escaped the exclusion list
  only because their names read like facts.
- `desires`, `vulnerabilities` — overlap the four affect blocks named in §4.4,
  and are inputs to the PDE judge rather than to narration.
- `likes`, `dislikes` — real but low-frequency value. Withheld initially; add
  later if their absence is felt.

**Second-order drift, and the guard.** Once this table is read back into the
prompt, a new loop exists: the extractor embellishes, the embellishment is
injected, the model treats it as fact, the next extraction confirms it. The
loop is bounded by construction — identity fields are excluded, so drift can
only reach facts (occupation, location, situation), and a wrong fact is
correctable in a way a drifted personality is not. Two rules keep it bounded:
the three excluded columns are never added, and the block is titled as
*what has happened in this relationship*, never as character definition, so it
cannot compete with the genome for authority.

**Mechanics.**

- Placement: after the stable cache prefix `{head}{PERSONA_GUARD}
  {ANTI_REFUSAL_GUARD}` (`prompt.rs:555-570`), with the other volatile blocks.
- Naming: the identifier `relationship_facts` is taken — it is an existing
  `build_prompt` parameter rendering `[shared_memories]` (`prompt.rs:515`).
- Emptiness of `TEXT[]` columns must be tested with `cardinality(col) = 0`.
  `array_length(col, 1)` returns NULL for an empty array, not 0, and silently
  drops rows from any comparison.

### 4.6 History window becomes a function of `character_insights` fill

The window size is derived per turn from how completely the character's
insights row is populated: a well-described character needs little or no
transcript, a barely-described one needs more. This makes §3.6's low-activity
gap self-correcting without a special case, and it is the one place in this
design where a persona-level property changes behaviour.

**Protected rows — outside the count entirely.** The window size counts rows
older than *the current turn and the turn before it*. Always injected,
regardless of window size:

- the current user message (already exempt under 2026-08-19 §4.3);
- the previous turn's user message and assistant reply.

Three rows in the steady state. `character_insights` is a state snapshot read
by primary key, lagging one turn behind (extraction runs in `post_process`,
after the reply is served), so it can carry *what is true of her* but never
*what was just said*. Reference resolution — 「你刚说的那个」, 「你答应我等下
告诉我的」 — has no other carrier. The protected pair costs 40–100 characters
per assistant row before §4.3 strips it.

The protected assistant row is still subject to §4.3's strip and to
`cancel_echo`; protection governs *selection*, not content. If §4.3 empties it,
§4.3's drop rule applies — the protection does not resurrect an empty row.

**Gate — count of non-empty fields across all ten.** Over `location`,
`occupation`, `current_situation`, `desires`, `vulnerabilities`, `habits`,
`personal_values`, `likes`, `dislikes`, `relationships`. A missing
`character_insights` row counts as zero. TEXT columns are empty when NULL or
blank; TEXT[] columns are empty when `cardinality(col) = 0` (§4.5).

The four fields §4.5 injects and the ten fields counted here are deliberately
different sets. Injection asks *is this useful in the prompt*; the gate asks
*how well does the engine know this character at all*, and for that question
every extracted field is evidence, including the six not injected.

**Ladder.**

| non-empty fields | prior rows beyond the protected pair | total injected rows |
|---|---|---|
| 7–10 | 0 | 3 |
| 6 | 2 | 5 |
| 5 | 4 | 7 |
| 4 | 6 | 9 |
| 3 | 8 | 11 |
| 2 | 10 | 13 |
| 1 | 12 | 15 |
| 0 | 14 | 17 |

Knee at 7, step of 2, measured against §3.6. A field count of ≥7 covers **68.7%
of turns**, so roughly two turns in three land on the thinnest rung. Instances
holding nothing at all — 21.6% of instances but **2.2% of turns** — land at 17
rows, near today's 20. The change is close to a no-op for a brand-new
relationship and maximal for an established one, and the expensive rungs serve
the least traffic.

The two medians disagree and the traffic-weighted one governs: the median
*instance* holds 5 non-empty fields, but weighted by *turns* the median sits
above 7, because the relationships with the fullest insight rows are the ones
carrying the traffic. A ladder calibrated on instance counts would sit far too
high.

**Invariants.**

- `HISTORY_WINDOW = 20` (`handlers.rs:62`) remains the fetch size. The ladder
  selects from what was fetched; it never widens the query.
- The driving-row pin (`handlers.rs:718-725`) still applies. On the async
  worker path a driving row outside the fetched window is re-fetched and
  inserted, and that insertion is exempt from every rule in this document.
- Selection is by recency over the rows that survive `model_facing_history`
  (`handlers.rs:236-256`), so channel rows and unknown roles are already gone
  before the count applies.

### 4.7 Unchanged

`cancel_echo` (`repetition.rs:108-150`) stays exactly as it is, including its
role-agnostic byte-exact key and its drop-every-occurrence behaviour. §3.5
shows its side effect on window composition is real but small, and the
turn-level variant considered during design is not justified by the data.

## 5. Decisions taken

Two questions gated §4.6. Both are settled; the alternatives are recorded
because the reasoning constrains future changes to the ladder.

### D1 — the thinnest rung keeps the previous turn

**Decided: the count excludes the current turn and the one before it (§4.6).**

Strict zero — system prompt plus the current user message and nothing else —
was considered and rejected. `character_insights` lags one turn and is a state
snapshot, so it cannot answer 「你刚说的那个」; the failure mode is a
user-visible non-sequitur, and the cost of avoiding it is one stripped
assistant row. Protecting the whole previous turn rather than only its
assistant half keeps the exchange coherent as a pair, which matters once §4.3
has shortened the assistant side.

### D2 — the gate counts all ten fields, not the four injected ones

**Decided: option 2 in the list below (§4.6).**

The original proposal was: all four injected fields present → 0, each missing
→ +3. The joint distribution was measured and the result is not the one either
argument for it predicted.

The concern that the gate would rarely fire is **wrong**, and by a wide margin
in the useful direction: fill is chained (§3.6), so all four present is 17.5%
of instances against 3.8% under independence — 4.6× — and 36.4% of *turns*.

The gate is nonetheless the wrong instrument, for a reason the joint
distribution makes plain: **the four-field AND collapses to a single-field
gate.** The 17 instances holding all four are exactly the 17 holding
`relationships`, the rarest field at 24.7%. Because fill is a Guttman chain,
every field above it is implied, so the other three contribute nothing to the
decision. A threshold that in practice reads one field is one unlucky
extraction away from moving a relationship a full rung.

The signal is also mismatched. An empty `relationships` means the character has
never mentioned another person — not that the engine does not know her.

Weighed:

1. Gate on `current_situation` + `location` (87.0% / 71.4%). Simple, but a
   ceiling of two rungs is too coarse and 6 rows is thin for a new
   relationship.
2. **Gate on the count of non-empty fields across all ten.** Chosen. Smooth,
   built on a quantity already measured (§3.6), and no single field's absence
   moves a whole rung.
3. Four-field AND with `relationships` swapped for `desires` (74.0%). Raises
   the hit rate but keeps a threshold that a single unlucky extraction can
   flip.

One methodological note worth carrying forward, because it reversed the
recommendation: instance counts and turn-weighted counts disagree so sharply
here (17.5% vs 36.4% for all-four; 21.6% vs 2.2% for wholly empty) that a gate
designed against instance counts alone would have been calibrated backwards.
Any future change to this ladder must be read against both.

## 6. Where it lives

| change | file | current anchor |
|---|---|---|
| §4.1 delete | `repetition.rs` | `:25-28`, `:50-77` |
| §4.1 delete | `chat.rs` | `:574-597` |
| §4.1 delete | `handlers.rs` | `:811-822` |
| §4.1 delete | `prompt.rs` | `:527`, `:656-664` |
| §4.2 delete | `prompt.rs` | `:524`, `:767-775` |
| §4.2 delete | `handlers.rs` | `:807` |
| §4.2 delete | `chat.rs` | `:468-513` |
| §4.3 strip | `handlers.rs` | `:118-142`, `:236-256` |
| §4.4 constant | `handlers.rs` | `:827`, `:837` |
| §4.4 copy | `prompt.rs` | `:676` |
| §4.5 new block | `prompt.rs` | after `:570` |
| §4.5 new read | `handlers.rs` | near `:781-784` |
| §4.6 window | `handlers.rs` | `:62`, `:710` |

Net DB round trips per turn: −2 (§4.1, §4.2), +1 (§4.5). One fewer than today.

## 7. Configuration

One new env flag gating §4.3 and §4.6 together, following the precedent of
`CHAT_ECHO_CANCELLATION_DISABLED` (`state.rs:380-385`, `:438`). Production has
no gradual rollout and no hot config path — configuration ships with the image
and reaches 100% of users at once — so a flag that restores the previous
injection shape without a rebuild is the rollback path.

§4.1, §4.2, §4.4 are deletions and get no flag; their rollback is the previous
image.

## 8. Observability

No new table and no new persisted column. Every reading this change needs is
recomputable from `chat_messages` after the fact, which is how §3 was produced:
re-running the §3.1 and §3.2 measurements over a post-deploy window is the
before/after comparison, using the same session-indexed bucketing.

One `tracing::info!` per turn where §4.3 dropped a row or §4.6 selected a
non-default window size, carrying counts and `session_id` only — no content,
no content hash, matching `apply_echo_cancellation` (`handlers.rs:274-283`).

## 9. Testing

- `opening_of` retained and its existing tests retained.
- New pure-function tests for the strip: multi-sentence row, single-sentence
  row (drops), whitespace-only remainder (drops), delimiter-only row, row with
  a `[你的照片：…]` marker, CJK char-boundary safety on a long delimiter-free
  row.
- `model_facing_history` test asserting no empty-content row ever reaches
  `assemble_chat_request`.
- Ladder tests: field-count 0 / 3 / 6 / 7 / 10 mapping to the §4.6 table; a
  missing `character_insights` row counting as 0; an empty `TEXT[]` counting as
  empty (the `cardinality` trap); the protected pair surviving at every rung
  including 7–10; a session shorter than the protected pair; and the
  driving-row pin taking effect at the thinnest rung.
- Existing `cancel_echo` tests unchanged — §4.7 does not touch it.
- Regression: the deletions in §4.1/§4.2 remove `build_prompt` parameters, so
  call-form changes in existing prompt tests are expected. Assertions on
  prompt content must not be weakened to accommodate them.

## 10. Non-goals

- Stopping models from repeating. Not achievable; carried from 2026-08-19 §3.2.
- Reducing input cost. §3.4 — the saving is a small percentage of the smaller
  term, and the lever is model routing, not history size.
- Changing user-side history. The user repeating a topic is signal, and §3.5
  shows it is not the mechanism here.
- Changing `max_tokens`. It governs output; nothing here touches output length.
- Any change to `persona_genomes`. If thin genomes need richer style anchors
  that is web's work, and §3.5 shows it is not this problem.
- **Any change to `[iron_rules]`** (`prompt.rs:796-805`). The block is the
  largest fixed component of the system prompt and an obvious next candidate
  once §4's removals land, but it governs what the persona may say rather than
  how the engine assembles history, so it is a different question with a
  different failure mode. **Deferred to a follow-up; explicitly out of scope
  here.** Changing it in the same release would also confound §8's before/after
  read-out, since both would move the same numbers.

## 11. Rejected alternatives

**Per-message embedding with similarity-based deduplication.** Evaluated and
rejected on three grounds. Semantic similarity is blind to the actual defect:
embeddings normalise away surface form, which is precisely what is being
removed here — two phrasings of one meaning score high, one template carrying
two meanings scores low. Assistant text is deliberately unembedded anywhere in
the system (issue #113: storing it fed the model's own prose back through
recall and collapsed replies), so this would be net-new Voyage cost on the side
we most want to remove. And assistant replies run 40–100 characters, a length
at which cosine similarity between two distinct short utterances is routinely
high enough to make any threshold unstable across personas.

**Turn-level echo cancellation.** Considered to prevent the message-level rule
from concentrating assistant text after dropping user duplicates. §3.5 measured
the concentration: small, with zero extreme cases in seven days. Not justified.

**Keeping `[avoid_repetition]` with reworded copy.** §3.3 shows the failure is
structural — the block names the strings — not a wording problem. Prior
attempts at rewording (#280–#282) already failed in production.

**Gating the window on genome richness.** §3.5: no correlation under four
denominators.

## 12. Risks

**The strip teaches malformed output.** History rows that begin mid-utterance
may be imitated as a style — replies that open abruptly — rather than read as
"do not open this way". This cannot be settled by reasoning; §8's post-deploy
re-run is the check. Substituting a placeholder marker instead of deleting is
*not* the mitigation: markers get imitated too, which is why #281 exists.

**Second-order drift through `character_insights`.** §4.5 states the loop and
its bound.

**Low-activity relationships lose the most.** 55.7% of active instances sit in
the 1–10 turn bucket with 63.0% row coverage and a 3/10 median. §4.6 exists to
handle exactly this: at 3/10 the ladder returns 11 injected rows, and at 0/10
it returns 17 — close to today's 20. The measurement sizes the exposure: instances holding
no populated fields are 21.6% of instances but 2.2% of turns, so the generous
rungs are cheap. The residual risk is that a relationship crosses a rung
mid-conversation and the window shortens under it; the step of 2 keeps any
single crossing small.

**Model attribution for the post-deploy read-out.** `chat_messages.model` and
`.usage` were dropped by `0061_drop_child_model_usage.sql`; the authoritative
source is now `engine.llm_generations`, joined on `generation_id`, which also
carries `task` and an index on `(task, created_at DESC)`. Any before/after
analysis must join that table rather than expect the old columns.

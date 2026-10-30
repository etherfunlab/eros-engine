# Gives move needs, two romantic life choices, and answer receipts — Design

- **Date:** 2026-10-30
- **Status:** Draft — ready for review
- **Type:** Engine change, three independent parts shipped as three PRs in
  order (A → B → C), then a docs PR. One migration (part C). Additive HTTP
  changes only.
- **Owner:** enriquephl (sole dev)
- **Builds on:**
  - `2026-10-13-persona-needs-design.md` (*the needs spec*)
  - `2026-10-28-outing-indulgence-needs-and-habits-design.md` (*the habits
    spec*: the give roll, `indulgence`)
  - `2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md`
    (*the v1 choices spec*, §3)
  - issue #480

## 1. Intent

- **A.** An accepted consumable should change how she feels right away. A
  meal makes her less hungry, a drink loosens her up. Today only the judge
  can move needs, and it re-rates freely each turn.
- **B.** A client that reloads, or a second device, can't tell how the user
  answered one of her choices. The thread reads as though she said goodbye,
  or was hurt, for no reason (#480).
- **C.** Life choices so far only have the bad branch (ask for space). The
  relationship gets two good ones: she asks to sleep with the user, and she
  proposes. Each fires only for a pair the caller marks as romantically
  compatible. Relationship status ("together", "married") is deliberately
  **not** modelled. The user and the persona settle it in conversation.

## 2. Part A — accepted gives move needs

### 2.1 The rule

On a 1:1 text turn whose `give` is **accepted**, the engine moves three
needs by one level each:

| `GivenItem` | hunger | social | indulgence |
|---|---|---|---|
| `food` | −1 | | |
| `cigarette`, `alcohol`, `herb` | −1 | +1 | +1 |
| `medicine`, `coffee` | −1 | −1 | −1 |
| every other item | | | |

−1 is one level toward `low` and +1 is one level toward `high`, clamped at
the ends. The quantity is ignored: one give moves one level.

*Accepted* means the turn's effective response is accept. A judge refusal,
or the habit roll forcing a refusal (`，这次你不会收下`, habits spec), moves
nothing. A fail-open turn (the judge failed) accepts the give under the
existing rule, so the move applies there too.

### 2.2 What gets written

- **Baseline.** The baseline is the levels the judge was shown: the newest
  rating before this user row, carried to its `sent_at` (`prior_needs`).
- **Moved needs.** For each need the item moves, the stored level is the
  baseline moved by one. The judge's rating of that need this turn is
  discarded. The judge sees the give in `[用户动作]` and often lowers hunger
  itself; shifting on top of its rating would count the meal twice.
- **Other needs.** The other needs keep today's rule: the judge's rating
  (resolved against the baseline as now). If the judge abstained or failed,
  they keep the baseline.
- **No baseline.** When no rating exists yet, the judge's rating is written
  unshifted. With no judge rating either, nothing is written. The engine
  never invents levels from nothing.
- **One write.** The result is written with the existing `set_needs`, once.
  On give turns it now also writes when the judge abstained, because the
  engine has its own reason to rate.
- **Readers.** The reply, the `body_state` frame and `GET …/effects` read the
  written levels as they already do. The judge's audit keeps what the judge
  said. No new audit key.

Voice and group turns write no needs today. That is unchanged.

### 2.3 Not changed

The judge prompt does not change, and neither does the give roll or the
drift. Life events still move needs only through the judge (the habits
spec's ruling stands). This rule covers gives only.

## 3. Part B — answer receipts in history (#480)

In both history routes (`/comp/chat/{session_id}/history` and the BFF history
and start routes), the row that a `life_choices.message_id` points at (her
ask) gains one optional field:

```json
"life_choice": {
  "id": "…",
  "kind": "ask_for_space",
  "status": "accepted",
  "resolved_at": "…"
}
```

- **`status`.** One of `pending | accepted | declined | ignored | expired`.
  `expired` is derived: a `pending` choice whose `asked_at + 24 h` is past.
  It is never stored.
- **`resolved_at`.** `null` unless the choice was resolved.
- **Where it comes from.** The field is derived at serve time with one
  lookup per page on `life_choices.message_id` (already indexed). It is
  omitted on every other row.
- **What clients do with it.** A client draws the answer between her ask
  and her reply. The reply rows carry `occasion: "life_choice"` as today.

The issue's other shape, a user-side receipt row written at resolution, is
rejected. It would store the answer twice. `life_choices` stays its only
home.

The delta's `life_choice` field is unchanged and still pending-only.

## 4. Part C — two romantic choices

### 4.1 Romance compatibility (new core criterion)

`eros-engine-core::scope` gains a single criterion. The engine uses it
wherever it needs to know whether a pair is romantically compatible:

```rust
/// Whether the caller's scopes mark this pair as romantically compatible:
/// relationship memory is on, and the affinity scope carries at least one
/// 暧昧感 axis (trust / intrigue / patience).
pub fn romance_compatible(memory: MemoryScope, affinity: AffinityScope) -> bool
```

Compatible iff `memory.resolve()` turns relationship memory on (`full`,
`neutral_and_relationship`, `relationship_only`) **and**
`affinity.any_chemistry_axis()`. Everything else is **incompatible**. That
includes the engine defaults (`neutral_and_relationship` + `bond`) and a
missing or unparseable value.

**Where the criterion came from.** It was set once, when the engine first
needed it, from a web client's scope table:

| `memory_scope` | `affinity_scope` | web client says | criterion |
|---|---|---|---|
| `full` | `chemistry` | compatible | compatible |
| `neutral_and_relationship` | `full` | compatible | compatible |
| `neutral_only` | `bond` | not | not |
| `neutral_and_relationship` | `bond` | not | not |
| `neutral_only` | `full` | unknown | not |
| `relationship_only` | `full` | (never sent) | compatible |

**Relation to that table.** The criterion is the engine's own table. It
must stay a superset of that client's compatible set, so every combination
that client marks compatible stays compatible here. It need not follow
later changes to that client's table. The rows above are the regression
cases.

The engine never receives or stores the user's gender or orientation. It
never decides who is attracted to whom, and the persona has no orientation
key. The caller states compatibility through the scopes it already sends,
and the engine only reads the result.

**Input.** The input is the resolved `memory_scope` and `affinity_scope`
recorded on the session's newest assistant row that carries both keys (the
`build_metadata` bag), skipping rows whose `metadata.proactive` is
`life_choice`: rows her own choice flow writes are not a turn of the caller's.
If no such row exists, or a value fails to parse,
the pair is incompatible.

**Public contract.** `docs/api-reference.md` documents the criterion as the
contract: a caller makes a pair eligible for romantic choices by sending
scopes that meet it.

### 4.2 Kinds

| kind | 中文 | options |
|---|---|---|
| `sleep_together` | 发生关系 | `accept`, `decline` |
| `propose` | 求婚 | `accept`, `decline` |

### 4.3 Triggers

These are checked in the same post-commit hook as `ask_for_space`, after
every judged 1:1 text turn. The tiers come from the committed, time-decayed
state, as in v1. A *judged turn* has v1's definition (a `message` affinity
event with a `generation_id` and no `eval_skip_reason`). Warmth and patience
are read from each event's `state_after`.

**Shared conditions** (from v1, §3.1):

- a genome instance's 1:1 text session;
- clear runway: no pending, un-expired choice anywhere on the instance, and
  no `taking_space` under way;
- only judged turns after the session's streak anchor count (the newest
  choice of any kind, unchanged);
- `romance_compatible` holds (§4.1).

**`sleep_together`:**

- `chem_tier ≥ 3`;
- the 3 newest judged turns after the anchor all have warmth > 0.7 and
  patience > 0.7;
- no `accepted` or `ignored` `sleep_together` in this session, ever.

**`propose`:**

- `chem_tier ≥ 4` and `bond_tier ≥ 4`;
- among the newest ≤ 8 judged turns after the anchor, at least 5 have
  warmth > 0.7 and patience > 0.7;
- no `accepted` or `ignored` `propose` in this session, ever.

**Thresholds.** 0.7 is an engine rule threshold, like v1's 0.5. Warmth above
0.7 needs judge warmth level 3 and a chemistry score above about 0.415.
Patience above 0.7 needs judge patience level 3 and a bond score above
about 0.415. So `sleep_together` in practice also needs bond in mid tier 3.
That follows from the endpoint formula, not from a separate rule.

**Both qualify.** When both kinds qualify on one turn, `propose` fires. They
can't collide with `ask_for_space`, which needs patience < 0.5.

### 4.4 The ask and delivery

The ask works as in v1 §3.2. Her words come first, through the proactive
path, as an ordinary assistant row with `metadata.proactive = 'life_choice'`.
The choice row follows, written under the instance's advisory lock with the
same re-checks. A blank or failed generation writes nothing. Delivery works
as in v1 §3.3: the same `life_choice` field on the affinity delta, with the
new `kind` values.

Her ask is generated with the engine's default scopes, as v1's ask is: no
intimate profile fields, bond axes only. Her replies at resolution use the
resolution request's own scopes, as v1's replies do. The pair's recorded
scopes (§4.1) decide eligibility only.

**Stage cues.** These are drafts and the wording is the owner's to adjust:

| | cue |
|---|---|
| `sleep_together` ask | （你们最近越来越亲密，你很想和对方更进一步。你决定主动开口，问对方愿不愿意和你上床，希望对方给你一个答复。） |
| `sleep_together` accept | （对方答应了你的邀请。） |
| `sleep_together` decline | （对方拒绝了你的邀请，你有点失落。） |
| `propose` ask | （你越来越确定想和对方共度一生。你决定郑重地向对方求婚，希望对方给你一个答复。） |
| `propose` accept | （对方答应了你的求婚。） |
| `propose` decline | （对方拒绝了你的求婚，你心里很难受。） |

### 4.5 Resolution

It uses the same endpoint, `POST /v2/comp/session/{session_id}/life-choices/{choice_id}`,
with the same 409 rules and the same response shape. `event` is always
`null` for these kinds.

- **accept.** Status becomes `accepted`. There is no event row, no tier
  change and no slowdown. Her reply is generated with the accept cue and
  returned.
- **decline.** Status becomes `declined`. In the same transaction:
  - `sleep_together`: the **chemistry** line drops one tier (v1's landing
    rule) and its pending deltas are cleared.
  - `propose`: **both** lines drop one tier, exactly as v1.

  The drop is recorded as an affinity event with
  `event_type = 'life_choice'`, `context.life_choice = "decline"` and
  `context.choice_id`, as in v1. Her hurt reply is generated and returned.
- **ignore.** The first ordinary text message after the ask, while the
  choice is pending, sets status to `ignored`. There is **no `fell_out`**
  and no cost. The turn runs normally and the judge reads whatever the user
  wrote. An answer in words is a real answer, and a client without the
  choice UI is never penalized. Action turns resolve nothing, as in v1.
- **expiry.** The choice lapses after 24 h at no cost, as in v1.

### 4.6 Repeats

- **After an accept or an ignore.** The kind never fires again in that
  session. A typed answer is an answer: a user on a client without the
  choice UI is asked once, not every three messages.
- **After a decline or an expiry.** The kind re-arms once the session has a
  fresh streak after the anchor. A decline also drops a tier, which may or
  may not close the tier gate; the streak is the real cost.

### 4.7 v1 code that has to learn about kinds

- `growth_slowed` reads the newest **`ask_for_space`** accepted choice. Today
  it reads any accepted choice, so an accepted `propose` would cancel a
  running slowdown.
- `convert_ignored_ask` writes `fell_out` only for `ask_for_space`. The new
  kinds go to `ignored` alone.
- The events route's "draw nothing while a choice is pending" already
  applies to every kind. No change.
- The story director's `life_choices` input stays `ask_for_space`-only
  (its query already filters on kind). The romantic choices leave no trace
  in her world story, insight or digest: their outcome lives in the
  conversation only.
- The decline drop is per kind (§4.5). v1's accept-side separation and
  return wording stay `ask_for_space`-only.

### 4.8 Storage — migration `0083`

```sql
ALTER TABLE engine.life_choices ALTER COLUMN tier_gap DROP NOT NULL;
ALTER TABLE engine.life_choices
  ADD CONSTRAINT life_choices_tier_gap_kind
  CHECK ((kind = 'ask_for_space') = (tier_gap IS NOT NULL));
```

`tier_gap` means nothing to the new kinds, so they have no value there.
Existing rows are all `ask_for_space` with a gap, so the check validates.
`kind` stays free text, with no CHECK.

## 5. Goals, non-goals, anti-goals

**Goals:**

- (A) a deterministic needs move for six consumables;
- (B) a resolved answer readable on every client;
- (C) two romantic choices, gated by the caller's compatibility.

**Non-goals:**

- relationship status (together, engaged, married);
- persona orientation;
- any user gender or orientation field in the engine;
- accept-side rewards;
- voice and group sessions;
- soft drink and tea moving needs (only the six items in v1);
- an accepted `sleep_together` counting toward her `ever_had_sex` for life
  events. Choices and her own life-event draw stay separate.

**Anti-goals** (failure even if it works):

- a romantic ask reaching a pair the caller did not mark compatible;
- the engine storing who the user is attracted to;
- a client without the choice UI being penalized for answering in words;
- a give counting twice into hunger.

## 6. Delivery

- **PRs.** The three PRs ship in order, one open at a time:
  - **A:** needs;
  - **B:** #480, whose squash commit carries `Fixes #480`;
  - **C:** romantic choices plus migration `0083`.

  Docs follow as their own PR.
- **Docs.**
  - `docs/life-events-needs.md` (+ `.zh`): §2.
  - `docs/api-reference.md` (+ `.zh`): §3, the new kinds, the
    compatibility contract.
  - `docs/affinity-model.md` (+ `.zh`, `.ja`): the per-kind decline drop.
  - `docs/model-config.md` (+ `.zh`): the cues, if cues are documented there
    as v1's are.
- **Rollout.** C is live for compatible pairs as soon as a deployment runs
  it. A client without the choice UI sees her ask as an ordinary proactive
  message, and a typed answer resolves it as `ignored` at no cost.

## 7. Decisions made by the owner (do not reopen)

- **Moves.** The six items and their moves are as in §2.1.
- **Kinds.** Two kinds only. No "together" or "married" milestones.
- **Gates.**
  - Tiers count on reaching the tier ("过 3" means tier ≥ 3).
  - `sleep_together` needs chem ≥ 3 and 3 consecutive judged turns with
    warmth and patience > 0.7.
  - `propose` needs chem and bond ≥ 4 and at least 5 of 8 judged turns with
    both > 0.7.
- **Decline costs.** Only a decline costs anything. A declined
  `sleep_together` drops chemistry one tier; a declined `propose` drops both
  lines. An accept changes nothing.
- **No repeat.** An accepted **or ignored** `sleep_together` or `propose`
  never fires again in the session. Declined and expired re-arm.
- **Story director.** It does not read the romantic kinds.
- **Generation scopes.** As v1: her ask uses the engine defaults, her replies use the resolution request's scopes.
- **Compatibility.** It is judged only from the two scopes the caller sent.
  The criterion is general: relationship memory is on and the affinity
  scope has a 暧昧感 axis (§4.1). Not found means incompatible. The engine
  never takes or stores the user's gender or orientation.
- **Relation to the web client's table.** It is a one-time borrowing, not a
  tie. The engine's table must stay a superset of that client's compatible
  set and need not follow it later.

## 8. Calls made in this design (check while reading)

- **Needs baseline.** The move counts from the pre-turn level and overrides
  the judge on the moved needs (§2.2). Quantity is ignored, and nothing is
  written without a baseline.
- **#480 shape.** It is an annotation derived on her ask row and covers all
  five statuses (§3).
- **Kind names.** `sleep_together` and `propose`.
- **Same-turn tie.** `propose` wins.
- **Ignoring a romantic choice.** It costs nothing and writes no `fell_out`.
- **Compatibility source.** It is the newest assistant row recording both
  scopes, skipping rows whose `metadata.proactive` is `life_choice`.
- **`ever_had_sex`.** The new kinds don't touch it.
- **The six cues.** They are drafts (§4.4).

## 9. Left to the builder

- **Accepted-give signal.** How the turn's effective accept is carried to the
  needs write site (the verdict's `action_response` plus the habit roll).
- **Query shapes.** For the 3- and 8-turn windows (generalize
  `last_judged_patiences` to return warmth too), and for the "ever accepted
  in session" check.
- **Code layout.** For per-kind cues and decline costs inside
  `life_choice.rs` and `routes/life_choices.rs`.
- **Tests.** Per part, following the existing test style in each module.

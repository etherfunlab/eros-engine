# Affinity feeling clause — LLM-written [feelings], slimmed [mood] — Design

- **Date:** 2026-09-03
- **Status:** Draft — ready for review
- **Type:** Engine change. One migration (two nullable columns on
  `engine.companion_affinity`), one new LLM task, one new post-process step,
  one prompt-block rewrite and one prompt-block slimming. Public API
  unchanged.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the text chat turn's system prompt and its
  post-process pipeline. Release number and timing are the owner's call, not
  this document's.

## 1. Motivation

The `[feelings]` block still injects the six affinity axes as raw floats
(`warmth=0.42, trust=0.31, …` — `prompt.rs` `build_prompt`, the `state`
binding). Raw numbers are the one rendering the affinity 4.0 line of work
ruled out everywhere else: models neither read nor emit calibrated floats
(`axis_band_label`'s doc comment; the eval judge is shown bands for exactly
this reason). The numbers spend prompt tokens re-anchoring the reply model on
arithmetic it cannot do.

The block also cannot use the two fixes that worked elsewhere. There is no
multiple-choice protocol to run — `[feelings]` is not asking the model to
judge anything — and the tier-to-directive fold already exists as `[mood]`;
rendering tiers twice would be a second voice on the same channel.

What the channel is missing is narrative: a short first-person statement of
how the character currently feels about this person, with the texture the
tier fold cannot express (combinations of axes, trajectory, the reasons
behind the state). An LLM summarizer writes that; the engine stores it and
injects it. This stays inside the #332 three-channel contract — the clause is
a judged conclusion of the affinity side, produced off-turn and injected,
never a standing rule.

## 2. Locked decisions

Three decisions were made before this document and are its premises:

1. **Trigger:** summarize only on turns with real affinity movement,
   piggybacked on `post_process` — not every turn, not a background sweeper.
2. **Summarizer input:** six-axis band labels plus recent event reasons. No
   raw floats, no previous clause (the summarizer is stateless, like the
   judge's endpoint reads), no conversation excerpt.
3. **Disposition of existing blocks:** the clause replaces `[feelings]`
   entirely (no raw-number fallback), and `[mood]` slims down to hard
   behavioral gates. `[emotional_context]`, `[reply_length]`, and
   `TurnNudges` are untouched.

## 3. Data model

Migration `0062` adds to `engine.companion_affinity`:

```sql
ALTER TABLE engine.companion_affinity
    ADD COLUMN feeling_clause     TEXT        NULL,
    ADD COLUMN feeling_clause_at  TIMESTAMPTZ NULL;
```

- One session, one clause: `companion_affinity` is the session-keyed
  affinity state row, so the clause rides it as derived state with a single
  authoritative carrier. No new table, no history — the trajectory already
  lives in `companion_affinity_events`, and every injected clause is
  replayable from the prompt log.
- `feeling_clause_at` carries the `updated_at` of the affinity state the
  clause was derived from, and the write is guarded on it (`<=`), so two
  overlapping summaries resolve to the one derived from the newer state
  regardless of write order. Observability only — not used for staleness
  scheduling.
- No new FKs, no new external-identity columns. Both columns are nullable:
  NULL means "never summarized", and the prompt renders nothing.

The `Affinity` struct in `eros-engine-core` gains the two fields; the store
row mapping follows.

## 4. Trigger

After `persist_affinity` commits in `post_process`, the turn is a
**movement turn** iff any line-axis grade (trust / intrigue / intimacy /
tension) is `>= 1` in either direction, or an endpoint (warmth / patience)
level read is `!= 2`.

Only movement turns invoke the summarizer. The predicate is purely ordinal —
it reads the judge's already-produced grades and levels, never compares
floats against band edges. In-turn band crossings always come with a
`grade >= 1`, so nothing is missed; silent decay drift between sessions does
not re-trigger, and the clause lags until the next real movement — accepted,
the clause is narrative state, and `[mood]`'s live gates keep reading current
floats.

Ghost turns are not a trigger: they never reach `post_process`, and the
summarizer's inputs carry no ghost signal — a ghost's fallout enters the
clause via the next evaluated turn (its grades, and the decayed bands).

The summarizer runs after the affinity write, inside the same post-process
task, attributed to the turn's real user (it is request-scoped work, not a
sweeper — the dreaming `SYSTEM_AUDIT_USER` sentinel does not apply). Its LLM
call is audited by the standard `llm_generations` path like any other call;
no extra audit columns.

## 5. Summarizer task

New omittable task-config section, following the existing pattern
(`memory_extraction` et al.):

```toml
[tasks.affinity_summary]
# model / params per the shared task-config shape
```

Section absent ⇒ feature off ⇒ the engine runs exactly as before except
`feeling_clause` stays NULL and `[feelings]` never renders.

**Input payload** (built engine-side):

- the character's name;
- band labels (低/中/高 via the existing `axis_band_label`, 0.35/0.65 cuts)
  for each of the six axes **active in the triggering request's
  `AffinityScope`** — out-of-scope axes are omitted. Endpoints are banded the
  same way as line axes: the anchoring concern that keeps floats away from
  the judge does not arise here (the summarizer reports no grades), but the
  no-floats rule holds — bands are all it needs;
- the reasons from the most recent 5 `companion_affinity_events` rows for
  this session that carry a judge reason (rows whose context is only a
  skip-reason annotation are passed over).

A session whose scope varies across requests gets a clause written under the
last writer's scope. Deployed scopes are static per integration; no
machinery for the mixed case.

**Output contract:** strict JSON `{"clause": "..."}`. The clause is 1–3
Chinese sentences, first person, in the character's voice — "how I feel
about him right now as a whole". The system prompt carries the same hygiene
rules as the eval judge's `reason` (no AI/assistant/system vocabulary, no
「用户」; the payload labels the human 「对方」), because the clause is
re-injected into later system prompts and a leaked system register would
canonise itself the same way a leaked refusal once did.

**Failure handling:** LLM error, unparseable output, or an empty clause ⇒
`tracing::warn!`, keep the old clause, no retry, never fail or delay the
post-process pipeline. The next movement turn rewrites anyway.

## 6. Prompt-side changes

### 6.1 `[feelings]` rewrite

The raw-number rendering is deleted with no fallback. When
`feeling_clause` is non-NULL and at least one axis is active in the
request's `AffinityScope`, the block renders:

```
[feelings]（你此刻对他的真实感觉，这是内心状态，绝对不要复述）
{clause}
```

Clause NULL, or zero axes in scope ⇒ block absent, byte-identical to the
current empty-scope case. Side benefit: floats moved every turn by ±0.01;
the clause changes only on movement turns, so this section is strictly more
cache-stable than what it replaces.

### 6.2 `[mood]` slimming

Criterion: **cold-side bans and behavior unlocks stay engine-side
(deterministic gates); warm-side texture moves to the clause (narrative).**
Per-directive disposition of `affinity_to_attitude_prompt`:

| Directive | Disposition |
|---|---|
| warmth ≤ 0.2 「语气冷淡，不主动延伸话题」 | **keep** — cold gate, same side as the nudge veto |
| warmth 0.2–0.35 「语气平淡，保持礼貌但不热络」 | drop — texture, clause's job |
| warmth 0.35–0.65 「语气友善自然」 | drop — texture |
| warmth > 0.65 「语气温暖，可以用一些亲昵的称呼」 | **halve**: drop 「语气温暖」, keep 「可以用一些亲昵的称呼」 (unlock) |
| trust > 0.6 「可以分享更私密的想法和小秘密」 | **keep** — unlock |
| trust < 0.3 「保持一定距离感，不轻易透露内心想法」 | **keep** — cold gate |
| intrigue > 0.7 「你对他很好奇，主动问问题，想了解更多」 | drop — question rhythm is `TurnNudges`' fact (#332); a standing "ask questions" fights the 5% die; curiosity texture goes to the clause |
| intrigue < 0.3 「你对他兴趣不大，不会主动找话题」 | **keep** — cold gate, nudge-veto side |
| intimacy > 0.5 「可以引用之前聊过的事情，有默契感，用你们之间的梗」 | **keep** — unlock |
| patience < 0.35 「你有点不耐烦了，回复可以更敷衍」 | **keep** — cold gate |
| patience > 0.65 「你很有耐心，愿意陪他聊」 | drop — texture |
| tension > 0.5 「带点小傲娇，不要太好说话，适度推拉」 | **keep** — a behavior instruction; a feeling-summary cannot reliably emit an operation like 推拉 |

`[mood]`'s header and empty-elision behavior are unchanged.

## 7. Testing

- **`prompt.rs`:** clause present ⇒ rendered verbatim under the new header;
  clause NULL ⇒ block absent; zero-scope ⇒ block absent. `[mood]` keep/drop
  asserted per the §6.2 table. Existing assertions on `warmth=`-style
  raw-number output are updated to the new spec — this is the feature's own
  expectation change, not assertion weakening.
- **`post_process`:** unit tests for the movement predicate — any
  `grade >= 1` triggers, endpoint level `!= 2` triggers, all-zero
  white-water turn does not.
- **store:** round-trip of the two new columns.
- **summarizer:** payload builder (scope filtering, band labels, reason
  selection) and output parsing (good JSON / garbage / empty).

## 8. Documentation sweep

Per the behavior-change sweep rule: `docs/`, `examples/*.toml` (add an
`[tasks.affinity_summary]` example section), `README` if it describes the
prompt layout. Scan by concept (`[feelings]`, raw values, six axes), not by
identifier.

## 9. Out of scope

- `[emotional_context]`, `[reply_length]`, `TurnNudges` probabilities and
  vetoes — all unchanged.
- Clause history / versioning.
- Any staleness-driven or sweeper-driven refresh.
- API surface: no endpoint changes; the clause is engine-internal state. If
  a later consumer wants it read-side, that is a new endpoint decision.

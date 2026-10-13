# Persona needs — Design

- **Date:** 2026-10-13
- **Status:** Draft — ready for review
- **Type:** Engine change. One migration (a partial index). Additive HTTP
  changes:
  - the PDE judge's verdict gains `needs`;
  - the `body_state` frame and `GET …/effects` gain `needs`;
  - the voice bootstrap gains `needs`.

  `eros-engine-core` gains a module; nothing in it breaks.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the 1:1 text turn's judge, reply prompt and
  frames; the effects route; the voice bootstrap.
- **Builds on:** `2026-10-12-given-item-lingering-effects-design.md` (*the
  effects spec*) and `2026-10-13-pde-reasoning-design.md`, which lands first
  and reshapes the judge call this design adds a field to.

## 1. Motivation

The persona has a mood (`inner_state`) and, after a give, a bodily state, but
no basic needs. She is never hungry, never worn out, never lonely for company
unless the conversation spells it out turn by turn. Needs are the signals that
make her read as a living creature with a day of her own.

Four needs, each at one of three levels, rated by the PDE judge. The PDE judge
rates the persona alone; the affinity evaluator rates the persona and the
user together. Needs belong to the first.

## 2. Principles applied

1. **The judge rates, the engine keeps time.** The judge outputs three-level
   ordinals. Between ratings, the engine carries the levels forward with fixed
   per-need drift; the model never reads or writes a number.
2. **One source per fact.** The levels in effect are stored once, on the user
   row of the turn whose judge rated them. Every reader derives the current
   state from the newest such row plus the time since.
3. **Default-open.** Needs exist wherever the LLM judge is configured. A
   deployment without `[tasks.pde_decision]` has none, and none of the readers
   below query for them.

## 3. The needs

| key | 中文 | `low` | `high` |
|---|---|---|---|
| `hunger` | 饥饿 | not hungry | very hungry |
| `social` | 社交欲 | wants no company | wants someone to talk to |
| `stamina` | 体力 | worn out | full of energy |
| `emotional` | 情感需求 | content | badly wants to be cared about |

Each is `low`, `mid` or `high`.

### 3.1 Core

A new module `eros-engine-core/src/needs.rs`:

```rust
#[serde(rename_all = "snake_case")]
pub enum NeedLevel { Low, Mid, High }

#[derive(Serialize, Deserialize, ..)]
pub struct Needs {
    pub hunger: NeedLevel,
    pub social: NeedLevel,
    pub stamina: NeedLevel,
    pub emotional: NeedLevel,
}

/// Levels rated `hours` ago, carried forward to now (§3.2).
pub fn carried(rated: Needs, hours: f64) -> Needs;
```

The serde shape is persisted on user rows (§5) and in the voice snapshot
(§8), and mirrored by the wire DTO (§7).

### 3.2 Drift

Each need moves one level toward its resting level for every full period
since it was rated, and stops there:

| need | resting level | period |
|---|---|---|
| `hunger` | `high` | 4 h |
| `social` | `high` | 12 h |
| `stamina` | `mid` | 8 h |
| `emotional` | `mid` | 12 h |

So time away makes her hungry and wanting company, while tiredness and
emotional swings settle. `hours ≤ 0` changes nothing. The table is a set of
`const`s in `needs.rs`, retuned in place.

## 4. The judge

### 4.1 Context line

`build_pde_ctx` gains `needs: Option<&Needs>` and always renders a line when
the judge runs, before `[身体状态]`:

```
[需求] 饥饿=高 社交欲=低 体力=中 情感需求=中
```

`高` / `中` / `低` map to `high` / `mid` / `low`. With no rating yet, the line
reads `[需求] 尚未判定`. The line is never absent on a judged turn: a missing
line must not read as "all low".

The levels shown are the newest rating strictly before this turn's user row,
carried to that row's `sent_at` (§5.2), so a re-driven turn shows the judge
the same line.

### 4.2 Verdict

`PdeVerdict` gains `needs: Option<Needs>`.

- `null`, an absent key, or any value that fails to parse reads as `None`.
  A malformed `needs` never fails the verdict; the action decision stands.
- The strict `response_format` gains a nullable `needs` object. When it is an
  object, all four keys are required, each an enum of `low`/`mid`/`high`.
- `None` means the judge abstained this turn. The previous rating keeps
  drifting.

### 4.3 Reference `filter_prompt`

The commented judge prompt in `examples/model_config.toml` gains:

```
#  "needs": 角色此刻的四项基本需要，各 "low" | "mid" | "high"：
#    hunger 饥饿（high=很饿）、social 社交欲（high=很想找人说话）、
#    stamina 体力（high=精力充沛）、emotional 情感需求（high=很需要被在乎）。
#    以 [需求] 行为起点，结合对话、[身体状态]、时间重判。只评角色自己，
#    不评你们之间的关系。拿不准就写 null（沿用上次的判定）。
```

### 4.4 Audit

- `VerdictAudit` gains `needs` (omitted when `None`): what the judge said.
- The `inputs` snapshot gains `needs`: the carried levels the judge was shown,
  or `null` when there was no rating. `v` stays `1`; the key is additive.

## 5. Storage

### 5.1 Writing

`ChatRepo::set_needs(user_message_id, &Needs)` writes `metadata.needs` with
`jsonb_set`, keeping every other key.

- It runs once the judge returns a parsed verdict carrying `needs`, before
  the turn's frames, and is awaited. A failed write is logged at `warn`; the
  turn goes on with the verdict's levels in memory.
- It runs whatever the turn's outcome — reply, image, ghost, product QA. The
  rating describes the persona, not the reply.
- A re-driven turn rates again and overwrites the row's `needs`.
- A rule-engine turn (the judge is off or failed) and a `needs: null` verdict
  write nothing.

### 5.2 Reading

`ChatRepo::latest_needs(instance_id, before: Option<Uuid>)` returns
`metadata.needs` and `sent_at` of the newest `role = 'user'` row carrying
`needs`, across every session of the instance (`chat_sessions.instance_id`).
With `before = Some(anchor)`, only rows strictly before the anchor's
`(sent_at, id)` are read. A value that fails to parse reads as no rating.

`carried(rated, hours)` then brings it to the reader's moment:

| reader | `before` | carried to |
|---|---|---|
| text turn: judge line and reply | the turn's user row | that row's `sent_at` |
| `GET …/effects` (§7.2) | `None` | now |
| voice call (§8) | `None` | the first voice turn |
| event director (`2026-10-13-life-events-design.md`) | `None` | now |

Needs are per instance, not per session: every text session with the same
persona shares one state. Only judged text turns write them; group and voice
turns never do.

### 5.3 Migration `0070_chat_messages_needs_idx.sql`

```sql
CREATE INDEX chat_messages_needs_idx
    ON engine.chat_messages (session_id, sent_at DESC)
    WHERE role = 'user' AND metadata ? 'needs';
```

It keeps `latest_needs` an index probe per session, including on instances
whose recent turns carry no rating. Building it scans `chat_messages`, as
`0064` did.

## 6. The reply

### 6.1 Which levels

The reply on a judged turn is written with the levels the turn ends up with:
the verdict's `needs` when present, otherwise the carried levels the judge was
shown. On a rule-engine turn of a deployment with the judge configured, it is
the carried levels.

### 6.2 Rendering

`prompt::needs_line(&Needs) -> Option<String>` renders the levels that are
not `mid`, in the order of §3, joined by 「；」; `None` when all four are
`mid`.

| need | `low` | `high` |
|---|---|---|
| `hunger` | 不饿 | 饿了 |
| `social` | 不想跟人打交道 | 想找人说说话 |
| `stamina` | 很累 | 精神很好 |
| `emotional` | 心里挺满足 | 很想被在乎 |

`build_prompt` and `build_reply_request` gain `needs: Option<&str>`.
`[character_state]` gains a line after 「身上还带着」:

```
- 此刻的需要：饿了；很累
```

- The line alone renders the block, as the outfit line does.
- `CHAT_NOISE_CANCELLATION_DISABLED` does not suppress it.
- All four at `mid`, or no rating, gives a prompt byte-identical to today's.
- The stream path passes it. `/open`, proactive messages, image edit and
  tests pass `None`.

## 7. Returning the state

### 7.1 Frame

`ProtocolFrame::BodyState` gains `needs: Option<Needs>`, serialized as an
object or `null`:

```json
{"type": "body_state", "user_message_id": "…", "effects": [],
 "needs": {"hunger": "high", "social": "low", "stamina": "mid", "emotional": "mid"}}
```

- It carries the levels the reply was written with (§6.1).
- It is sent when `effects` is non-empty or `needs` is known, in the same
  position as today: after `action_response`, before the first `meta`.
- An idempotent replay re-sends it from the replayed row's `metadata.needs`,
  or, when the row has none, from the carried levels anchored at that row.

### 7.2 `GET /v2/comp/session/{session_id}/effects`

The response gains `needs`: the instance's levels carried to now, or `null`
when there is no rating or the judge is not configured. `NeedsView` is the
`ToSchema` DTO, shared by the route and the documented frame.

## 8. Voice

- `VoiceBootstrap` gains `needs: Option<Needs>` (`serde(default)`): the
  instance's levels carried to the call's first turn, frozen for the call.
  It loads alongside the effects part, and a failed read fails that part.
- The `[身体状态]` block renders `此刻的需要：…` after the effects, using the
  same `needs_line`. The block counts as non-empty when only this line is
  present.
- Every voice turn's `body_state` frame carries the frozen `needs`.
- A call never writes needs.

## 9. Where it lives

| change | file |
|---|---|
| `NeedLevel`, `Needs`, `carried`, drift table | `core/src/needs.rs` (new), `core/src/lib.rs` |
| `set_needs`, `latest_needs` | `store/src/chat.rs` |
| migration | `store/migrations/0070_chat_messages_needs_idx.sql` |
| verdict field, schema, `[需求]` line, write, audit, frame | `server/src/pipeline/stream.rs` |
| `needs_line`, `[character_state]` line | `server/src/prompt.rs` |
| `needs` through `build_reply_request` | `server/src/pipeline/handlers.rs`, its callers |
| replay | `server/src/routes/companion_stream.rs` |
| `needs` on the route, `NeedsView` | `server/src/routes/session_effects.rs` |
| bootstrap part, block line, frame | `server/src/pipeline/voice.rs` |
| reference judge prompt | `examples/model_config.toml` |
| OpenAPI | `crates/eros-engine-server/openapi.json` (regenerated) |

## 10. Documentation

- `docs/api-reference.md` / `.zh.md`: the `body_state` frame's `needs`, the
  wider condition for sending it, and `needs` on `GET …/effects`.
- `docs/model-config.md` / `.zh.md`: the judge's `needs` field, the
  `[需求]` context line, the drift table.
- `docs/llm-audit.md` / `.zh.md`: `needs` in the decision payload and in
  `inputs`.
- Wherever the voice bootstrap's parts and the `[character_state]` lines are
  listed.

## 11. Rollback and deploy

- **Rolling back.** The previous image ignores `metadata.needs`, the verdict
  key and the index. Frames and the route lose `needs`.
- **Deploying.** The index builds at boot. A deployment sees needs once its
  judge prompt asks for them; a strict-schema judge produces them on its own.

## 12. Testing

- **core.** `carried` for every need from every level: zero hours, just under
  and exactly one period, several periods, stopping at the resting level, and
  negative hours. `Needs` serde shape.
- **store** (`#[sqlx::test]`). `set_needs` keeps other metadata keys and
  overwrites on a second call. `latest_needs` returns the newest rated row
  across two sessions of one instance; ignores other instances and rows
  without `needs`; with an anchor, reads strictly before it.
- **judge.** The `[需求]` line with levels and as `尚未判定`, before
  `[身体状态]`. A verdict with `needs` parses. A verdict with `needs: null`,
  without the key, or with a bad value parses with `None` and keeps its
  action. The schema requires the four keys inside a non-null object.
- **stream.** A judged turn writes `metadata.needs`, sends one `body_state`
  frame with `needs` and empty `effects`, and puts `此刻的需要` in the reply
  prompt only for non-`mid` levels. A turn whose verdict has `needs: null`
  writes nothing and carries the previous levels. A ghost turn still writes. A
  re-driven turn shows the judge the same line. A replay re-sends the frame.
  With the judge unconfigured: no query, no line, no frame change.
- **prompt.** Every word of §6.2. The 「；」 join. The line after
  `身上还带着`, alone rendering the block, kept under the noise-cancellation
  flag. All-`mid` gives a byte-identical prompt.
- **voice.** The bootstrap freezes carried needs at the first turn. An old
  marker without `needs` reads as none. The block line renders. The frame
  carries it.
- **route.** `needs` carried to now, and `null` with no rating.
- **Audit.** The payload carries what the judge said; `inputs` carries what it
  was shown.
- **Regression.** New parameters on `build_pde_ctx`, `build_prompt`,
  `build_reply_request` and the `BodyState` variant change call forms only.
  Frame assertions that now see a `needs` key change their expected value;
  each is listed in the PR.
- `openapi.json` regenerated.

## 13. Non-goals

- Rules that act on needs directly, such as ghosting more when tired or
  sending fewer pictures when hungry. The judge sees the needs and decides.
- A time-of-day factor in the drift.
- Needs in the image composer, `/open`, proactive messages or image edit.
- Needs rated or read by the group director.
- Any need beyond the four, or a fourth level.

## 14. Decisions taken

- **Stored on the rated turn's user row**, over a per-instance state table: a
  table would hold a second copy of what the judge said, a replay could not
  recover the levels a past reply was written with, and the user row already
  carries the turn's other verdict in effect (`metadata.action.response`).
- **Per instance**, read across the instance's sessions: needs describe the
  persona, so a new session does not reset her hunger.
- **Drift toward a resting level**, one fixed period per need, over a
  rule-computed baseline that the judge may override.
- **Always a `[需求]` line on a judged turn**, `尚未判定` when there is no
  rating.
- **Only non-`mid` levels reach the reply**, so a typical state adds nothing,
  and the persona does not mention her needs every turn.
- **A malformed `needs` never fails the verdict.**

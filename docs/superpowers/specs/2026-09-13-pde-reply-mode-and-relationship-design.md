# PDE reply mode, relationship ground, dice in the audit row — Design

- **Date:** 2026-09-13
- **Status:** Draft — ready for review
- **Type:** Engine change. No migration, no schema change. One new judge
  verdict field, one removed; one new prompt section, three removed, one
  merged; the per-turn dice move from the reply builder into the decision
  plan and land in the existing audit `payload`. Public API unchanged; the
  judge JSON contract gains `reply_mode` and loses `tone`.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — the text chat turn's decision and system prompt.
  Release number and timing are the owner's call, not this document's.

## 1. Motivation

The echo-cancellation work (2026-08-19, 2026-09-02) removed the amplification
paths through injected history. Repetition now surfaces only deep into a
session, which says the history-driven pattern is gone and what remains is
the model's own default shape for a reply. The next lever is therefore the
system prompt itself: give the reply a different skeleton each turn, decided
by the judge rather than left to the model.

Three observations shape the change.

**The reply's relation to the last message is hard-coded.** Iron rule ①
(`prompt.rs:977`) says *catch what they just said: continue it, or react to
it*. That is one conversational move — the "follow" move — presented as an
invariant. Listening without extending, asking, and pushing back are all
legitimate moves the persona never gets to make, because the only standing
instruction describes one of them.

**The prompt describes tone from the affinity numbers but never says what the
relationship is.** `[mood]` renders per-axis gates and `[feelings]` a
first-person clause, both of which modulate *how* the persona speaks. Nothing
states *who the user is to her*. With no ground to stand on the model defaults
to "a friend", and the cold quadrant never reads as cold — the persona cannot
turn on the user because nothing ever told her she is not his friend.

**The dice are invisible after the fact.** `TurnNudges` (`prompt.rs:180-208`)
is rolled in `build_reply_request` (`handlers.rs:899-905`) and leaves no
record. The judge's audit row (`companion_decision_events`) cannot say whether
a turn's opening question came from the die or from the model, which makes the
"judge unsure → dice take over" hand-off designed here unauditable.

Four prompt sections are also retired in the same change because the
per-section review that produced this design found them either superseded or
harmful; §3.5 lists them with the reason each goes.

## 2. Principles applied

1. **Three channels, unchanged** (#332). Affinity = judged conclusions
   injected as state; iron rules = invariants independent of affinity; dice =
   cadence the engine owns. `reply_mode` is a judged conclusion about *this
   message*; `[relationship]` is a judged conclusion about *this person*.
   Neither is a standing rule.
2. **Prohibition by absence.** A mode's variant of rule ① names only the move
   it wants; the moves it does not want are simply not mentioned. No
   "don't ask", no "don't extend" (#329).
3. **One source per fact.** The relationship quadrant is derived from two
   affinity axes already snapshotted in the audit row's `inputs`; it is not
   stored again. `[clothing]` moves into `[character_state]` rather than
   remaining a second block about the same present-tense state.
4. **The model reads ordinals, not numbers.** `reply_mode` is a four-value
   enum plus null. Reply length becomes three fixed ceilings rather than
   ranges the model would have to judge.
5. **The prompt is a pure function of its inputs.** `build_prompt` renders
   what it is handed. The invariant "a definite mode means no dice" is
   enforced where the plan is finalised, not inside the renderer.

## 3. The changes

### 3.1 Judge verdict: `reply_mode` in, `tone` out

`PdeVerdict` (`stream.rs:1990-2008`) gains

```
reply_mode: Option<ReplyMode>   // serde(default); listen | follow | ask | judge
```

and loses `tone`. `ReplyMode` is a new `enum` in `eros-engine-core::types`,
`snake_case` serde. The strict `response_format` schema
(`stream.rs:2101-2125`) adds `"reply_mode": {"type": ["string","null"],
"enum": ["listen","follow","ask","judge",null]}` to `properties` and
`required`, and removes `tone` from both.

`null` means the judge is not sure, and the engine decides (§3.2). A
`filter_prompt` written before this change never emits the field, so every
turn is `null` and behaviour equals today's. Removing `tone` is safe in both
transports: under `structured_output` the schema no longer admits it; without
it, serde ignores unknown fields.

**Carriage.** `ActionPlan` (`core/src/types.rs:165-192`) gains
`reply_mode: Option<ReplyMode>` and loses `reply_tone`. `plan_for`
(`core/src/pde.rs:133-196`) takes `reply_mode` in place of `reply_tone` and
applies the rule that governs `clothing` today: kept on `ReplyText` and
`ReplyTextImage`, dropped to `None` on `ReplyImage`, `Ghost`, `ProductQa`.
The rule engine's `decide` and the tips path produce `None`.

**Judge guidance** lives where the judge's prompt lives — the operator's
`filter_prompt`. The engine ships the reference text in
`examples/model_config.toml` (the commented block at `:725-743`), replacing
the `tone` line with:

```
"reply_mode": 选填,"listen" | "follow" | "ask" | "judge" | null。这一轮怎么接对方的话:
  listen = 对方在倾诉或铺陈,接住就好;
  follow = 顺着对方的话往下接;
  ask    = 对方话少、话题快断,抛一个问题;
  judge  = 对方的话你不买账或有疑点,反问、质疑;
  拿不准就 null(引擎自己决定)。
```

The judge is a classifier, not the persona, so its prompt may describe the
criteria; principle 2 applies to the reply prompt only. `build_pde_ctx`
(`stream.rs:2416-2491`) is unchanged: `[最近对话]` and `[信号]` carry what the
mode decision needs.

### 3.2 Dice move into the plan and into the audit row

`TurnNudges` — the three booleans — moves to `eros-engine-core::types` and
becomes `ActionPlan.nudges: TurnNudges` (`Default` = all false). The roll stays
in the server as a free function over the core struct
(`prompt::roll_nudges(affinity, scope, rng) -> TurnNudges`), because core has
no `rand` dependency and should not gain one. Probabilities, cold-floor vetoes
and scope gating are byte-for-byte the existing `TurnNudges::roll`.

**Where the roll happens.** In `stream.rs`, after the plan is final — after
`guard_action`, `plan_for`, the ghosting kill-switch (`:4137-4142`) and the
forced-image override (`:4151-4161`) — and before the audit task is spawned
(`:4182`):

```
plan.nudges = if plan.reply_mode.is_none() && plan.action_type.bears_text() {
    roll_nudges(&input.affinity, affinity_scope, &mut rand::thread_rng())
} else {
    TurnNudges::default()
};
```

where `bears_text` is `ReplyText | ReplyTextImage`. A definite mode means no
roll; a non-text action means no roll. `build_reply_request`
(`handlers.rs:899-905`) no longer rolls; it passes `plan.nudges` and
`plan.reply_mode` to `build_prompt`. `image_edit.rs`'s `edit_turn_plan`
(`:301-314`) fills both new fields with their defaults.

**Audit.** `VerdictAudit` (`stream.rs:2523-2552`) gains

```
reply_mode: Option<&str>        // skip_serializing_if none
nudges: Option<TurnNudges>      // always serialised: null = not rolled
```

and loses `tone`. Both are read from the finalised `plan`, not from the
verdict, so the row records the values that took effect: `reply_mode` after
carriage, `nudges` as rolled or `null`. A payload therefore reads either
`{"action":"reply_text","reply_mode":"listen","nudges":null,…}` or
`{"action":"reply_text","nudges":{"affirm":true,"share_slice":false,"open_question":false},…}`.
The `payload` column is JSONB; no migration.

**Boundary.** The audit row is written only when the LLM judge ran
(`stream.rs:4167-4168`). Deployments on the rule PDE keep rolling dice and
keep not recording them. This document does not open a second audit path.

### 3.3 `[relationship]` — the ground the persona stands on

One sentence stating who the user is to the persona, derived from the affinity
state `build_prompt` already receives (the previous turn's judged values, the
same instance `[mood]` reads):

| | patience ≥ 0.5 | patience < 0.5 |
|---|---|---|
| **warmth ≥ 0.5** | 好朋友 | 快被磨光耐心的朋友 |
| **warmth < 0.5** | 没什么交情的人 | 死对头 |

Rendered as

```
[relationship]
你是用户的{noun}
```

placed after `[reply_length]` and before `[mood]`, with the other
affinity-derived sections. The sentence is an identity, not an instruction:
it carries no behaviour. How the persona acts on it remains `[mood]`'s and
`[feelings]`'s job, and neither is touched.

The quadrant is independent of the bond / chemistry projections and of the
`[mood]` cold floors (0.2 / 0.3 / 0.35). Both axes move every turn, so the
ground moves with them.

**Omitted** when there is no affinity yet, or when `affinity_scope` excludes
either `warmth` or `patience` — the same rule `[mood]` applies to a missing
axis. **Not audited**: `pde_inputs_snapshot` already stores both axes, and the
quadrant is a two-comparison derivation.

### 3.4 Sections that vary with `reply_mode`

`build_prompt` gains `reply_mode: Option<ReplyMode>`. Three sections read it.

**Iron rule ①.** One sentence per mode; `None` is today's text unchanged.

| mode | ① |
|---|---|
| `None` | 先接住对方刚说的话：顺着它往下接，或对它给出你自己的反应；不解释自己为什么这样说 |
| `follow` | 顺着对方刚说的话往下接，把它接成你们的话题；不解释自己为什么这样说 |
| `listen` | 对方在说，你在听：只对刚说的这句给出你的反应；不解释自己为什么这样说 |
| `ask` | 接住对方刚说的话，然后问一个你真的想知道答案的问题；不解释自己为什么这样说 |
| `judge` | 对方刚说的话你不全买账：挑出你不认同的那一点，反问回去；不解释自己为什么这样说 |

Each sentence has a different shape and names one move. `listen` never says
"continue"; `follow` never says "ask"; `judge` never says "catch".

**`[reply_length]`** (`prompt.rs:216-225`). The three tiers keep their
composite-score thresholds (0.25 / 0.55) and become fixed ceilings:

| tier | text |
|---|---|
| low | 最多 1 句，不超过 40 字 |
| mid | 最多 2 句，不超过 80 字 |
| high | 最多 3 句，不超过 120 字 |

`listen` renders one tier below the affinity tier; the low tier stays low.
The other modes and `None` use the affinity tier as-is.

**`[this_turn]`** (`prompt.rs:929-953`) renders `plan.nudges` exactly as
today. Because §3.2 leaves the dice at their default whenever a mode is set,
the block is absent on every definite-mode turn without the renderer knowing
why.

### 3.5 Sections retired

Each of these was reviewed section by section against the current prompt.

- **Identity timezone clause** (`prompt.rs:699-702`, `你所在时区：X。`). The
  persona's `timezone` exists so the engine can compute her local time for
  `[now]`; naming the zone to the model invites it to narrate the zone
  instead of the time. The clause goes; `now_context` still resolves the zone.
- **`[now]` zone name** (`prompt.rs:158-168`). Same reasoning. The line
  becomes `现在你当地时间是 {date}（{wd}）{hh}:{mm}，{period}。…`; the rest of
  the sentence is unchanged.
- **`[turn_style]`** (`prompt.rs:292-300`, `:738`, `:964`). Pinned to
  `Neutral` on every judge-decided text turn, so on the main path it is a
  constant line saying "natural and calm" under whatever `[mood]` and
  `[feelings]` say. The section, `style_directive`, and `build_prompt`'s
  `style` parameter go. `ReplyStyle` stays: `decide` and the ghost plan
  still produce it and tests read it.
  On the edit-turn path (`image_edit.rs:286-297`, `tier_reply_style`,
  #349) this was the only tone channel, chosen by an A/B read at the time.
  That mapping becomes dead code and goes with the section; edit turns build
  through the same `build_prompt` and now carry `[relationship]`, `[mood]`
  and `[feelings]` like every other turn.
- **`[reply_tone]`** (`prompt.rs:755-760`). The judge's free-text delivery
  directive is a second voice on the channel `[relationship]` + `[mood]` +
  `[feelings]` now fills. The section, the verdict field, the schema entry,
  `ActionPlan.reply_tone`, and the `tone` lines in
  `docs/model-config.md:619`, `docs/model-config.zh.md` and
  `examples/model_config.toml:730` all go.
- **`[clothing]`** (`prompt.rs:764-769`) merges into `[character_state]`
  (`prompt.rs:843-874`) as a final line `- 此刻的穿着：{c}` after the four
  insight fields. The judge keeps emitting `clothing`; only its rendering
  moves. When the insights row is missing or empty and the judge gave an
  outfit, the block renders with the outfit line alone. The
  `CHAT_NOISE_CANCELLATION_DISABLED` flag (`handlers.rs:890-897`) keeps
  suppressing the four insight fields and nothing else: the flag restores
  the pre-noise-cancellation shape, and the outfit was never part of that.

## 4. Data flow, one turn

```
judge verdict           action · inner_state · clothing · reply_mode
  → guard_action        ghost veto, image / product_qa degrade
  → plan_for            reply_mode carried on text actions, dropped otherwise
  → kill-switch, force_image
  → roll                mode set or non-text action ⇒ Default; else veto-then-roll
  → audit row           payload.reply_mode, payload.nudges (from the plan)
  → build_reply_request passes plan.reply_mode, plan.nudges
  → build_prompt        ①, [reply_length], [this_turn], [relationship],
                        outfit inside [character_state]
```

The voice pipeline (`voice.rs` `build_voice_prompt`) takes none of these
inputs today and is not touched.

## 5. Where it lives

| change | file | anchor |
|---|---|---|
| `ReplyMode`, `TurnNudges`, `ActionPlan` fields | `core/src/types.rs` | `:165-192` |
| `plan_for` signature and carriage; `decide` defaults | `core/src/pde.rs` | `:133-196`, `:38-115` |
| verdict field, schema, plan wiring, roll, audit | `server/src/pipeline/stream.rs` | `:1990-2008`, `:2101-2125`, `:2523-2552`, `:4087-4133`, `:4182-4209` |
| roll removed; new `build_prompt` arguments | `server/src/pipeline/handlers.rs` | `:899-924` |
| `edit_turn_plan` defaults; `tier_reply_style` removed | `server/src/routes/image_edit.rs` | `:286-314` |
| `roll_nudges`; `[relationship]`; ① variants; length tiers; identity, `[now]`, `[turn_style]`, `[reply_tone]`, `[clothing]` | `server/src/prompt.rs` | `:158-168`, `:180-225`, `:292-300`, `:695-702`, `:738-769`, `:843-874`, `:929-983` |
| verdict field table | `docs/model-config.md`, `docs/model-config.zh.md` | `:610-625` |
| `[relationship]` mention | `docs/affinity-model.md` | — |
| reference judge prompt | `examples/model_config.toml` | `:725-743` |

## 6. Configuration and rollback

No new flag. `reply_mode` is off for any deployment whose `filter_prompt`
does not mention it (every turn `null`, dice as today). `[relationship]`,
the length ceilings and the five retirements are unconditional prompt-shape
changes whose rollback is the previous image, matching the `clothing`
change (#352).

## 7. Observability

`companion_decision_events.payload` answers, per judged turn: which mode the
judge chose, whether it survived carriage, whether dice were rolled, and what
they showed. The relationship quadrant is recomputable from `inputs.warmth`
and `inputs.patience`. `PROMPT_LOG_DIR` dumps show the rendered ① and
`[relationship]` for spot checks. No new table, column, or log line.

## 8. Testing

- **core.** `plan_for` carries `reply_mode` on `ReplyText` /
  `ReplyTextImage` and drops it on `ReplyImage` / `Ghost` / `ProductQa`;
  `decide` yields `None` and default `nudges`; `ActionPlan` no longer has
  `reply_tone`.
- **stream.** A verdict with and without `reply_mode` parses; the schema
  lists `reply_mode` in `required` and not `tone`; a definite mode leaves
  `plan.nudges` at default and the audit `nudges` as JSON `null`; a `null`
  mode on a text action rolls and audits an object; `Ghost` and `ReplyImage`
  never roll.
- **prompt.** Quadrant boundaries, with 0.5 on the warm / patient side; the
  section absent with no affinity or with either axis out of scope; each of
  the five ① sentences present for its mode and the other four absent; the
  three fixed length lines; `listen` one tier down with the low tier
  unchanged; `[this_turn]` absent under a definite mode; the identity line
  without a zone clause; `[now]` without a zone name while still rendering
  the zone-local time; `[turn_style]`, `[reply_tone]`, `[clothing]` headers
  absent everywhere; the outfit line inside `[character_state]`, alone when
  the insights row is empty, and present under the noise-cancellation flag.
- **handlers / image_edit.** `build_reply_request` rolls nothing and reads
  the plan; `edit_turn_plan` compiles with defaults.
- **Regression.** `build_prompt` loses `style` and `reply_tone` and gains
  `reply_mode`; `plan_for` swaps one parameter. Every existing call site in
  tests changes form. Assertions on prompt content are not weakened; tests
  that asserted `[reply_tone]` or `[turn_style]` content are deleted, not
  inverted.

## 9. Non-goals

- Changing the dice mechanism, probabilities, vetoes or the `[this_turn]`
  wording.
- Changing `[mood]`, `[feelings]`, `[inner_state]` or `[emotional_context]`.
  `[mood]`'s warmth and patience cold lines overlap the cold quadrants by
  design: the ground says who, the gate says how.
- Auditing turns on the rule PDE.
- The voice prompt.
- Any change to the affinity model or its thresholds.

## 10. Decisions taken

- **A definite mode suppresses the dice** rather than rolling alongside
  them. `[this_turn]` has one author per turn; a `listen` verdict next to an
  `open_question` die would be two voices on one channel.
- **Dice move into the plan** rather than being written back to the audit
  row after `build_reply_request` rolls them. A second write by `run_id`
  would race the spawned insert and still need the mode passed down to skip
  the roll; putting the roll at plan finalisation makes the plan the whole
  record of what the engine decided.
- **Modes are a separate field, not new `action` values.** `action` already
  carries the image dimension; folding modes in would multiply the enum and
  give ghost a sibling it does not have.
- **`null`, not an explicit `unsure` value.** Backward compatibility with
  every existing `filter_prompt` falls out for free.
- **Section variants, not whole-prompt variants.** Only ①, `[reply_length]`
  and `[this_turn]` depend on the mode; the other sections and the cache
  prefix are shared.
- **Noun set.** 好朋友 / 快被磨光耐心的朋友 / 没什么交情的人 / 死对头, chosen
  over two milder sets so the cold quadrants read as cold.
- **① copy.** The rewritten set (different sentence shape per mode) over a
  minimal edit of the current sentence; the point of the change is that the
  model sees a different skeleton, and four near-identical sentences would
  not deliver that.
- **Length ceilings are "at most N"**, not "exactly N".
- **The noise-cancellation flag does not suppress the outfit line.**
- **Retiring `[turn_style]` reverses the edit-turn tier→style decision
  (#349).** That decision was made when `[turn_style]` was the only tone
  channel on that path; with `[relationship]` on every path it is not.

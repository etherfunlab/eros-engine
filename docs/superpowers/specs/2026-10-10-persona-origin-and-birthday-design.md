# Persona species, origin, and birthday — Design

- **Date:** 2026-10-10
- **Status:** Draft — ready for review
- **Type:** Engine change. No migration. Four new optional keys in
  `persona_genomes.art_metadata`, two new prompt lines and one species-aware
  iron rule, one new engine-decided greeting occasion on
  `POST /v2/comp/session/{session_id}/open`. `eros-engine-core` is untouched.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — additive throughout. A genome without the new
  keys produces a byte-identical prompt and identical `open` behaviour.
  Release number and timing are the owner's call, not this document's.

## 1. Motivation

A persona today is a name, a gender, an age, an MBTI and prose. It has no
species, no home, and no birthday. Non-human characters (an elf, a deity, a
visitor from another planet) are forced through an iron rule that tells them
to react "as a human would", and no persona can have a day that is hers the
way a holiday is the user's.

Two behaviours follow:

1. **Identity on every turn.** The persona knows what she is and where she is
   from, and the iron rule no longer contradicts a non-human genome.
2. **The persona's birthday is an event.** She knows it is coming, knows when
   it is today, and speaks first when the user opens the session that day —
   the same shape as the holiday greeting, on the persona's side of the
   calendar.

## 2. Decisions

Settled during design review; not open for re-derivation during
implementation.

1. **The four fields are `art_metadata` keys, not columns.** Every persona
   attribute the engine reads (`gender`, `age`, `mbti`, `timezone`,
   `appearance`, …) already lives there; `persona_genomes` has been chat-only
   since migration 0024 and stays that way. No migration.
2. **The engine validates nothing.** As with every existing key, a missing or
   malformed value is silently "absent". Validation is the author's (the web
   form's) job.
3. **`species` is a slug, rendered as a label.** `human` / `alien` /
   `beastkin` / `elf` / `deity` map to Chinese labels in the identity line and
   English labels in iron rule ⓪. Any other non-blank string renders verbatim
   in both places — the same policy as `gender`. Absent or `human` renders
   nothing and leaves rule ⓪ untouched.
4. **`earth` is the reserved planet.** `planet` is free text; absent or
   `earth` renders no planet. Downstream maps its "Earth" choice to the slug.
5. **The birthday is a month-day string, `"MM-DD"`.** No year. February 29
   counts as February 28 in a non-leap year.
6. **The birthday follows the persona clock**: `art_metadata.timezone` →
   `user_timezone` → `Asia/Singapore`, the chain `[now]` already uses. A
   holiday is the user's day; a birthday is the persona's.
7. **Three birthday behaviours ship**: the `[now]` fact line with the
   holiday's six-day look-ahead, and the engine-decided `open` greeting.
8. **Birthday outranks holiday.** When both fall on the same open, the
   engine speaks for the birthday. One proactive row per local date, as today.
9. **Caller-named occasions are untouched.** A request that names
   `first_meet` / `returning` / `just_opened` gets that occasion whatever the
   date.
10. **Voice is out of scope.** The voice prompt has no identity line and no
    `[now]`; nothing changes there.
11. **Downstream work is not this document's.** The web form's new fields,
    its "Earth" → `earth` mapping, and deriving its zodiac from the birthday
    instead of storing one are follow-ups in the web repository.

## 3. Data contract

Four optional keys on `engine.persona_genomes.art_metadata`:

| Key | Value | Absent / invalid |
|---|---|---|
| `species` | slug; see §4.1 for the five known ones | treated as `human` |
| `planet` | free text; `earth` reserved | no planet rendered |
| `hometown` | free text | no hometown rendered |
| `birthday` | `"MM-DD"`, zero-padded, a real calendar day | no birthday |

"Invalid" for `birthday` means anything that does not parse as a month-day:
wrong shape, month 13, day 32, `02-30`. `02-29` is valid (Decision 5).
Blank strings count as absent for all four keys.

`examples/personas/aria.toml` documents the keys in its `[art_metadata]`
block, commented like `timezone` and `appearance`. The seed path
(`upsert_genome`) already writes the whole JSONB object and needs no change.

## 4. Prompt rendering

All in `crates/eros-engine-server/src/prompt.rs`, inside `build_prompt`. The
three affected pieces are the identity line (stable prefix), iron rule ⓪
(volatile tail, but constant per genome), and `[now]` (volatile tail). The
prompt-cache boundary does not move.

### 4.1 Species in the identity line

Labels:

| slug | identity label | rule ⓪ label |
|---|---|---|
| `human` | — | — |
| `alien` | 外星人 | alien |
| `beastkin` | 兽人 | beastkin |
| `elf` | 精灵 | elf |
| `deity` | 神明 | deity |
| other | verbatim | verbatim |

The species label goes before the gender label:

```
你是 Aria，精灵，女性，27 岁，INFJ 性格。
你是 Aria，精灵，27 岁，INFJ 性格。          (no gender)
你是 Aria，女性，27 岁，INFJ 性格。          (human / absent — unchanged)
```

### 4.2 Origin sentence

One sentence appended to the identity line, present only when at least one of
`planet` (non-`earth`) or `hometown` is set:

```
来自 X 星，故乡是 Y。     both
来自 X 星。               planet only
故乡是 Y。                hometown only
```

Rendered on the same line as the identity sentence, separated by nothing more
than the sentence-final `。`.

### 4.3 Iron rule ⓪

Human or absent — today's text, byte for byte:

```
⓪ You are a real person. You have your own life, feelings, and quirks; you live, feel, and react as a human would.
```

Any other species, with `{label}` from §4.1:

```
⓪ You are a real {label}, not an AI and not an actor. You have your own life, feelings, and quirks; you live, feel, and react as your kind would.
```

Rules ①, ② and the gender anatomy clause are unchanged.

### 4.4 Birthday line in `[now]`

A persona-side line after the user-side holiday line (spec 2026-09-26 §5.2):

```
今天是你的生日。
明天是你的生日。
3 天后是你的生日。
```

Window: today through `holiday::LOOKAHEAD_DAYS` (six) days ahead, on the
persona clock (Decision 6). At most one line; no birthday in the window → no
line. The line states a fact; nothing tells the persona to celebrate.

The persona clock is resolved once per build (today it is resolved inside
`now_context_at`); the birthday computation takes the same `Tz`. February 29
on a non-leap year matches February 28 of that year (Decision 5); on a leap
year it matches February 29 only.

## 5. The birthday greeting

`POST /v2/comp/session/{session_id}/open`, the branch without `occasion`
(`holiday_occasion` in `routes/session_open.rs`), becomes:

1. Resolve the persona clock's `Tz` and today's persona-local date.
2. If the genome's `birthday` is that date (Decision 5 rule for Feb 29):
   `Occasion::Birthday { local_date }`. `since` is
   `holiday::local_midnight_utc(tz, local_date)`. The de-duplication is
   exactly the holiday's: an existing proactive row for `local_date` is
   returned; a message since local midnight means nobody speaks.
3. Otherwise the holiday logic runs as today.

`Occasion::Birthday` in `pipeline/proactive.rs`:

- `name()` = `"birthday"`; the row's `metadata.proactive = "birthday"` plus
  `local_date` (`YYYY-MM-DD`), alongside the post-resolve keys every proactive
  row carries.
- The stage cue is `OPEN_CUE`, unchanged. The fact arrives through `[now]`
  (§4.4), which this turn renders as `今天是你的生日。`.
- Recall query: `生日`, so the user's own past mention of the persona's
  birthday can surface.
- Persistence: `insert_proactive_message` with the same `since` re-check and
  `ON CONFLICT` fallback to `proactive_greeting_on` as the holiday.

Wire contract: `GreetingOccasion` gains `birthday`; `GreetingDto::from`
deserialises it like the others. `OpenOccasion` (the caller's enum) does
not change.

What does not run is what does not run for the holiday (spec 2026-09-26
§6.4): no post-process, no ghost streak, no signals.

## 6. Testing

Prompt unit tests (`prompt.rs`):

- species identity line for `elf` with and without gender; unknown slug
  rendered verbatim; `human` and absent byte-identical to today.
- origin sentence: both / planet only / hometown only / `earth` dropped /
  none.
- rule ⓪ swapped for `deity`, untouched for `human` and absent.
- birthday line: today / tomorrow / N days / 7 days out (absent) / Feb 29 on
  a leap and a non-leap year / malformed string (absent) / persona timezone
  vs user timezone picks the persona's.
- a genome with all four keys absent builds a prompt byte-identical to the
  pre-change fixture.

Route tests (`session_open.rs`), with the clock pinned as the holiday tests
do:

- birthday today → `greeting.occasion == "birthday"`, one row, second open
  returns the same row.
- birthday and holiday on the same date → `birthday`.
- user spoke since persona-local midnight → `null`.
- caller-named `first_meet` on the birthday → `first_meet`.

Proactive metadata test: the written row carries `proactive: "birthday"` and
`local_date`.

## 7. Documentation

- `docs/api-reference.md` / `.zh.md`: the `open` section's list of occasions
  gains `birthday` under the engine's call, next to the holiday.
- `examples/personas/aria.toml`: the four keys, commented.
- `docs/architecture.md` / `.zh.md` wherever the identity line or `[now]`
  is described (scan by concept, not identifier).

## 8. Non-goals

- A birthday that needs a year (age progression).
- A scheduler that finds every persona whose birthday is today; speaking
  first stays tied to `open`.
- A persona-side holiday calendar keyed on `planet`.
- Any validation of `art_metadata` in the engine.
- Web-side form, overlay, or zodiac changes (Decision 11).

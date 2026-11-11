# Stocks, trading, getting lost, habit events and `just_have_sex` — Design

- **Date:** 2026-11-11
- **Status:** Approved
- **Type:** Engine change. No migration. Additive HTTP change: the events
  route's kind enum gains eight drawable kinds. `having_sex` keeps its wire
  value and gains a gate. `PersonaTraits` gains the persona's habits.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-15-life-events-v2-design.md` (the draw, gates,
  factors), `2026-10-25-life-events-follow-ons-and-persona-keys-design.md`
  (follow-ons, `is_virgin`, `sex_addict`), `2026-10-28-outing-indulgence-
  needs-and-habits-design.md` (habit keys, `indulgence`),
  `2026-11-08-strip-and-world-setting-design.md` (`setting`),
  `2026-11-11-eight-short-events-and-hygienic-design.md`. Everything there
  stands except where this document says otherwise.

## 1. Intent

More of her day: watching the market, making a deal, getting lost on a
trip, the drink, smoke or weed she is hooked on, and, for a sex addict,
masturbating. Sex in her life
becomes something she tells the user about afterwards; the offline
"busy having sex" stretch is kept for a persona who is a sex addict.

## 2. Eight new short kinds

All `short`, told when they start, never offline, no picture.

| kind | label | weight | cooldown | lasts | setting | cue |
|---|---|---|---|---|---|---|
| `watching_stocks` | 在看股票 | 2 | 24 h | 30 min | modern only | （你正在看股票行情。） |
| `trading` | 在交易 | 1 | 24 h | 1 h | both | （你正在和人谈一笔买卖。） |
| `got_lost` | 迷路了 | 3 | 48 h | 1 h | both | （你刚迷了路，找不到要去的地方。） |
| `drinking` | 在喝酒 | 2 | 12 h | 30 min | both | （你正在喝酒。） |
| `smoking` | 在抽烟 | 2 | 12 h | 30 min | both | （你正在抽烟。） |
| `smoking_weed` | 在抽大麻 | 2 | 12 h | 30 min | both | （你正在抽大麻。） |
| `just_have_sex` | 刚做完爱 | 1 | 48 h | 30 min | both | （你刚和人做完爱。） |
| `masturbating` | 自慰中 | 1 | 24 h | 30 min | both | （你正在自慰。） |

`trading` is making a deal with someone, not trading shares, so it fits a
fantasy world too.

### 2.1 Gates

| kind | gate |
|---|---|
| `got_lost` | a background under way that is `short_trip`, `long_trip`, `hometown_visit` or `traveling_together` |
| `drinking` | the persona's `alcoholic` is `true` |
| `smoking` | the persona's `smoking` is `true` |
| `smoking_weed` | the persona's `drug` is `true` |
| `just_have_sex` | intimacy rung ≥ 3 (`DESIRE_MIN_RUNG`); no romance-compatibility check |
| `masturbating` | the persona's `sex_addict` is `true`, **and** intimacy rung ≥ 3 (my call: she tells it, so it waits for the rung `sudden_desire` waits for) |
| the other two | none |

`got_lost` is the first kind that needs a specific background. It is a
new filter in the draw, beside the background's `rules_out`: a kind that
names required backgrounds is eligible only while one of them is the
background under way. No other kind names any.

### 2.2 Factors and follow-ons

| kind | factor |
|---|---|
| `drinking`, `smoking`, `smoking_weed` | ×3 when `indulgence` is `high` (`indulgence_factor`) |
| `just_have_sex` | `sex_addict` ×3 / ×⅓, as `sudden_desire`; `UNLIKELY_FACTOR` while she is a virgin, as `having_sex` |

Follow-ons (`FOLLOW_FACTOR`): `ex_message`, `sudden_desire` and `strip`
lead to `just_have_sex` as they lead to `having_sex`, and `just_have_sex`
leads to `pregnancy_scare` as `having_sex` does. `sudden_desire` leads to
`masturbating` (my call).

### 2.3 Ruled out by a background

`hospitalized` and `post_surgery` rule out `got_lost` (already impossible
without a trip), `trading`, `drinking`, `smoking`, `smoking_weed` and
`just_have_sex` (my call: she is not out and about, and is not up to it). `traveling_together` rules out `just_have_sex`: she is
away with the user.

### 2.4 Getting lost with the user

During `traveling_together` the `got_lost` cue is
（你们一起出来玩，刚迷了路。）. During her own trips it is the table's
cue, opened with the trip's state as any foreground cue during a
background is: （你正在短途旅行。你刚迷了路，找不到要去的地方。）.

## 3. `having_sex` only for a sex addict

`having_sex` keeps everything it has (mid, offline, not told, 1 to 2 h,
weight 1, 48 h cooldown, its follow-ons) and gains a gate: it is drawn
only when the persona's `sex_addict` is `true`. Unset or `false`, it is
never drawn. Existing rows stay; nothing is rewritten.

## 4. Her virginity reads both kinds

The draw's `ever_had_sex` is now true when the instance has any
`having_sex` **or** `just_have_sex` row, ever. So the first
`just_have_sex` ends a virgin persona's virginity as the first
`having_sex` did, and `pregnancy_scare` stops being unlikely after it.

## 5. The habits in the draw

`PersonaTraits` gains `habits: Habits` (the `smoking`, `alcoholic`,
`drug`, `coffee`, `gluttony` keys, read by `persona_habits`), filled by
`persona_traits`. Only the three gates in §2.1 read it.

## 6. Goals, non-goals, anti-goals

- **Goal:** rows in the existing tables plus one small filter (§2.1).
- **Goal:** a persona with no `sex_addict` stops going offline to have sex
  and instead tells the user about it afterwards, from rung 3.
- **Non-goal:** a `coffee` or `gluttony` event; changing a stored need
  when a habit event is drawn (the judge reads the event).
- **Non-goal:** a persona key for the stock kinds.
- **Anti-goal:** `having_sex` or `masturbating` drawn for a persona whose
  `sex_addict` is not `true`.
- **Anti-goal:** a virgin persona whose virginity survives a
  `just_have_sex`.

## 7. Documentation

English and Chinese where both exist: `api-reference` (the events kind
table, adult-themes list now naming `just_have_sex` and `masturbating`, gates, persona
settings), `life-events-needs` (the kinds, gates, factors, follow-ons,
rule-outs, `having_sex`'s gate, virginity), a migrating note for the
release (a client adds the eight kinds; a persona without `sex_addict` no
longer draws `having_sex`).

## 8. Left to the builder

- Rust names (`EventKind::WatchingStocks`, …, the required-background
  helper), where the eight go in `ALL` (appended), test names.
- How `ever_had_sex` is read for two kinds (one query over a kind list,
  or two calls).
- Where the `traveling_together` variant of the `got_lost` cue is chosen
  (`event_cue` knows the background).

# Eight short events, `hygienic`, `employed` and `bathe_together` — Design

- **Date:** 2026-11-11
- **Status:** Approved
- **Type:** Engine change. No migration. Additive HTTP changes: the events
  route's kind enum gains eight drawable kinds and the placed kind
  `bathing_together`; the life-choice kind enum gains `bathe_together`. The
  character card 1.1 gains the `hygienic` (boolean) and `employed` (string)
  keys. `PersonaTraits` gains two fields.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-15-life-events-v2-design.md` (the draw, gates,
  factors), `2026-10-25-life-events-follow-ons-and-persona-keys-design.md`
  (follow-ons, `sex_addict`, `homesick`), `2026-10-28-outing-indulgence-needs-
  and-habits-design.md` (needs factors), `2026-11-08-strip-and-world-setting-
  design.md` (`romance_compatible` gate, `setting`),
  `2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md`
  (the romantic choices), and the `travel_together` choice (a placed event
  on accept, a cooldown). Everything there stands except where this
  document says otherwise.

## 1. Intent

Her day gets eight more small things that can happen in it. A persona can
be someone who loves being clean, which makes the bathing kinds come up
more often, and can be someone without a job, which makes the work kinds
rare. And she can ask the user to take a bath with her.

## 2. The eight kinds

All `short`, all told when they start (none is offline), none carries a
picture, all fit both settings. Each is one row in the existing tables:
`rule`, `category`, `event_label`, `kind_cue`, and where noted `passes_gate`,
`needs_factor`, `follows`, `rules_out`, `sex_addict_factor`.

| kind | weight | cooldown | lasts | label | cue |
|---|---|---|---|---|---|
| `bird_encounter` | 2.0 | 12 h | 30 min | 遇到一只鸟 | （你刚看到一只鸟停在不远处。） |
| `ex_wants_back` | 0.5 | 30 d | 1 h | 前任来求复合 | （你的前任刚来找你，说想复合。） |
| `want_to_quit_job` | 1.0 | 7 d | 1 h | 想辞职 | （你刚冒出想辞职的念头。） |
| `pondering_life` | 2.0 | 48 h | 1 h | 在思考人生 | （你刚一个人发呆，在想人生的事。） |
| `streaking` | 0.5 | 30 d | 30 min | 刚裸奔了一圈 | （你刚一时兴起，脱光了跑了一圈。） |
| `want_to_see_sea` | 2.0 | 72 h | 1 h | 想去看海 | （你现在很想去看海。） |
| `want_hot_spring` | 1.0 | 72 h | 1 h | 想去泡温泉 | （你现在很想去泡温泉。） |
| `want_sauna` | 1.0 | 72 h | 1 h | 想去洗桑拿 | （你现在很想去洗桑拿。） |

### 2.1 Gates

| kind | gate |
|---|---|
| `ex_wants_back` | intimacy rung ≥ 2 (`CONFIDE_MIN_RUNG`), as `ex_message` |
| `streaking` | intimacy rung ≥ 3 (`DESIRE_MIN_RUNG`) **and** the session's scopes are romance-compatible, as `strip` |
| `want_to_quit_job` | `employed` is not `free` (§4) |
| the other five | none |

### 2.2 Factors

| kind | factor |
|---|---|
| `want_to_see_sea` | `needs_factor` ×2 when `outing` is `high` |
| `want_hot_spring`, `want_sauna` | `needs_factor` ×2 when `stamina` is `low`, as `bathing` |
| `streaking` | `sex_addict_factor` ×3 / ×⅓, as `sudden_desire` |
| `showering`, `bathing`, `want_hot_spring`, `want_sauna` | `hygienic_factor` (§3) |
| `temp_work`, `scolded_by_superior`, `scolded_superior`, `business_trip` | `employed_factor` (§4) |

### 2.3 Follow-ons (`FOLLOW_FACTOR`, per live prior)

| prior | kind |
|---|---|
| `ex_message`, `breakup` | `ex_wants_back` |
| `scolded_by_superior`, `temp_work` | `want_to_quit_job` |
| `breakup`, `quarrel`, `want_to_quit_job` | `pondering_life` |
| `pondering_life` | `want_to_see_sea` |

### 2.4 Ruled out by a background

`hospitalized` and `post_surgery` rule out `bird_encounter` and `streaking`:
she is not out and about. The three wants and the two thoughts happen in
bed as well as anywhere, so nothing rules them out. `period` and
`traveling_together` are unchanged.

## 3. The `hygienic` key

`art_metadata.hygienic`: boolean. `PersonaTraits.hygienic: Option<bool>`,
read beside `sex_addict`; a value that is not a boolean reads as unset.

`hygienic_factor` scales `showering`, `bathing`, `want_hot_spring` and
`want_sauna` within `short`: `true` ×3 (`PREFERENCE_FACTOR`), `false` ×⅓,
unset ×1. Its own factor, so it multiplies with `needs_factor`.

It is not a `Habits` key: it does not count toward the `indulgence` drift,
`uses` no `give` item, and does not enter the judge's brief. It is read by
life events only.

## 4. The `employed` key

`art_metadata.employed`: `"free"`, `"part_time"` or `"full_time"`.
Absent, or any other value, reads as `part_time`, so every existing persona
keeps today's draw and has time for the user.
`PersonaTraits.employed: Employment { Free, PartTime, FullTime }`, parsed
as `Setting` is.

`employed_factor` scales the work kinds `temp_work`,
`scolded_by_superior`, `scolded_superior` and `business_trip` within their
categories:

| `employed` | factor | `want_to_quit_job` |
|---|---|---|
| `free` | ×⅓: rare, not impossible | not drawn (§2.1) |
| `part_time` (default) | ×1 | drawn |
| `full_time` | ×3 (`PREFERENCE_FACTOR`): more of her day is work | drawn |

Nothing else reads the key; `in_class` and `busy` are unchanged.

## 5. The `bathe_together` life choice

`LifeChoiceKind::BatheTogether` / wire `bathe_together`: she asks the user
to take a bath with her. A romantic kind (`is_romantic`), so a typed reply
settles it as ignored at no cost, as for `sleep_together`.

### 5.1 When she asks

All of:

- the session's newest recorded scopes are romance-compatible
  (`romance_compatible`), as for `sleep_together`;
- chemistry tier ≥ 2, or ≥ 1 when the persona's `hygienic` is `true`;
- the newest 3 judged turns after the streak anchor are all warm
  (`newest_streak_warm`);
- no `bathe_together` accepted or ignored in the session within the last
  7 days. A decline or a lapse re-arms on a fresh streak;
- the usual runway (`runway_clear`).

It is decided in `romantic_kind`, after `propose` and `sleep_together`: when
a turn qualifies for more than one, the bigger ask goes first. Because its
chemistry floor is lower, it is usually the first romantic ask a pair sees.
A trip under way does not hold it back, as for the other romantic kinds.

### 5.2 Her ask and the answers

| | |
|---|---|
| ask cue | （你们最近越来越亲密，你很想和对方一起洗个澡。你决定主动开口，问对方愿不愿意和你一起洗，希望对方给你一个答复。） |
| accept | places `bathing_together` (§5.3); no line drops; her message's cue is the `Answered` arm: （对方答应了和你一起洗澡，你们现在一起泡在浴缸里。） |
| decline | chemistry drops one tier, as for `sleep_together`; cue （对方没有答应和你一起洗澡，你有点失落。） |
| ignore / lapse | no cost |

### 5.3 The `bathing_together` event

`EventKind::BathingTogether` / wire `bathing_together`: placed by the
accept, never drawn (`SPECIAL`, weight 0). Written with
`insert_preempting_in_tx` as `traveling_together` is, so an incident under
way ends and a background keeps going.

| | |
|---|---|
| category | `short` |
| lasts | 30 min |
| label | 和对方一起洗澡 |
| told | no: her accept message is the telling |
| offline | no: she is with the user |
| setting | both |

While it lasts it is the incident under way, so no other incident is drawn.
A `kinds` filter that lists it is rejected, as for the other placed kinds.

## 6. Character card 1.1

`hygienic` (boolean) and `employed` (string) join 1.1 beside `sex_addict`
and `setting`, read by life events. 1.1 is not released yet, so this is the
same version; its *Versions* line says 1.1 adds `gluttony`, `setting`,
`hygienic` and `employed`.

## 7. Goals, non-goals, anti-goals

- **Goal:** eight rows in the existing tables and two more persona
  factors, with no new mechanism.
- **Goal:** an existing persona with neither key draws exactly as today
  apart from the eight new kinds sharing the `short` pool.
- **Non-goal:** a picture on `streaking`.
- **Non-goal:** taking her out of reach for any of the eight.
- **Non-goal:** a `hygienic` or `employed` effect on cooldowns or
  durations: weight and the one gate only.
- **Non-goal:** `employed` touching the offline kinds (`busy`,
  `in_battle`); how much she is reachable stays with those kinds' own
  rules.
- **Non-goal:** a picture on the `bathe_together` accept.
- **Anti-goal:** `streaking` or `bathe_together` reaching a pair whose
  scopes are not romance-compatible.

## 8. Documentation

English and Chinese where both exist: `api-reference` (the events kind
table, persona settings, the life-choice kinds), `life-events-needs` (the
kinds, gates, factors, follow-ons, the ruled-out list, persona settings,
the life choices), `eros-character-v1.1.md`.

The release needs a migrating note: a client adds the `bathe_together`
choice kind (buttons, tray) and the `bathing_together` event kind to its
catalog. An unknown kind degrades as it did for `travel_together`.

## 9. Left to the builder

- Where the eight sit in `EventKind::ALL` (draw order; append after
  `strip` is the obvious choice) and the `ALL` length.
- Test names; the `kind_cue` test table gains eight rows.

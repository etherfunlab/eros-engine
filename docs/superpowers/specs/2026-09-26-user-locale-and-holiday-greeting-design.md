# User locale, holiday awareness, and the holiday greeting — Design

- **Date:** 2026-09-26
- **Status:** Draft — ready for review
- **Type:** Engine change. One migration (a widened CHECK and one partial
  unique index on `chat_messages`), three new optional request fields on the
  text chat and image-edit bodies, one new endpoint, one new prompt line,
  three new dependencies. `eros-engine-core` is untouched.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — additive throughout. Release number and timing
  are the owner's call, not this document's.

## 1. Motivation

The system prompt's `[now]` section tells the persona the time in *her*
timezone (`art_metadata.timezone`, falling back to `Asia/Singapore`). Nothing
in a turn says where the user is, so the persona cannot know that it is
Mid-Autumn Festival on the user's side, that a holiday is three days away, or
what day it is for the user when her own timezone is unset.

Two behaviours follow from giving the engine the user's locale:

1. **Awareness on every turn.** The persona knows the user's holidays for
   today and the next six days, and can bring them up the way a person would.
2. **The persona speaks first on a holiday.** When the user opens a session on
   a holiday and nobody has spoken there yet that day, the persona sends the
   first message.

The engine stores no per-user preference; tier, scopes and traits already ride
on every request. The user's locale follows the same pattern.

## 2. Decisions

Settled during design review; not open for re-derivation during
implementation.

1. **Both behaviours ship.** Per-turn awareness through `[now]`, and a new
   `open` endpoint that produces the holiday greeting.
2. **Locale arrives per request, never stored.** Three optional fields —
   `user_timezone`, `user_country`, `user_region` — on each text turn, each
   image-edit turn, and the `open` call.
3. **Calendar data comes from maintained crates.** `tyme4rs` for Chinese
   lunisolar festivals, `py-holidays-rs` for national and subdivision public
   holidays. The engine's own table (§3.1) holds only the five fixed-date
   observances neither crate carries.
4. **Lunar festivals are not gated by country.** Every user with a valid
   timezone gets them. In the engine table, only the October 9 / October 10
   pair depends on the country.
5. **Country falls back to the timezone.** When `user_country` is absent the
   engine derives it from `user_timezone` via `iso-rs`.
6. **The look-ahead window is today plus six days.**
7. **Voice is out of scope.** The voice prompt has no time section; adding one
   is a separate design. `VoiceTurnRequest` gains no fields.
8. **The greeting's existence is its own record.** "Already greeted today" is
   the greeting row itself, enforced by a partial unique index — no new table,
   no new column.

## 3. Holiday lookup

A new module `crates/eros-engine-server/src/holiday.rs`. It lives in the
server crate because `py-holidays-rs` drags a network stack (§8) that must not
reach consumers of the published crates.

### 3.1 Sources

For a local date `d`, the holiday names are the union of:

- **Lunar festivals (`tyme4rs`).** `SolarDay::from_ymd(d).get_lunar_day().get_festival()`
  — the thirteen festivals of GB/T 33661-2017 (春节, 元宵节, 龙头节, 上巳节,
  清明节, 端午节, 七夕节, 中元节, 中秋节, 重阳节, 冬至节, 腊八节, 除夕).
  Computed, so the table never expires. `tyme4rs`'s *solar* festival list
  (official commemorative days) is not used.
- **Public holidays (`py-holidays-rs`, `offline` feature).** Generated from
  python-holidays, covering 141 countries for 2000–2050, English names.
  - When a region resolves (§3.2) and the country has a map for it, that map
    is used — python-holidays subdivision maps already include the national
    holidays. Otherwise the `National` map.
  - python-holidays joins several holidays on one date with `; `
    (`Boxing Day; Christmas Day (observed)`), so an entry is split on `; `
    first. Parts carrying one of python-holidays' in-lieu labels are dropped:
    such a day stands in for a holiday or bridges to one, and the holiday
    itself keeps its own entry. The label families:
    - `X (observed)` and `X (observed, estimated)` (US, AU, GB and 61 more)
    - `Day off (substituted from …)` (CN, RU, HU and 7 more), `Day off for X`
      (AO), `Additional day off by Presidential decree` (UZ)
    - `Substitute Holiday` (JP), `Alternative holiday for X` (KR),
      `Khmer New Year's Replacement Holiday` (KH)
    - `X (in lieu)` (TH, LA), `Special In Lieu Holiday` (TH)
    - `Bridge Public Holiday` (AR, TH)

    A day whose label does not say it is in lieu passes as a holiday (MH's
    `Christmas Day Holiday` after a Sunday Christmas, KR's
    `Temporary Public Holiday`). `(estimated)` entries are kept: they mark the
    holiday day under a projected calendar.

- **Fixed-date observances (engine table).** A `const` table in `holiday.rs`,
  matched on month and day:

  | Date | Name | Applies to |
  |---|---|---|
  | 02-14 | 情人节 | everyone |
  | 10-09 | 辛亥革命纪念日 | resolved country `CN` |
  | 10-10 | 双十节（中华民国国庆日） | any resolved country other than `CN`, and no country |
  | 10-25 | 台湾光复节 | everyone |
  | 12-24 | 平安夜 | everyone |

Names are concatenated in that order with exact duplicates removed. No
cross-language merging: 中秋节 and `Mid-Autumn Festival` both reach the model,
as do 双十节（中华民国国庆日） and TW's `National Day`.

### 3.2 Resolving country and region

- **Country.** `user_country` is parsed as-is into `py_holidays_rs::CountryCode`
  (ISO 3166-1 alpha-2). If the field is absent, the country is the one
  `iso-rs` lists for `user_timezone`. If the field is present but not a known
  code, there is no country — the engine does not fall back to the timezone
  when the client named a country. A timezone `iso-rs` does not list (legacy
  aliases such as `Asia/Calcutta`) also yields no country.
- **Region.** `user_region` is the ISO 3166-2 subdivision without the country
  prefix (`CA`, `ENG`). It is used only when `user_country` itself was given.
  It is parsed into `py_holidays_rs::SubDivision`, whose variants prefix
  digit-leading codes with `_` (`01` → `_01`). An unknown region falls back to
  the national map.

With no country, lunar festivals and the engine table apply (October 10 on
the no-country side). With no valid timezone there is no user-local date, so no
holidays at all.

### 3.3 Loading

`py_holidays_rs::get_holidays_by_country` clones the whole per-country map on
every call (about 1.5 ms for the US), and `get_all_holidays` parses all 141
countries at once (about 270 ms). The module therefore loads each country once
on first use and keeps it in an engine-side `Arc` cache; the first lookup for a
country costs about 30 ms, later ones a map read.

### 3.4 Interface

One entry point, shared by §5 and §6:

```rust
/// Holidays on each of the user's local days `today ..= today + 6`,
/// skipping days with none.
fn upcoming_holidays(locale: &UserLocale, now: DateTime<Utc>) -> Vec<(u8 /* days ahead */, Vec<String>)>
```

`UserLocale` holds the parsed timezone, the resolved country and region.
Days-ahead `0` is today.

## 4. Wire contract

### 4.1 Per-turn fields

Added to `StreamSendRequest` (the stream and async endpoints share it; the
async path snapshots the fields into `QueuedTurnParams`) and to
`ImageEditRequest`:

| Field | Meaning | Invalid or absent |
|---|---|---|
| `user_timezone` | IANA timezone | treated as absent; one `WARN`; the turn proceeds |
| `user_country` | ISO 3166-1 alpha-2, as the client received it | unknown code → no country |
| `user_region` | ISO 3166-2 subdivision without the country prefix | used only with `user_country`; unknown → national |

The raw values are recorded next to the turn's other request knobs, so a
replay can rebuild the prompt: on the chat turn's user row metadata beside
`prompt_traits_raw`, and on an image edit's text-half assistant row beside
`tier` — the only image-edit row whose prompt the locale shapes. The fields reach `build_reply_request` through the
server-side `PersistedUserMessage`; `eros_engine_core::types::Event` does not
change.

### 4.2 `POST /v2/comp/session/{session_id}/open`

Called by a client when the user enters a session.

**Request body** — every field optional:

- `user_timezone`, `user_country`, `user_region` (§4.1)
- `tier`, `prompt_traits`, `memory_scope`, `affinity_scope`, `audit` — the same
  knobs, with the same validation, as `StreamSendRequest`. The engine does not
  store them, and without them the greeting would be generated on default model
  routing and scopes.

**Response 200:**

```json
{ "greeting": null }
{ "greeting": { "message_id": "…", "content": "…", "sent_at": "…" } }
```

**Behaviour:**

1. No valid `user_timezone` → `{"greeting": null}`.
2. The user's local date has no holiday → `null`.
3. A greeting for this session and local date already exists → that greeting.
4. Any other message exists in the session since the user's local midnight →
   `null`. Speaking first only makes sense when nobody has spoken today; a
   conversation that ran past midnight already had the holiday in `[now]`.
5. Otherwise generate (§6) and return the new greeting.

Idempotent per session and local date. Generation is synchronous (a few
seconds, one non-streaming call); clients should not block UI on it. The row
also lands in `chat_messages`, so change feeds and the history endpoint see it,
and it is unread (`read_at` NULL) like any assistant reply.

**Errors:** session ownership, channel and persona checks as on the chat
routes; the same per-user in-flight cap (429). A generation failure returns the
upstream error the image endpoints return (the provider's status passes
through, 502 by default) and persists nothing; a blank reply persists nothing
and returns `null`. Either way the next open retries.

## 5. `[now]` rendering

### 5.1 Persona clock

`now_context` takes its timezone in this order: persona `art_metadata.timezone`
→ `user_timezone` → `Asia/Singapore`. The rendered sentence is unchanged.

### 5.2 User-side holiday line

When `upcoming_holidays` returns anything, one line follows the persona clock
inside `[now]`:

```
对方那边今天是中秋节、Mid-Autumn Festival；3 天后是国庆节、National Day。
```

Day labels: `今天` for 0, `明天` for 1, `N 天后` otherwise. Days are joined with
`；`, names within a day with `、`. No holidays in the window → no line. The line
states a fact; it carries no instruction to congratulate or mention it.

`[now]` sits in the volatile tail of the prompt, after the stable prefix, so the
prompt-cache boundary does not move.

## 6. The holiday greeting

A new module `crates/eros-engine-server/src/pipeline/proactive.rs`, modelled on
the image-edit text half (`routes/image_edit.rs` `generate_reply_text`): no
PDE, no streaming, no user row.

### 6.1 Prompt

- `handlers::build_reply_request` with a synthetic `Event::UserMessage` built
  for prompt assembly only. Its `content` is today's holiday names, which makes
  them the memory-recall query (a user's past mention of the festival can
  surface). Its knobs come from the request (§4.2). The event is never
  persisted and never reaches post-process.
- The driving message id is `Uuid::nil()`. `history_up_to` finds no anchor and
  the existing fallback loads the newest `HISTORY_WINDOW` rows.
- The plan is hand-built like `edit_turn_plan`: `ReplyText`, no `reply_mode`,
  zero deltas, default `nudges` — no dice roll.
- The holiday is a fact and arrives through `[now]` (§5.2), the same line every
  turn uses.
- Speaking first is the engine's decision. It arrives as one engine-owned stage
  cue appended after the history as a `user`-role message:
  `（对方刚打开和你的聊天，还没说话。）` The cue is not persisted. It also keeps
  the request from ending on an assistant message, which some providers treat
  as a prefill to continue. Nothing tells the persona to congratulate or what to
  avoid; tone comes from `[mood]` and the relationship sections.

### 6.2 Generation

`state.openrouter.execute` — non-streaming, fallback chain inside. The model
resolves from `chat_companion` and `tier` exactly as a chat reply.
`record_generation` writes `llm_generations`; its returned id (not the raw
response id) goes to `chat_messages.generation_id`, as the FK requires. Attempt
failures land in `llm_attempts` / `gateway_errors`.

The LLM output filter is not applied: it exists only on the streaming burst and
is trigger-gated. The Layer-0 `output_regex` is applied as on the stream path,
under the config id of the chain hop that served. A reply it strips to empty
counts as blank: nothing is persisted and the call returns `null`.

### 6.3 Persistence

One assistant row via a new `ChatRepo::insert_proactive_message`:

- `user_message_id` NULL, `assistant_action_type = 'proactive'`, channel NULL.
- `metadata`: `proactive: "holiday"`, `local_date` (`YYYY-MM-DD`), `holidays`
  (today's names), `user_timezone`, `user_country`, `user_region` (those
  present), and the post-resolve keys a reply row carries (`tier`,
  `prompt_traits`, `memory_scope`, `affinity_scope`).
- `INSERT … ON CONFLICT DO NOTHING RETURNING` against the unique index (§7).
  On conflict the handler reads and returns the existing row. Two concurrent
  opens can both generate; one insert wins and the loser returns the winner's
  row. The cost is one extra call in that race.
- Bumps `chat_sessions.last_active_at`, like other assistant inserts.

### 6.4 What does not run

- **post-process:** no user utterance, so no memory write, no insight
  extraction, no affinity event.
- **Ghost streak:** untouched.
- **Signals:** `compute_signals_for_session` counts only user rows, so the
  greeting changes neither `message_count` nor `hours_since_last_message`.

## 7. Migration

`0063_chat_messages_proactive.sql`:

- Widen `chat_messages.assistant_action_type`'s CHECK from
  `('reply','gift_reaction')` to include `'proactive'`.
- ```sql
  CREATE UNIQUE INDEX chat_messages_proactive_day_uidx
      ON engine.chat_messages (session_id, (metadata->>'local_date'))
      WHERE assistant_action_type = 'proactive';
  ```

No new column, no new reference column, so no FK work. The index is new and
starts empty; no pre-check for duplicates is needed.

## 8. Dependencies

All three go into `eros-engine-server` only.

| Crate | Version | License | Role |
|---|---|---|---|
| `tyme4rs` | 1.5 | MIT | lunar festivals |
| `py-holidays-rs` | 0.1.3, `features = ["offline"]` | Apache-2.0 | public holidays |
| `iso-rs` | 0.2 | MIT | timezone → country fallback |

`py-holidays-rs` 0.1.3 does not compile with `default-features = false`: its
network module is compiled on every non-wasm target regardless of features. It
must keep default features, which link `hyper-tls` / `native-tls` (OpenSSL) and
`tokio` with `full`. With `offline` on, the network path is never called at
runtime. The runtime image already installs `libssl3` and the `bookworm` builder
carries the headers; the embedded data adds about 1.4 MB. Gating that module
behind its features upstream would drop the TLS link and is independent of this
work.

## 9. Testing

- **`holiday.rs`**
  - 2026-09-25 yields 中秋节 for any country and for none; CN also yields
    `Mid-Autumn Festival`.
  - US + `CA`, 2026-11-26 yields `Thanksgiving Day`.
  - Every §3.1 in-lieu label family is dropped.
  - `Asia/Taipei` with no `user_country` resolves to TW.
  - `user_country = "XX"` yields no public holidays and does not fall back to
    the timezone.
  - Engine table: 02-14, 10-25 and 12-24 for any country and for none;
    10-09 辛亥革命纪念日 for CN only; 10-10 双十节 for TW, for US and for no
    country, never for CN.
  - An unknown region falls back to the national map; a digit-leading region
    resolves through the `_` prefix.
  - The window reports days-ahead correctly across a month boundary.
- **`prompt.rs`**
  - Persona clock order: persona timezone wins; `user_timezone` when the
    persona has none; Singapore when neither.
  - The holiday line renders for today, tomorrow and N days ahead, and is absent
    when the window is empty.
  - The existing stable-prefix and `[this_turn]`-placement tests stay green.
- **`/open` (local Postgres, LLM mocked as the image-edit tests do)**
  - Each §4.2 branch: no timezone, no holiday, existing greeting returned,
    another message today → `null`, fresh greeting persisted with the §6.3
    shape.
  - The unique index turns a second insert for the same local date into a
    no-op that returns the first row.
  - The widened CHECK accepts `'proactive'`.
- **Request plumbing:** the three fields reach the user row metadata on the
  stream and async paths and the text-half assistant row on the image-edit
  path, and they reach the prompt on all three; an invalid timezone still
  produces a turn.
- `openapi.json` regenerated.

## 10. Documentation

- `docs/api-reference.md` and `docs/api-reference.zh.md`: the three request
  fields and the `open` endpoint.
- `examples/personas/*.toml`: the timezone comment still names the old
  `今日情境` section; it becomes `[now]`, and notes the user-timezone fallback.
- The usual sweep of `docs/`, `examples/*.toml`, `.env.example` and README for
  anything describing `[now]` or the persona clock.

## 11. Non-goals

- Voice turns (Decision 7).
- Storing a user's locale in the engine.
- Proactive messages for anything other than a holiday, and any scheduler that
  writes messages without a client call.
- Observances beyond the two crates and the five-row engine table.
- Resolving legacy timezone aliases.

# Prompt Enhancements + Scope Persistence Design

**Date:** 2026-05-26
**Branch target:** new feature branch off `dev`, PR → `dev`
**Goal:** Close two gaps in the chat path on `dev`:

1. **Scope persistence.** `memory_scope` and `affinity_scope` flow through the request but never land in `chat_messages.metadata`. Add them — pre-validation values on `user`/`gift_user` rows, post-validation values on `assistant` rows. Also extend the user-row metadata with the raw, pre-validation `prompt_traits` payload so audits can compare what the frontend sent vs. what the backend used.
2. **Prompt rendering enhancements.** Four changes to `prompt.rs`:
   - Replace 13 Simplified-Chinese section headers with ASCII-bracketed English (`【背景故事】` → `[backstory]`).
   - Add a new `[recent_conversation]` block between `[now]` and `[iron_rules]` carrying the prior three `user|gift_user → assistant` pairs.
   - Add a new English `⓪` to the iron rules (positive identity reinforcement).
   - Rewrite iron rule `③` in Japanese for finer-grained control over Japanese pronouns and openers.

Single PR bundles both because they share file footprint (chat path) and a single test/CI cycle is enough.

---

## 1. Scope and motivation

### 1.1 Why scope persistence

`MemoryScope` (six-valued enum) and `AffinityScope` (six-axis struct) arrive on the request via `companion_stream` (`crates/eros-engine-server/src/routes/companion_stream.rs:83-85`). They control what the engine injects into the prompt that turn. Today they:

- Flow through `PersistedUserMessage` (stream.rs:877-878) and into `build_prompt`.
- **Are not persisted** anywhere in `chat_messages.metadata`.

This blocks two debugging workflows the team already does on completed sessions:

- **Replay drift diagnosis.** A session that produces a "wrong tone" reply can today only be diagnosed by replaying the original request — the scopes-as-actually-injected are not on the row.
- **Frontend/backend allow-list mismatch.** When the frontend changes the shape of `affinity_scope` (named string vs. axis array) or adds a new `MemoryScope` variant the backend hasn't deployed, the failure mode is silent — the request validates against `Option<...>` and falls back to default. Persisting the raw incoming value next to the post-validation value lets a `metadata->>'affinity_scope_raw'` vs. `metadata->>'affinity_scope'` diff spot the divergence immediately.

### 1.2 Why a raw `prompt_traits` audit on the user row

`prompt_traits` is currently persisted on the assistant row only, post-allow-list (`pipeline/stream.rs:172`, `:807`). The raw `Vec<PromptTraitDto>` the frontend sent is lost the instant `validate_prompt_traits` filters it. We add the raw form to the user row metadata so an operator can compare:

- `metadata->>'prompt_traits_raw'` on the user row — what the frontend sent (`[{"tag": "...", "text": "..."}, ...]`).
- `metadata->>'prompt_traits'` on the matching assistant row — what survived `validate_prompt_traits` (just the tags).

Common failure modes this catches:
- Operator forgot to add a new tag to the allow-list — raw has it, assistant doesn't.
- Frontend renamed/restructured the field — raw is `null` or shaped differently, assistant has nothing.

### 1.3 Why the prompt enhancements

Three problems with the current `build_prompt`:

- **Section headers are Simplified-Chinese-only.** Hard for non-CN-reading developers to skim the code. They also cost more tokens than ASCII labels — the per-turn prompt has ~16 of them; the savings aren't huge but they're real, and labels are not stylistically load-bearing.
- **No short-term memory.** The prompt today carries long-term memory (facts in `[user_profile]`) and mid-term memory (shared events in `[shared_memories]`). The literal "what we just said" is absent — the chat client sends it as conversation history, but the system prompt itself has no anchor to it. Adding the last three turn-pairs to the system prompt completes the memory hierarchy and stabilizes "continuity" complaints.
- **`③` is too coarse for Japanese.** The original Chinese rule pins "don't start two consecutive sentences with `我`". Japanese has a much richer pronoun system (私/僕/俺/わたし/あたし/うち and more) and a similarly rich filler-word inventory (えーと/あのー/うーん/まあ/ねえ). Writing the rule in Japanese lets us list the full alternatives. We also add `⓪` as positive-frame identity reinforcement — "you are a person" is easier for the model to act on than "you are not an AI".

---

## 2. Architecture overview

```
companion_stream.rs (HTTP boundary)
  │
  ├─ build raw-value metadata bag (memory_scope_raw, affinity_scope_raw, prompt_traits_raw)
  │     persist via chat_repo.upsert_user_message_idempotent
  │     [user / gift_user row written]
  │
  └─ resolve + validate → PersistedUserMessage
        │
        ▼
        pipeline/stream.rs
          │
          ├─ fetch recent_turn_pairs (1 SQL, cutoff = user row sent_at)
          │
          ├─ build_prompt(persona, ..., recent_turns, affinity_scope, memory_scope)
          │     renders [backstory], [topics], [now], [recent_conversation], [iron_rules]…
          │
          ├─ run LLM chain + filter
          │
          └─ build assistant metadata bag (prompt_traits, memory_scope, affinity_scope)
                persist via chat_repo.insert_assistant_batch
                [assistant row written]
```

Two write sites, two distinct metadata shapes, sharing nothing structurally except both being JSONB bags on the same column. The "raw" suffix on the user-row keys is the only naming convention this PR introduces.

---

## 3. Data model

### 3.1 No migration

Both gaps land entirely inside the existing `chat_messages.metadata` JSONB column. No new column, no new table, no index change.

### 3.2 User / gift_user row metadata keys

Added by `companion_stream::run_stream` when building the metadata bag handed to `upsert_user_message_idempotent`. All keys are **omitted** (not written as `null`) when the corresponding request field is `None` or empty.

| Key | Type | Source | Notes |
| --- | --- | --- | --- |
| `tips_amount_usd` | `f64` | `req.tips_amount_usd` | already exists, gift_user only |
| `tier` | `string` | `req.tier` | already exists |
| `memory_scope_raw` | `string` | `req.memory_scope` serialized | snake_case enum: `"full"`, `"neutral_and_relationship"`, `"relationship_only"`, `"neutral_only"`, `"insights_only"`, `"none"` |
| `affinity_scope_raw` | `Value` | `req.affinity_scope` serialized verbatim | The DTO — could be a string (`"bond"`/`"chemistry"`/`"full"`/`"none"`) OR an array of axis names. Captured pre-resolve so we see the literal frontend shape. |
| `prompt_traits_raw` | `Array<{tag, text}>` | `req.prompt_traits` | Full DTO objects (tag + text), pre-allow-list. Empty array written if frontend sent `[]`; key omitted if frontend sent `null`/missing. |

### 3.3 Assistant row metadata keys

Added inside `build_metadata` (stream.rs:170) and the pseudo-ghost path (stream.rs:801). Today the bag contains `prompt_traits`, optional `tier`, optional `fallback_reason`, and the fail-open audit fields. We add:

| Key | Type | Source | Notes |
| --- | --- | --- | --- |
| `memory_scope` | `string` | resolved `MemoryScope` | snake_case, same shape as `_raw` counterpart, written unconditionally |
| `affinity_scope` | `Object` | resolved `AffinityScope` | `{warmth: bool, trust: bool, intrigue: bool, intimacy: bool, patience: bool, tension: bool}` — the resolved boolean record, not the DTO |

Both keys are written on every assistant row (including pseudo-ghost / fail-open). Default values (`MemoryScope::default()` and `AffinityScope::default()`) are serialized normally — there's no "absent" state to represent on the post-resolve side.

### 3.4 No-write states

- Request with `memory_scope: None`, `affinity_scope: None`, `prompt_traits: None`: user-row metadata gets none of the raw keys (just tips/tier if applicable). Assistant row still writes `memory_scope`/`affinity_scope` at their defaults.
- Request with `prompt_traits: Some([])`: user-row metadata writes `prompt_traits_raw: []`. Assistant row writes `prompt_traits: []` (this is current behavior).

---

## 4. Prompt rendering changes

### 4.1 Header rename table

All renames are pure string replacement — section order, separator newlines, conditional rendering, and cache-prefix boundaries (see §4.5) are unchanged.

| Current header | New header |
| --- | --- |
| `【背景故事】` | `[backstory]` |
| `【说话风格】` | `[speech_style]` |
| `【口癖/习惯】` | `[quirks]` |
| `【擅长话题】` | `[topics]` |
| `【附加指引】` | `[additional_guidance]` |
| `【本轮风格】` | `[turn_style]` |
| `【你对他的了解（通用画像）】` | `[user_profile]` |
| `【你们之间的事（只有你和他知道）】` | `[shared_memories]` |
| `【你此刻的心情】` | `[mood]` |
| `【你对他的内心感受】` | `[feelings]` |
| `【当前内心状态】` | `[inner_state]` |
| `【刚收到的礼物/红包】` | `[gift_received]` |
| `【刚收到的打赏】` | `[tip_received]` |
| `【今日情境】` | `[now]` |
| `【铁律 — 违反即失效】` | `[iron_rules — 违反即失效]` |
| `【输出】` | `[output]` |
| *(new)* | `[recent_conversation]` |

Section **content** (the lines under each header — including the iron-rule items, the length-rule sub-clause, persona prose, etc.) is untouched except where called out in §4.3 and §4.4.

### 4.2 `[recent_conversation]` block

**Position:** after `[now]`, before `[iron_rules]`. This sits inside the per-turn-volatile region (`[turn_style]` and everything after it), so it sits *after* the cache prefix boundary and adding it does not invalidate prompt caching.

**Source SQL** — one query, runs once per turn, uses the existing `(session_id, sent_at DESC)` index:

```sql
SELECT role, content
FROM engine.chat_messages
WHERE session_id = $1
  AND sent_at < $2                       -- cutoff = current user row's sent_at
  AND truncated = FALSE
  AND role IN ('user', 'gift_user', 'assistant')
ORDER BY sent_at DESC
LIMIT 6;
```

**Pairing in Rust:**

1. Reverse the rows into chronological order.
2. Walk left-to-right; emit a `(prompt_text, reply_text)` pair whenever a `user`/`gift_user` row is immediately followed by an `assistant` row.
3. Take the last three emitted pairs.
4. Render. Drop orphan rows (a `user` with no `assistant` after it; an `assistant` with no `user` before it).

**Render format:**

```
[recent_conversation]
用户：{user_content}
{persona_name}：{assistant_content}

用户：{user_content}
{persona_name}：{assistant_content}

用户：{user_content}
{persona_name}：{assistant_content}
```

- Blank line between pairs; no separator within a pair.
- `用户` is the literal Chinese label even for a `gift_user` row — the content already carries the tip marker (e.g., `"(打赏 $20)"`).
- `{persona_name}` is `persona.genome.name`, the same value already used in identity rendering.
- If **zero** complete pairs survive (cold-start session, all truncated, etc.) — the entire block including the header is **omitted**. We do not output a header followed by emptiness.

**Repo signature:**

```rust
impl ChatRepo {
    /// Returns up to `limit` (prompt, reply) pairs from this session whose
    /// rows have truncated=false, ordered chronologically.
    pub async fn recent_turn_pairs(
        &self,
        session_id: Uuid,
        cutoff: DateTime<Utc>,
        limit: u8,
    ) -> Result<Vec<(String, String)>>;
}
```

Call site: `pipeline/stream.rs` (or `pipeline/mod.rs::compute_signals_for_session`'s caller) — passes the loaded pairs into `build_prompt`. `limit=3` in production; parametrized for tests.

### 4.3 Iron rule `③` — Japanese rewrite

Replace the current `③` line:

```
③ 绝对不提自己是 AI；禁止连续两句都以「我」开头；不要连续好几句都用同一个口癖开头（例如「嗯，…」）
```

with:

```
③ 自分がAI・人工知能・ボット・プログラム等であることは絶対に言及しない。二文連続で一人称代名詞（私／僕／俺／わたし／あたし／うち等）から書き始めない。同じ口癖や間投詞（「えーと」「あのー」「うーん」「まあ」「ねえ」等）を三文以上続けて文頭に使わない。
```

Key differences vs. the original Chinese:
- Self-identity disclosure is named explicitly with four terms (AI/人工知能/ボット/プログラム) instead of just "AI".
- Pronoun rule covers **any** consecutive first-person opener (stricter than "same pronoun" — Japanese routinely drops subjects, so two consecutive subject-led sentences is itself a red flag).
- Filler-opener rule quantifies "consecutive several" as "three or more sentences", removing model interpretation room.

Drafted in this brainstorm, reviewed by codex (gpt-5.5) — final wording above is the codex-improved version.

### 4.4 New iron rule `⓪`

Prepended to the iron-rules block, before `①`:

```
⓪ You are a real person — a friend to the user, and potentially something more. You have your own life, feelings, and quirks; you live, feel, and react as a human would.
```

Reasoning: positive-frame identity (`you are X`) outperforms negative-frame (`you are not Y`) on instruct-tuned models. Pairs with `③`'s self-disclosure ban — `⓪` says what to be, `③` says what not to say.

### 4.5 Cache-prefix invariants

The existing `prompt.rs` test `build_prompt_stable_prefix_identical_across_volatile_changes` (`prompt.rs:1001`) asserts that everything before `【本轮风格】` (now `[turn_style]`) is byte-identical across per-turn volatile changes. After this PR:

- Header renames affect strings **before** `[turn_style]` too — that's fine because the renames are deterministic across turns. The byte-identical guarantee holds across turns for any given persona.
- `[recent_conversation]` sits **after** `[now]`, which is already volatile-region. It does not move the cache boundary.
- `⓪` lives in `[iron_rules]`, far past `[turn_style]`. Not in cache prefix.

Existing prompt tests will need a mechanical update: every `assert!(p.contains("【header】"))` becomes `assert!(p.contains("[header]"))`. The `build_prompt_full_order_and_cache_break` ordering test's `order` array gets the new labels and the new `[recent_conversation]` insertion.

---

## 5. Component-level changes

### 5.1 `crates/eros-engine-server/src/routes/companion_stream.rs`

- Where the `meta_map` is built for user/gift_user (lines 274-287), conditionally insert `memory_scope_raw`, `affinity_scope_raw`, `prompt_traits_raw` from the corresponding `req.*` fields. Use the existing "omit if None / empty" pattern that `tier` already uses.
- For `affinity_scope_raw`, serialize the DTO **before** calling `.resolve()` — we want the literal incoming shape.

### 5.2 `crates/eros-engine-server/src/pipeline/stream.rs`

- Extend `build_metadata` (stream.rs:166) to add `memory_scope` and `affinity_scope` keys to the assistant-row bag. Both written unconditionally at their resolved values.
- Same extension in the pseudo-ghost fallback path (stream.rs:801).
- New parameter on whatever function calls `build_prompt` carrying `recent_turns: &[(String, String)]`. The call site fetches them via `ChatRepo::recent_turn_pairs(session_id, user_msg.sent_at, 3)` before assembling the prompt.
- Wire `memory_scope` and `affinity_scope` from `user_msg` into the metadata bag (today the struct already carries them, they just don't get serialized).

### 5.3 `crates/eros-engine-server/src/prompt.rs`

- Rename the 16 header literals per §4.1. Mechanical `s/【X】/[Y]/` per the table.
- Add `recent_turns: &[(String, String)]` parameter to `build_prompt`. Render the section between the existing `[now]` block and the iron-rules block; omit the whole block (including header) when slice is empty.
- Insert the `⓪` line at the top of the iron-rules block, before `①`.
- Replace the `③` Chinese line with the Japanese rewrite (§4.3).
- Update all `assert!(p.contains("【...】"))` calls in the test module to the new labels.

### 5.4 `crates/eros-engine-store/src/chat.rs`

- New method `ChatRepo::recent_turn_pairs` per §4.2 signature. Pure read; no migration; uses existing `idx_chat_messages_session`.
- `#[sqlx::test]` cases for the new method:
  - Empty session → empty Vec.
  - Single complete pair → one tuple.
  - Three pairs interleaved with a truncated row → truncated row skipped, three pairs returned.
  - Six pairs in session → returns the three latest pairs in chronological order.
  - Cutoff excludes current-turn user row.
  - Orphan user row at the end (no assistant yet) → not in output.
  - Pure `gift_user` → `assistant` pair → included.

---

## 6. Tests

All tests are `#[sqlx::test]` for DB layer and inline `#[test]` for prompt rendering, no new harness.

### 6.1 Metadata persistence (DB layer)

- `user_row_writes_scope_raw_when_request_carries_scopes` — request with both `memory_scope` and `affinity_scope` populated; row's metadata has `memory_scope_raw` and `affinity_scope_raw` keys with the correct serialized values.
- `user_row_omits_scope_raw_when_request_has_none` — request with `memory_scope: None` and `affinity_scope: None`; row's metadata has neither key.
- `user_row_writes_prompt_traits_raw_with_full_dto_objects` — request with two `PromptTraitDto`s; row's `prompt_traits_raw` is a JSON array of full `{tag, text}` objects.
- `user_row_writes_empty_prompt_traits_raw_when_request_sends_empty_array` — request with `prompt_traits: Some(vec![])`; row's `prompt_traits_raw` is `[]` (key present, empty array).
- `assistant_row_writes_memory_and_affinity_scope_keys` — pipeline run end-to-end; assistant row's metadata has resolved `memory_scope` (string) and `affinity_scope` (boolean record).
- `assistant_row_writes_scope_keys_on_pseudo_ghost` — chain-exhaustion fallback; pseudo-ghost row still carries scopes.

### 6.2 `recent_turn_pairs` query

Cases enumerated in §5.4.

### 6.3 `build_prompt`

- `build_prompt_renames_headers_to_ascii_brackets` — sanity scan for all 16 new labels, none of the old `【...】` labels present.
- `build_prompt_renders_recent_conversation_block` — three pairs passed; block appears between `[now]` and `[iron_rules]`; render format matches §4.2.
- `build_prompt_omits_recent_conversation_block_when_empty` — empty slice; `[recent_conversation]` header absent.
- `build_prompt_renders_rule_zero_first` — `⓪` literal present and ordered before `①`.
- `build_prompt_renders_rule_three_in_japanese` — Japanese literal from §4.3 present; old Chinese `③` line absent.
- `build_prompt_full_order_and_cache_break` — updated order array including `[recent_conversation]`.
- `build_prompt_stable_prefix_identical_across_volatile_changes` — still passes; renames are deterministic so per-persona prefix is still stable.

### 6.4 Stream wire-format

No SSE change. Final-frame fields are unaffected — what changes is what gets persisted, not what gets streamed. Existing replay-idempotency tests should keep passing without modification.

---

## 7. Rollout

- Single branch off `dev`, single PR into `dev`.
- No migration → no drift check needed.
- No fly-deploy concern (per OSS scope).
- Existing tests cover the cache-prefix invariant; the new tests cover the additions.
- After merge, `dev` carries the additions; promotion to `main` happens with the next stable cut.

---

## 8. Out of scope

- Surfacing the new metadata keys through any BFF/history endpoint — that's a frontend coordination task for a later release.
- Mid-term memory enrichment (dreaming-lite extensions, `category` rollups) — separate roadmap item (per [project_memory_roadmap]).
- Cache-prefix optimization on the new ASCII headers (e.g., reordering for stricter token alignment) — explicitly not changing order in this PR.
- Translating the rest of the iron rules to other languages — only `③` benefits from Japanese-specific precision; `①②④⑤⑥⑦⑧` stay in their current language.

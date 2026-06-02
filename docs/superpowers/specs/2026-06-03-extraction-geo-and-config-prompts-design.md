# eros-engine — Geo insight fields + extraction-prompt precision + config-driven extraction prompts (Spec B2)

**Status**: design, pending implementation plan
**Target release**: `0.5.x` dev track. **One migration (`0026`).**
**Scope**: the second of two specs splitting the original "Spec B" extraction overhaul.
Adds three geographic identity fields (`location` / `nationality` / `hometown`) to the
structured-insights schema and its `human_insights` projection; rewrites the three
extraction prompts to stop misattributing the AI companion's attributes to the user; and
moves the **facts** and **memory** extraction prompts into `model_config.toml`
(`filter_prompt`), refusing to boot if either is unset.

**Spec B1** (the `companion_insights_events` audit table + the
`model`/`usage`/`generation_id` columns) shipped separately (PR #74, migration `0025`) and
is **not** revisited here.

---

## 0. Background

### The three items this spec covers

3. **Item 3 — geo fields.** Add `location` / `nationality` / `hometown` to the
   structured-insights `SCHEMA`, to `companion_insights` (JSONB — no DDL), and to
   `human_insights` (new columns). Redefine `city` semantics. Precise semantics:
   - `city` = **常住城市** (where the user usually lives, long-term residence).
   - `location` = **目前所在地** (where the user is right now — travel, business trip).
   - `hometown` = **老家** (籍贯 / where they're originally from).
   - `nationality` = **国籍**.
   - Worked example: a 香港新界 person working in 深圳, traveling to 台北 →
     `city=深圳`, `location=台北`, `hometown=新界`, `nationality=中国香港`.
4. **Item 4 — attribution precision.** `memory_extraction` and `insight_extraction` both
   misattribute the AI companion's job/city/etc. to the user. Rewrite the extraction
   prompts to fix this.
5. **Item 5 — config-driven extraction prompts.** Move the `memory_extraction` and
   `insight_extraction` (facts-stage) prompts into `model_config.toml` the way
   `chat_input_filter` / `chat_output_filter` / `chat_vision` read `filter_prompt`. Refuse
   to boot if either prompt is unset.

### Current state (verified)

- **Schema constant** `COMPANION_INSIGHTS_SCHEMA` lives at
  `crates/eros-engine-server/src/prompt.rs:589-608`. It already has `city`
  (`"string — user's city"`, training weight `0.05`) and `occupation`; no `location` /
  `hometown` / `nationality`.
- **`companion_insights`** (migration `0005`) stores insights as `JSONB`; `InsightRepo::merge`
  (`crates/eros-engine-store/src/insight.rs:84-112`) does a shallow top-level key merge —
  arbitrary new keys ride along, **no DDL needed**.
- **`human_insights`** (migration `0015`) is the flat projection mirror and already has a
  `city TEXT` column. Projection: `project_columns`
  (`crates/eros-engine-store/src/human_insight.rs:73-92`) + the INSERT in
  `project_from_insights` (`human_insight.rs:120-162`). `HumanInsightsRow` is the
  `FromRow` struct.
- **`training_level`** is `compute_training_level` (`insight.rs:34-47`) summing a `WEIGHTS`
  table (`insight.rs:21-31`) that sums to `1.0`; `city` contributes `0.05`.
- **Two chat-context renderers** turn insights into "基础画像" bullets fed to the reply
  prompt, and a parity test asserts they agree byte-for-byte:
  - `insights_to_bullets` (`companion_insights` JSONB → bullets,
    `crates/eros-engine-server/src/pipeline/handlers.rs:~410-456`).
  - `human_insights_to_bullets` (`HumanInsightsRow` → bullets, `handlers.rs:464-500`),
    with `InsightMode::Neutral` dropping the *intimate* fields
    (`love_values` / `interests` / `emotional_needs`) and matching-only columns never
    rendered.
  - Parity test `human_insights_full_matches_companion_insights_renderer`
    (`handlers.rs:~1332`).
- **Three extraction prompts**, all currently sent as a **single user message, no system
  role**, all in `prompt.rs`:
  - `extract_facts_prompt(user_msg, assistant_msg)` (`prompt.rs:614-621`) — facts stage of
    `insight_extraction`; called at `post_process.rs:684`.
  - `extract_structured_insights_prompt(facts, existing)` (`prompt.rs:661-682`) — structured
    stage; injects `COMPANION_INSIGHTS_SCHEMA`; called at `post_process.rs:762`. **Currently
    written in traditional Chinese** (copy-paste artifact).
  - `extract_memories_prompt(turns)` (`prompt.rs:634-654`) — `memory_extraction`; called at
    `dreaming.rs:169`.
- **Config / `filter_prompt`** pattern: `TaskConfig.filter_prompt: Option<String>`
  (`crates/eros-engine-llm/src/model_config.rs:346-402`), read by `resolve_input_filter`
  (`757-786`), `resolve_output_filter` (`681-739`), `resolve_vision` (`788-813`) — each
  returns `None` when the task table is absent or the prompt is blank → feature silently
  skipped. The `insight_extraction` / `memory_extraction` tasks exist in
  `examples/model_config.toml` (lines `182-186` / `193-197`) but carry only model/temp;
  their prompts are hardcoded in `prompt.rs`.
- **Refuse-to-boot precedent**: `main.rs:217-225` bails on an empty `VOYAGE_API_KEY`;
  `main.rs:253-259` bails on a missing JWT source. There is **no** boot validation of any
  `filter_prompt` today.
- **Read API**: `ProfileResponse.companion_insights` (`routes/companion.rs:137-140`) is
  `Option<serde_json::Value>` with `#[schema(value_type = Object)]` — geo fields ride
  inside the untyped object, so **`openapi.json` is unaffected**. `human_insights` has no
  HTTP read surface (it only feeds chat-context bullets).

---

## 1. Goals / non-goals

**Goals**
- Add `location` / `nationality` / `hometown` end to end: schema constant → `companion_insights`
  JSONB (free) → `human_insights` columns + projection → both bullet renderers.
- Rewrite all three extraction prompts so the model never banks the AI companion's
  self-described attributes as user facts; normalize touched prompt text to simplified
  Chinese.
- Make the **facts** and **memory** prompts config-driven via `filter_prompt` on the
  existing `insight_extraction` / `memory_extraction` task keys; refuse to boot if either
  is unset.

**Non-goals (out of scope)**
- The structured-fill prompt stays **in code** (it is coupled to `COMPANION_INSIGHTS_SCHEMA`
  and the projection); it gets only a wording/attribution fix, not config-ification.
- **No `training_level` change.** The three geo fields are **not** added to `WEIGHTS`;
  `compute_training_level` math is unchanged (no rebalance, no silent shift on existing
  users' next extraction). `city` keeps `0.05`.
- **No backfill.** New `human_insights` columns are `NULL` until the next extraction
  repopulates them; there is no source JSONB geo data to backfill from.
- No change to `companion_insights_snapshot` (stores whole JSONB — geo rides along) or to
  `companion_insights_events` / the affinity tables (Spec B1).
- No read-API / DTO / `openapi.json` change.
- `InsightMode::Neutral` field classification: geo fields are **non-intimate** (rendered in
  both `Full` and `Neutral`), like `city` / `occupation`.

---

## 2. Item 3 — geo schema

### 2a. Schema constant (`prompt.rs`)

Replace the `city` line and prepend the geo cluster; add a worked example; normalize the
existing traditional `夜貓子` → `夜猫子`:

```
companion_insights schema (all fields optional, only include if confident):
{
  "city": "string — 常住城市（用户长期居住的城市）",
  "location": "string — 目前所在地（用户此刻所在的城市/地点，如出差、旅游）",
  "hometown": "string — 老家（用户的籍贯 / 出生成长地）",
  "nationality": "string — 国籍",
  "occupation": "string — job/career",
  "mbti_guess": "string — e.g. INFP",
  "love_values": "string — attitude toward love & relationships",
  "interests": ["list", "of", "hobbies"],
  "emotional_needs": "string — what emotional support they need",
  "life_rhythm": "string — e.g. 夜猫子, 早睡早起",
  "matching_preferences": {
    "preferred_gender": "string",
    "age_range": [min_int, max_int],
    "deal_breakers": ["list"]
  },
  "personality_traits": ["list", "of", "traits"]
}
地理字段示例：一个在深圳工作的香港新界人到台北旅游 → city=深圳, location=台北, hometown=新界, nationality=中国香港
Return ONLY a JSON object with the fields you are confident about.
Do not invent or guess anything not clearly supported by the facts.
```

### 2b. Migration `0026_human_insights_geo.sql`

```sql
-- SPDX-License-Identifier: AGPL-3.0-only
-- Adds the geographic identity fields to the flat human_insights mirror.
-- city already exists (0015); location/hometown/nationality are new. Existing
-- table grants + RLS (0015) carry over to the new columns — no lockdown block.
-- No backfill: companion_insights JSONB has no geo data yet; rows repopulate on
-- the next insight_extraction run.
--
-- Spec: docs/superpowers/specs/2026-06-03-extraction-geo-and-config-prompts-design.md

ALTER TABLE engine.human_insights
    ADD COLUMN location    TEXT,
    ADD COLUMN hometown    TEXT,
    ADD COLUMN nationality TEXT;
```

### 2c. Projection (`human_insight.rs`)

- `ProjectedColumns`: add `location: Option<String>`, `hometown: Option<String>`,
  `nationality: Option<String>`.
- `project_columns`: `location: str_field(insights, "location")`, etc.
- `project_from_insights` INSERT: add the three columns, three binds, and three
  `ON CONFLICT … DO UPDATE SET` lines.
- `HumanInsightsRow` (the `FromRow` struct): add the three `Option<String>` fields so the
  row round-trips and the renderer can read them.

### 2d. Bullet renderers + parity (`handlers.rs`)

Both renderers change **identically** so the parity test stays green. Insert the geo cluster
immediately after **城市**, all non-intimate (rendered in every non-`Off` mode):

`insights_to_bullets` (JSONB):
```rust
push_str(&mut out, "city", "城市");
push_str(&mut out, "location", "所在地");
push_str(&mut out, "hometown", "老家");
push_str(&mut out, "nationality", "国籍");
push_str(&mut out, "occupation", "职业");
// … unchanged …
```

`human_insights_to_bullets` (row) — same labels/order:
```rust
push_str(&mut out, &row.city, "城市");
push_str(&mut out, &row.location, "所在地");
push_str(&mut out, &row.hometown, "老家");
push_str(&mut out, &row.nationality, "国籍");
push_str(&mut out, &row.occupation, "职业");
// … unchanged …
```

Extend the parity-test fixture and the `human_insights_to_bullets` row fixtures to populate
the three new fields.

---

## 3. Item 4 — attribution precision (prompt rewrites)

All extraction prompts gain an explicit **anti-attribution clause**: the assistant
(AI 伴侣) is a fictional persona and its self-described attributes are never user facts.
The facts and memory prompts move to a **system + user** message structure (the system
message is the instruction; the conversation/turn is a separate user message). The
structured prompt stays a single in-code message but gains the same clarity. All touched
text is simplified Chinese.

### 3a. Facts prompt → `insight_extraction.filter_prompt` (system message)

```
你是事实提取器，只分析【真人用户】，从这一轮对话中提取关于用户的新事实发现。

铁律：
- 只提取关于真人用户的事实。
- assistant（AI 伴侣）是虚构角色；它自我介绍的职业、城市、年龄、性格、喜好等，绝不是用户的事实。
- 仅当用户主动陈述、复述或明确认同某信息时，才算用户事实。
- 没有新的用户事实时，返回空数组 []。

严格输出 JSON，格式：{"facts": ["事实1", "事实2"]}
```

User message assembled server-side (replaces the data half of `extract_facts_prompt`):
```
用户: {user_msg}
AI: {assistant_msg}
```

### 3b. Memory prompt → `memory_extraction.filter_prompt` (system message)

```
你从一段已结束的对话中，提取 0-10 条值得长期记住的、关于【真人用户】的记忆条目，每条带一个 category 标签。

category（只能用这五种之一）：
- fact: 客观事实，如住在哪、做什么工作、家庭状况
- preference: 偏好/喜好，如喜欢什么、讨厌什么、口味、品味
- event: 发生的事件，如最近发生了什么、经历过什么
- emotion: 情绪/心理状态，如对某事的感受、长期心理倾向
- relation: 与他人的关系，如朋友、家人、同事

铁律：
- 只记关于真人用户的内容。
- assistant（AI 伴侣）是虚构角色；它自我介绍的职业、城市、喜好等绝不是用户的记忆。
- 不记 AI 单方面的回复内容，不记一次性的寒暄、玩笑。
- 同一事实合并成一条，不要重复。
- 没有任何值得记的就返回空数组。

严格输出 JSON，格式：{"memories": [{"content": "...", "category": "fact"}]}
```

User message assembled server-side (the joined conversation, labels `用户：`/`AI：`
unchanged):
```
{convo}
```

### 3c. Structured-fill prompt (stays in code; `extract_structured_insights_prompt`)

```
以下是从对话中提取的【用户】事实：
{facts_str}

现有的用户画像（companion_insights，供参考，不要重复已知信息）：
{existing_str}

请根据上方的【用户事实】，填充以下 schema 中你有信心的字段。schema 描述的是【真人用户】本人——occupation、city、location 等都指用户，绝不是 AI 伴侣：
{COMPANION_INSIGHTS_SCHEMA}

仅输出 JSON，不要任何解释。
```

---

## 4. Item 5 — config-driven facts + memory, refuse-to-boot

### 4a. Resolve functions (`model_config.rs`)

Add `resolve_insight_extract()` and `resolve_memory_extract()`, each mirroring
`resolve_vision` (returns `Option<Resolved…>` bundling `model` / `fallback_model` /
`temperature` / `max_tokens` / a prompt field / `retry_depth` / `reasoning`). They read the
existing task keys `"insight_extraction"` / `"memory_extraction"`; return `None` when the
task table is absent **or** `filter_prompt` is blank/whitespace. (No rename of the task
keys; no new `TaskConfig` field — reuse `filter_prompt`.) Add `Resolved*` structs alongside
`ResolvedVision`.

### 4b. Call-site wiring

- **`extract_facts` (`post_process.rs:673-710`)**: resolve via `resolve_insight_extract()`.
  Build `messages = [ {role:"system", content: prompt}, {role:"user", content: <用户/AI body>} ]`.
  The `<body>` is the former data half of `extract_facts_prompt`; repurpose that builder to
  emit just the body (or inline it). Model/temp/max_tokens/reasoning come from the resolved
  bundle.
- **Memory call (`dreaming.rs:134-183`)**: resolve via `resolve_memory_extract()`. Build
  `messages = [ {role:"system", content: prompt}, {role:"user", content: <convo body>} ]`.
  Repurpose `extract_memories_prompt` to emit just the joined-convo body.
- **Structured call (`post_process.rs:749-777`)**: unchanged shape — one user message built
  in code by `extract_structured_insights_prompt`; same task model via
  `resolve(INSIGHT_TASK, None)`. (The boot gate guarantees the facts prompt exists; the
  structured prompt is in-code so always present.)

### 4c. Refuse-to-boot (`main.rs`)

After the **serve-path** `model_config` load (`main.rs:~262-272`, i.e. not the
`backfill-human-insights` / seed subcommands) and before building `AppState`, mirroring the
`VOYAGE_API_KEY` bail!:

```rust
if model_config.resolve_insight_extract().is_none() {
    anyhow::bail!(
        "insight_extraction filter_prompt is unset — eros-engine refuses to boot \
         (insight extraction prompt is required; see examples/model_config.toml)"
    );
}
if model_config.resolve_memory_extract().is_none() {
    anyhow::bail!(
        "memory_extraction filter_prompt is unset — eros-engine refuses to boot \
         (memory extraction prompt is required; see examples/model_config.toml)"
    );
}
```

### 4d. `examples/model_config.toml`

Add `filter_prompt = """…"""` (the §3a / §3b defaults) to the existing
`[tasks.insight_extraction]` and `[tasks.memory_extraction]` tables. **Breaking change**
(intended): any `model_config.toml` lacking these two prompts now fails to boot — call this
out in release notes.

---

## 5. Error handling

- Projection / renderer changes inherit the existing fail-open posture (`post_process.rs`
  wraps the projection in a `tracing::warn!`-and-continue).
- The boot checks are **fail-loud by design** (the whole point of item 5) — they abort
  `main` before the server binds, exactly like the `VOYAGE_API_KEY` guard.
- When a configured prompt *is* present, the facts/memory calls behave as today (fail-open
  extraction; a transport error degrades silently).

---

## 6. Testing / verification

- **Migration**: `0026` applies cleanly; `human_insights` has `location` / `hometown` /
  `nationality`.
- **Projection (`#[sqlx::test]`)**: a `companion_insights` JSONB carrying the three geo
  fields projects into the matching `human_insights` columns; round-trip read asserts the
  values. Extend `project_columns` unit tests.
- **Renderers**: extend `human_insights_full_matches_companion_insights_renderer` and the
  row fixtures; add a test asserting the geo cluster renders (城市/所在地/老家/国籍) in both
  `Full` and `Neutral`.
- **Config (`model_config.rs`)**: `resolve_insight_extract` / `resolve_memory_extract`
  return `Some` with a set `filter_prompt`, `None` when blank or when the task table is
  absent (mirror the existing `resolve_vision` tests).
- **Pipeline (wiremock)**: with a **sentinel** `filter_prompt` set in the test
  `ModelConfig`, assert the outgoing facts request's **system** message equals the sentinel
  (proves config → wire) and that extraction still parses the served JSON. Same for the
  memory call in `dreaming.rs`.
- **Fixture migration (important)**: every existing test that exercises `extract_facts` /
  the memory sweeper builds a `ModelConfig` — those fixtures must now set the relevant
  `filter_prompt`, otherwise `resolve_*` returns `None` and the call no longer fires.
  Likewise, the **Spec B1** insight-extraction wiremock tests
  (`insight_extraction_writes_two_events_sharing_run_id`,
  `insight_extraction_empty_facts_writes_one_event`,
  `insight_extraction_facts_parse_error_writes_one_event`) route by prompt substrings
  (`列出你对用户的新事实发现`, `填充以下 schema`); update their config to set the facts
  `filter_prompt` and adjust the `body_string_contains` routing to match the new
  system+user message bodies (the structured call's `填充以下 schema` substring stays valid).
- **Gate**: `cargo fmt` / `clippy --workspace -D warnings` / `test --workspace`
  (DB tests via `.test-env`). `openapi.json` unchanged (untyped `Object` insights payload).

---

## 7. Files touched

- `crates/eros-engine-store/migrations/0026_human_insights_geo.sql` — new: ALTER
  `human_insights` (location/hometown/nationality).
- `crates/eros-engine-store/src/human_insight.rs` — `ProjectedColumns`, `project_columns`,
  `project_from_insights` INSERT, `HumanInsightsRow`.
- `crates/eros-engine-server/src/prompt.rs` — schema constant (geo fields + city redefine +
  example + 简体 normalize); structured-fill prompt wording; repurpose
  `extract_facts_prompt` / `extract_memories_prompt` to emit the user-message body only.
- `crates/eros-engine-server/src/pipeline/handlers.rs` — both bullet renderers + the
  `HumanInsightsRow` fixtures + parity test.
- `crates/eros-engine-llm/src/model_config.rs` — `resolve_insight_extract` /
  `resolve_memory_extract` + `Resolved*` structs.
- `crates/eros-engine-server/src/pipeline/post_process.rs` — facts call: system+user from
  config bundle.
- `crates/eros-engine-server/src/pipeline/dreaming.rs` — memory call: system+user from
  config bundle.
- `crates/eros-engine-server/src/main.rs` — two refuse-to-boot bail! checks.
- `examples/model_config.toml` — `filter_prompt` on `insight_extraction` /
  `memory_extraction`.
- Tests alongside the above.

`crates/eros-engine-store/src/insight.rs` (`WEIGHTS`) is **unchanged** (geo fields
unweighted). No DTO / route / `openapi.json` change.

# Insights

[English](insights.md) · [中文](insights.zh.md)

The insights pipeline builds what the engine knows about the real user from conversation, in two layers. A **fact ledger** (`engine.human_facts`) records atomic facts the extraction call mines from each turn, one row per distinct fact with recurrence counters. A **profile** (`engine.human_insights`, one row per user) is derived from the ledger on a schedule, not per turn; the chat prompt, the voice opener, the profile route and the profile export read it. Every LLM call leaves an audit row in `engine.human_insights_events`. A user's data can be erased in one call, and exports carry the erasure to downstream mirrors.

The per-relationship chains `character_insights` and `user_insights` are separate pipelines with their own tables; see [llm-audit.md](llm-audit.md#character-and-user-insights-experimental). Upgrade steps for deployments coming from v2.3.1 or earlier are in [migrating/v2-4-0-insights-2-0.md](migrating/v2-4-0-insights-2-0.md) and [migrating/v2-4-1-insights-cleanup.md](migrating/v2-4-1-insights-cleanup.md).

```
user message + assistant message
   │
   ├─ insight_extraction   system = filter_prompt (deployment-owned)
   │   → {"facts": [{content, category, domain, …}]}      audit row  stage='facts'
   │
   ├─ ledger upsert        engine.human_facts   (exact-match dedup, recurrence counters)
   │   └─ pending_facts += N      engine.human_profile_state
   │
   └─ pending_facts ≥ 20, or untouched for the idle period (sweeper; an hour by default)
        └─ profile_rebuild   user = the whole ledger + the engine-owned schema
            → full replace of engine.human_insights row         audit row  stage='profile'
```

## The fact ledger

`engine.human_facts` holds one row per distinct fact per user. The ledger records and never adjudicates: "user is single" and a later "user has a partner" are two rows, each with its own timestamps. Judging what is currently true belongs to the derived profile and to downstream consumers.

| Column | Meaning |
|--------|---------|
| `id` | Row id. |
| `user_id` | Whose fact. No foreign key. |
| `content` | The fact as a sentence, as the first occurrence wrote it. |
| `category`, `domain`, `evidence_type`, `temporality`, `persistence`, `confidence` | The six labels of the first occurrence; a recurrence does not relabel. Vocabularies [below](#vocabularies). |
| `occurrences` | Number of extraction runs that emitted the fact. |
| `session_count` | Number of sessions it was seen in (see the counting rule below). |
| `first_seen_at`, `last_seen_at` | Timestamps of the first and latest occurrence. |
| `first_session_id`, `first_message_id`, `last_session_id`, `last_message_id` | Where the first and latest occurrence came from. Foreign keys, `ON DELETE SET NULL`. |
| `first_run_id`, `last_run_id` | Correlation ids of the extraction runs (the `run_id` of the audit rows). Not references. |
| `roleplay`, `content_hash` | Generated: `evidence_type = 'roleplay_expression'`, and `md5(engine.fact_norm(content))`. |

**Dedup.** A fact is the same fact when the user, the evidence class and the normalized text match: a unique index on `(user_id, roleplay, content_hash)`. `engine.fact_norm` applies NFKC, lower-cases, and removes whitespace and punctuation (ASCII and the common CJK marks). Dedup is exact-match only, with no embedding or paraphrase merging, so a negated, hedged, conditional or reworded variant is its own row. `roleplay_expression` facts never merge with any other evidence type.

**One occurrence is one extraction run** that emitted the fact. A 1:1 turn that produces several assistant messages makes one run per message; a group turn makes one run. Duplicates inside a single run's array count once, the first of them winning.

**On a dedup hit** the row keeps its text and labels, and `occurrences` goes up by one; `last_seen_at` and the `last_*` pointers move to the new run; `session_count` goes up by one only when the new run's session differs from the stored `last_session_id`. That approximation can overcount a fact that alternates between two sessions, but it detects "ever seen in two or more sessions" exactly, which is all the tier needs.

**Recurrence tier**, derived from the counters and exported as `recurrence`:

| Tier | Condition |
|------|-----------|
| `once` | `occurrences = 1` |
| `repeated_in_session` | `occurrences > 1` and `session_count = 1` |
| `repeated_across_sessions` | `session_count > 1` |

The tier is used alongside the `persistence` label, never instead of it: `persistence` separates "on the subway right now" from a durable trait, which the counters cannot.

## Fact extraction

`[tasks.insight_extraction]` supplies the model, fallback, temperature, `max_tokens` and sampling knobs. Its `filter_prompt` is the system message and belongs to the deployment; the engine has no built-in extraction prompt. The user message is built by `prompt::facts_user_message`:

```
用户: {user message}
AI:   {assistant message}
```

### When it runs

The run is part of post-process, spawned after the reply and never blocking it. `memory_scope` gates prompt injection only; it does not gate extraction.

- **1:1 text turn** (`post_process::run`): one run for each produced assistant message with non-empty text, pairing the user's message with that message's text. When the user's turn carries an action, the user text is prefixed with its action marker.
- **Group turn** (`post_process::run_group`): one run per turn. The assistant side is every non-empty reply as `{speaker name}：{reply}` lines. See [group-chat.md](group-chat.md#what-reaches-11).
- **Voice turns** never run it.
- A run is skipped when the session is already archived or the user text is blank.

### Output contract

The reply is one JSON object whose `facts` is an array of objects: `content` plus the six labels. The shipped `examples/model_config.toml` prompt shows the exact shape and asks for at most 12 facts, `{"facts":[]}` when there is nothing new, facts about the real user only (never the AI's lines or persona), roleplay expressions marked `roleplay_expression`, and no geography and no third-party names.

What the engine does with the reply:

- Parses it as JSON, the whole text first and then the first balanced `{…}` block. No parseable JSON ends the run (audit status `parse_error`).
- Reads `facts` as an array and keeps the items that are objects whose `content` is neither blank nor a placeholder (`string`, `null`, `unknown`, `n/a`, `未知`, `无`, or a `<…>` / `「<…>」` slot). Any other item is ignored, and a run that ignored any logs one warning with the count. A prompt still on a string-array contract therefore writes nothing to the ledger and warns on every run.
- Folds labels into the vocabularies below; a missing or unlisted label never rejects a fact.
- Ends the run when no usable fact remains (audit status `empty`).
- Writes the usable facts to the ledger in one statement, so a failed write changes nothing and a retry cannot count a fact twice.

### Vocabularies

The engine owns the vocabularies and validates every label against them. `content` is trimmed; blank or placeholder `content` is skipped.

| Field | Values | Fallback |
|-------|--------|----------|
| `evidence_type` | `explicit_statement`, `explicit_agreement`, `observed_behavior`, `roleplay_expression`, `legacy_profile`, `unknown` | `unknown` |
| `category` | `fact`, `preference`, `aversion`, `goal`, `value`, `habit`, `skill`, `event`, `emotion`, `relation`, `communication_style`, `roleplay_tendency`, `constraint`, `other` | `other` |
| `domain` | `basic_profile`, `career`, `education`, `technology`, `finance`, `family`, `friendship`, `romance`, `sexuality`, `kink`, `health`, `mental_health`, `lifestyle`, `hobby`, `ideology`, `personality`, `communication`, `other` | `other` |
| `temporality` | `atemporal`, `current`, `past`, `future`, `unknown` | `unknown` |
| `persistence` | `stable`, `ongoing`, `transient`, `one_off`, `unknown` | `unknown` |
| `confidence` | `high`, `medium`, `low`, `unknown` | `unknown` |

`politics` and `religion` arriving in `domain` map to `ideology`; every other unlisted value, `location_culture` included, becomes `other`. `legacy_profile` marks the facts the upgrade seeded from pre-2.0 profile values; the example prompt does not offer it. A deployment whose prompt uses a richer vocabulary than these lists loses that granularity to `other` and `unknown`; this is by design, not an error.

## The derived profile

`engine.human_insights` holds one row per user, keyed on `user_id` (no foreign key). The profile is global to the user: facts come from every persona the user talks to, and no column records which persona heard what. A rebuild writes it; nothing else does.

| Column | Type | Meaning |
|--------|------|---------|
| `occupation` | `TEXT` | Job and work status with industry, level or daily detail. |
| `mbti_guess` | `TEXT` | A self-reported type verbatim; otherwise a hedged guess, worded as one, from patterns recurring in the ledger. |
| `love_values` | `TEXT` | Attitude toward and expectations of romantic relationships; at most three propositions. |
| `emotional_needs` | `TEXT` | What emotional support the user needs; at most three propositions. |
| `life_rhythm` | `TEXT` | Sleep and daily routine as a concrete pattern. |
| `interests` | `TEXT[]` | Hobbies; each item a concrete short phrase with a real detail. |
| `personality_traits` | `TEXT[]` | Traits; each item a concrete phrase grounded in a situation. |
| `deal_breakers` | `TEXT[]` | Things the user cannot accept in real life; complaints about the AI's style or in-scene reactions do not qualify. |
| `education` | `TEXT` | School, major, studying or graduated. |
| `education_level` | `TEXT` | One of `secondary_or_below`, `associate`, `bachelor`, `master`, `doctorate`. |
| `education_status` | `TEXT` | `enrolled` or `graduated`. |
| `family` | `TEXT` | Family structure: marital and parental status, members, relations. |
| `relationship_history` | `TEXT` | Past relationships: how the last one ended, how long single. |
| `social_pattern` | `TEXT` | Solitude or gathering tendency, online and offline habits. |
| `future_plans` | `TEXT` | Near-term goals and life direction. |
| `finance_status` | `TEXT` | Income clues and spending habits; only when stated. |
| `kinks` | `TEXT[]` | Concrete play and roles; items sourced from roleplay say so inside the phrase. |
| `ideology` | `TEXT` | Political, religious and social-values stance; hedged wording when inferred from behavior, never from who the user is. |
| `mental_health` | `TEXT` | User self-report only, no diagnosis. |

That is 19 fields (`education_level` and `education_status` count as two) plus `user_id` and `updated_at`. Unknown means absent: an empty field is `NULL` (or `[]`), never a stored placeholder. The row has no geography, gender-preference or age-range columns (`city`, `location`, `hometown`, `nationality`, `preferred_gender`, `age_min`, `age_max`); a deployment that needs them for matching should collect them from the user directly.

Indexes: GIN on `interests` and `personality_traits` and a btree on `(updated_at, user_id)` (the export keyset). Row-level security is enabled with no policy, and the `anon` and `authenticated` roles are revoked where they exist.

### State and triggers

`engine.human_profile_state` keeps one row per user with facts:

| Column | Meaning |
|--------|---------|
| `pending_facts` | Ledger facts written or bumped since the last settled rebuild. |
| `last_fact_at` | When the counter last moved (or a failure last pushed it back). |
| `last_rebuilt_at` | When a rebuild last settled. |
| `rebuild_failures` | Consecutive failed rebuilds. |
| `claimed_at` | Set while a rebuild is in flight. |

After each extraction run that wrote or bumped N ≥ 1 ledger rows, `pending_facts` grows by N and `last_fact_at` is set to now. A rebuild starts in one of two ways:

- **Count.** When `pending_facts` reaches 20 after a run, that run's post-process task rebuilds inline. It is already off the reply path.
- **Idle flush.** A sweeper wakes every `PROFILE_REBUILD_TICK_SECS`, claims up to `PROFILE_REBUILD_BATCH` users whose `pending_facts > 0` and whose `last_fact_at` is older than `PROFILE_REBUILD_IDLE_SECS`, longest-waiting first, and rebuilds them one after another. With the sweeper off, only the count trigger fires.

A rebuild first claims the state row, so overlapping turns and the sweeper never rebuild one user twice. A claim older than 600 seconds is taken as a dead worker's and can be claimed again; the sweeper refreshes each claim right before that user's rebuild, so a long batch does not age its own tail into re-claimable claims.

### What a rebuild reads

The user's whole ledger and nothing else: **never the previous profile.** Rewriting the old profile is what makes output grow and truncate, and the ledger already holds everything the profile is a summary of. The prompt (engine-owned, `prompt.rs`) is one user message holding the rendered ledger and `PROFILE_REBUILD_SCHEMA`.

The ledger renders as `## <domain>` groups with one line per fact. Rows are ranked by `occurrences` descending, then `last_seen_at` descending, and admitted in that order while the whole text, headers included, stays within **16,000 characters**. The first fact that does not fit ends the walk, so what is read is always the top-ranked prefix of the ledger; the rest stays in the ledger and is left out of that rebuild only. Groups appear in the order their first fact was admitted. A line carries the fact, the evidence type, the persistence (omitted when `unknown`), the recurrence tier with counts when the fact was seen more than once, and the last-seen date:

```
## hobby
- 爱爬山（证据: explicit_statement；持续性: stable；复现: repeated_across_sessions，×3，2 个会话；最近 2026-10-21）
```

The labels are printed as their literal codes because the prompt's rules name them.

### What a rebuild writes

The model returns one JSON object with the fields above (the schema uses `<…>` placeholders only, never example values that could be copied back). Rules the prompt carries:

- Fill only fields the ledger clearly supports; omit the rest.
- The array fields carry at most 12 items each, picked by occurrence and recency, with no duplicates, verbatim or reworded.
- `transient` facts stay out of every field unless they recur across sessions.
- `roleplay_expression` facts go only into `kinks`, marked as roleplay inside the phrase; they never reach a real-life field.
- `mental_health` is the user's self-report; `ideology` may rest on behavior shown in the facts, with hedged wording.
- `education_level` is an object `{"level", "status"}`; `{}` when nothing is writable.
- The reply is one compact JSON object, no indentation or newlines.

Before writing, the engine projects the object onto the columns and drops what carries no information: a scalar must be a JSON string, an array keeps only its string items — the first occurrence of each, at most 12 (`PROFILE_LIST_MAX_ITEMS`), so a reply that ignored the cap or looped on the same batch still writes a bounded row — and a value that is blank, `string`, `null`, `unknown`, `N/A` (any case), `未知`, `无`, or a `<…>` / `「<…>」` slot left unfilled is dropped. `education_level.level` and `.status` are each kept only when they are one of the codes in the table above. Wrong types and unknown keys are ignored.

Two ledger-aware guards run between the projection and the write, for replies that ignored the prompt's rules (the projection is pure and cannot see the ledger): a ledger of only `roleplay_expression` facts keeps `kinks` and nothing else, and `mental_health` is dropped unless the ledger holds a `health` or `mental_health` fact.

The write is a **full replace** of the 19 fields in one statement: a field the rebuild omits becomes empty, and `updated_at` moves, which is what moves the row through `GET /svc/v1/insights/changes`.

### Outcomes

| Outcome | When | Profile row | Counter |
|---------|------|-------------|---------|
| `ok` | The object projected to at least one field and the write succeeded. | Replaced. | `pending_facts = 0`, `last_rebuilt_at = now`, `rebuild_failures = 0`. |
| `empty` | The user has no ledger rows, or only `transient` facts that never recurred across sessions (no call is made in either case — the correct reply is always `{}`), or the reply is a JSON object with nothing writable (including `{}`). | Untouched. | Settled like `ok`: the batch is accepted as a residual, and the user's next facts arm the trigger again. |
| `failed` | The ledger read failed, the call returned no reply, the reply is not a JSON object, or the write failed. | Untouched; the old row keeps serving. | `pending_facts` is kept and `last_fact_at = now`, so the idle flush waits another full idle period (`PROFILE_REBUILD_IDLE_SECS`, one hour by default); `rebuild_failures` goes up by one. The third consecutive failure instead zeroes `pending_facts` and `rebuild_failures`. |

A user with an unrebuildable ledger therefore costs at most three calls per burst of activity, never a call per tick. The claim is released in every outcome.

## Readers

### Chat prompt

The reply handler loads the row and renders it with `human_insights_to_bullets` under the label `基础画像`, as the first group of the `[user_profile]` section, ahead of the memory-category groups. How much is rendered follows the request's `memory_scope` through `InsightMode`:

| `memory_scope` | `InsightMode` |
|----------------|---------------|
| `full` | Full |
| `neutral_and_relationship` (default) | Neutral |
| `relationship_only` | Off |
| `neutral_only` | Neutral |
| `insights_only` | Full |
| `none` | Off |

Off reads nothing. The renderer emits a bullet only for a non-blank value:

| Tier | Fields (label) |
|------|----------------|
| Neutral and Full | `occupation` (`职业`), `education` (`教育`), `education_level` with `education_status` (`学历`, e.g. `学历：本科·在读`), `mbti_guess` (`MBTI`), `ideology` (`意识形态`), `life_rhythm` (`作息`), `social_pattern` (`社交模式`), `personality_traits` (`性格特质`), `future_plans` (`未来计划`) |
| Full only (the intimate fields) | `love_values` (`感情观`), `relationship_history` (`感情经历`), `interests` (`兴趣`), `emotional_needs` (`情感需求`), `family` (`家庭`), `finance_status` (`经济状况`), `kinks` (`性癖`), `mental_health` (`心理健康`) |
| Never rendered | `deal_breakers` |

A bullet is `{label}：{value}`, with array values joined by `、`. The order is fixed: `职业`, `教育`, `学历`, `MBTI`, `意识形态`, then the Full-only fields in the order of the table, then `作息`, `社交模式`, `性格特质`, `未来计划`. A failed read is logged and yields no bullets. See [memory-layers.md](memory-layers.md#retrieval-and-injection).

**Affinity gate.** Under Full, each intimate field also has to clear the relationship's affinity: it reads one line and renders only from a minimum tier on that line (tiers 1–5 of the time-decayed affinity, see [affinity-model.md](affinity-model.md); a pair with no affinity row is tier 1 on both lines). The other line never unlocks it, so building friendship does not surface romance-side fields.

| Field (label) | Line | From tier |
|---------------|------|-----------|
| `love_values` (`感情观`) | chemistry | 2 (flirtation) |
| `relationship_history` (`感情经历`) | chemistry | 3 (crush) |
| `kinks` (`性癖`) | chemistry | 4 (lover) |
| `interests` (`兴趣`) | bond | 2 (friend) |
| `emotional_needs` (`情感需求`) | bond | 3 (close_friend) |
| `family` (`家庭`) | bond | 3 (close_friend) |
| `finance_status` (`经济状况`) | bond | 4 (confidant) |
| `mental_health` (`心理健康`) | bond | 4 (confidant) |

The gate filters rendered bullets by their leading `{label}：`, and only ever removes: the neutral fields are never gated, and nothing outside `memory_scope` is added. The same table routes profile-layer memories by domain ([memory-layers.md](memory-layers.md#affinity-gate)).

### Voice

On a voice session's first turn the bootstrap snapshot renders the same bullets with the same function, under `[关于他]`. The tier comes from that first turn's `memory_scope` (Neutral when omitted), and the snapshot is frozen into `chat_sessions.metadata.voice_bootstrap`, so later changes to the row do not reach that call. Each turn passes the frozen bullets through the affinity gate above with that turn's tiers, so the stored marker never changes but what reaches the prompt follows the pair's affinity. A failed read is not swallowed: the snapshot is not frozen and the next turn retries. See [memory-layers.md](memory-layers.md#voice-turns).

### Profile route

`GET /comp/user/{user_id}/profile` returns the row as `ProfileResponse`: `user_id`, the 19 fields (all of them, the sensitive three included, since it is the user's own data) and `updated_at`. The path `user_id` must equal the JWT's user (403 otherwise). A user with no row gets 200 with `updated_at: null`, `null` scalars and `[]` arrays. See [api-reference.md](api-reference.md#get-compuseruser_idprofile).

### Daily snapshot

`pipeline::snapshot::sweeper` runs on a cron schedule (default 23:00 `Asia/Singapore`) and inserts one row per `human_insights` row into `engine.human_insights_snapshot`, with the whole row as JSONB and a `captured_at` shared by all rows of one fire. There is no LLM call, no dedupe and no filter. `SNAPSHOT_DISABLED`, `SNAPSHOT_CRON` and `SNAPSHOT_TZ` configure it; see [deploying.md](deploying.md#operational-notes).

## Exports and deletions

Three sub-trees of `/svc/v1/*`, each mounted only when its own secret is set, so a consumer holds only the credential for the streams it reads. Admission compares the header with the secret in constant time; missing or wrong is 403, decided before the query string is validated. A sub-tree whose secret is unset answers like any unknown path (never 403, never 200). A secret is trimmed, blank counts as unset, and fewer than 32 bytes refuses to boot.

| Secret | Header | Routes |
|--------|--------|--------|
| `INSIGHTS_EXPORT_SECRET` | `x-insights-export-secret` | `GET /svc/v1/insights/changes`, `GET /svc/v1/memories/changes`, `GET /svc/v1/insights/deletions` |
| `FACTS_EXPORT_SECRET` | `x-facts-export-secret` | `GET /svc/v1/facts/changes`, `GET /svc/v1/facts/deletions` |
| `USER_ERASE_SECRET` | `x-user-erase-secret` | `POST /svc/v1/users/{user_id}/erase` |

All streams are keyset-paged with `since`, a tie-break id, and `limit` (default 500, clamped to 1..=1000); `next` is present only when the page is full and `null` means the walk is complete. Request and response shapes and polling guidance are in [api-reference.md](api-reference.md#service-endpoints).

- **Profile export.** `GET /svc/v1/insights/changes` walks `human_insights` by `(updated_at, user_id)`. A row carries `user_id`, `updated_at` and every profile field **except `kinks`, `ideology` and `mental_health`: those three are never exported.**
- **Facts export.** `GET /svc/v1/facts/changes` walks the ledger by `(last_seen_at, id)`. A row carries `id`, `user_id`, `content`, the six labels, `occurrences`, `session_count`, `recurrence`, `first_seen_at` and `last_seen_at`; the session, message and run pointers stay inside the engine. A repeat moves `last_seen_at`, so a row reappears in the stream each time its counters change. `first_seen_at` and `last_seen_at` are stamped by the ledger write itself (`now()`, and a recurrence never moves `last_seen_at` backwards), so a stamp precedes its commit by one short statement; a resuming poller overlaps by a few seconds.
- **Deletions.** `GET /svc/v1/insights/deletions` and `GET /svc/v1/facts/deletions` are the same stream, one route per secret: user-level tombstones `{user_id, erased_at}` from `engine.user_erasures`, keyset-paged by `(erased_at, user_id)`. There is no per-fact deletion. **A consumer that mirrors profiles, memories or facts must honor this stream**: an erase deletes the rows it mirrors, and none of the changes feeds can report a row that is gone.
- **Memories export.** `GET /svc/v1/memories/changes` is unchanged and carries no deletions of its own; its mirror learns about erased users from `insights/deletions`.

## Erasure

`POST /svc/v1/users/{user_id}/erase`, mounted only under `USER_ERASE_SECRET`, removes everything in `engine.*` the engine derived about the user. It is separate from the export secrets so that erasure works on a deployment that never enables an export. One call runs in one transaction: the deletes, then the blanks, then the tombstone.

**Deleted** (rows removed):

| Table | Scope |
|-------|-------|
| `human_profile_state` (first: it is the lock that serializes the erase against an in-flight rebuild write), `human_facts`, `human_insights`, `human_insights_snapshot`, `companion_insights_snapshot` | `user_id` |
| `companion_memories` | `user_id` (profile and relationship layers) |
| `world_memories`, `world_states`, `world_enrollments`, `world_posts` (comments go with their post), `persona_story_insights`, `persona_story_memories`, `persona_story_events` | `owner_uid` |
| `user_insights`, `user_insights_snapshot`, `character_insights`, `character_insights_snapshot` | the user's persona instances (`persona_instances.owner_uid`) |
| `relationship_scan_state` (first of the four: the relationship scan's writes check it under a row lock, so a write in flight lands before the deletes that remove it, and a later one writes nothing), `relationship_episodes`, `relationship_arcs`, `relationship_journal` | the user's persona instances |

**Blanked** (rows kept with ids and timestamps, content cleared):

| Table | Cleared |
|-------|---------|
| `human_insights_events` | `payload` |
| `companion_decision_events` | `payload` |
| `chat_images_events` | `inputs`, `subject`, `caption`, `composed_prompt`, `last_failure` |
| `chat_vision_events` | `vision`, `image_url`, `last_failure` |
| `user_insights_events`, `character_insights_events` | `payload` (scoped through the user's persona instances) |
| `companion_affinity` | `feeling_clause`, `feeling_clause_at`; the numbers are the relationship, not the person |
| `companion_affinity_events` | `context` |

**Written:** an upsert into `engine.user_erasures (user_id, erased_at)`, which feeds both deletion streams.

The response is `{user_id, erased_at, deleted: {table: n}, blanked: {table: n}}`; every table above is listed, with 0 when it had nothing to do. `blanked` counts only rows that still carried content.

- **Idempotent.** A repeat finds nothing left, reports 0 everywhere, and moves `erased_at` forward, which re-announces the erasure on the deletion streams for a mirror that missed the first.
- **Order.** Call it **before** deleting the user's sessions or persona instances. The insight chains and the relationship scan's tables key on the instance alone, so they are reachable only while the instance row exists.
- **Quiesce first.** A stage-1 extraction already in flight can write facts after the erase commits; stop the user's traffic before calling, and call again to clean up if one slipped through. A rebuild in flight cannot write a profile back: it writes under the state row's lock, which the erase takes first, and then finds the user gone.
- **Not touched.** Chat history (`chat_messages`, sessions, instances) belongs to the caller's own deletion flow. `relationship_scan_events` holds counts and ids only and is left as it is.

A table added later that carries a user attribution joins the erase in the change that adds it.

## Audit

Each LLM call that returned a response writes one row to `engine.human_insights_events`, append-only.

| Column | Meaning |
|--------|---------|
| `run_id` | One per extraction run, and a fresh one per rebuild. It is also the `first_run_id` / `last_run_id` of the ledger rows the run touched. |
| `user_id` | Whose profile the run was for. No foreign key. |
| `session_id`, `message_id` | The session and the produced assistant message (the turn's last one, for a group). `NULL` on a `profile` row: a rebuild is not a turn. Foreign keys, `ON DELETE SET NULL`. |
| `stage` | `facts` or `profile`. |
| `status` | `ok`, `empty` or `parse_error`. |
| `payload` | See below. |
| `generation_id` | Join key into `engine.llm_generations`, `ON DELETE SET NULL`. `NULL` when the ledger write degraded. |

| `stage` | `status` | `payload` |
|---------|----------|-----------|
| `facts` | `ok` | `{"facts": [...]}`: the array exactly as the model emitted it, items the engine ignored included. |
| `facts` | `empty` | The same shape with no usable fact. |
| `facts` | `parse_error` | `NULL`. |
| `profile` | `ok` | The model's JSON object as returned, off-schema keys included. |
| `profile` | `empty` | The model's JSON object, with nothing writable in it. |
| `profile` | `parse_error` | `{"raw": "<first 2000 characters of the reply>"}`, plus `finish_reason` when the provider reported one (`"length"`: truncated at `max_tokens`). |

Rows with `stage = 'structured'` (and `facts` payloads of the form `{"facts": [...], "details": [...]}`) are history written by the previous pipeline; nothing writes them now. A transport error writes no row, and a rebuild for a user whose ledger is empty — or holds only `transient` facts that never recurred across sessions — makes no call and writes none.

Model and token usage are not on the event row. The extraction call is recorded in `llm_generations` as task `insight_extraction`, the rebuild as `profile_rebuild` with a `NULL` `session_id`:

```sql
SELECT e.run_id, e.stage, e.status, g.task, g.model, g.usage
FROM engine.human_insights_events e
LEFT JOIN engine.llm_generations g ON g.generation_id = e.generation_id
WHERE e.user_id = $1
ORDER BY e.created_at DESC, e.stage;
```

See [llm-audit.md](llm-audit.md#llm-generations-ledger). Payloads hold extracted facts and profile values about the user verbatim; an erase blanks them.

## Configuration

| Setting | Effect |
|---------|--------|
| `[tasks.insight_extraction]` present | Extraction runs. `filter_prompt` is required and must be a plain string: blank refuses to boot, and a variant shape (array or table) refuses to boot. |
| `[tasks.insight_extraction]` absent | Extraction is off: no calls, no audit rows, no ledger writes. Existing rows are still served by the readers, and rebuilds already queued still run. |
| `[tasks.profile_rebuild]` present | Supplies the rebuild's `model`, `fallback`, `temperature`, `max_tokens` and sampling knobs. A `filter_prompt` in any shape, blank included, refuses to boot: the prompt is engine-owned. |
| `[tasks.profile_rebuild]` absent | The rebuild runs on `[tasks.insight_extraction]`'s parameters, not on the global defaults. With both absent it falls to the global defaults. |
| `[tasks.insight_structuring]` present | Ignored. |
| Tier blocks under `[tasks.profile_rebuild]` | Refuse to boot. |
| `[[providers.<name>.body]]` rules scoped with `tasks` | Match on the exact task name: `insight_extraction` for extraction, `profile_rebuild` for the rebuild. |

| Variable | Default | Effect |
|----------|---------|--------|
| `PROFILE_REBUILD_DISABLED` | unset | `1` or `true` stops the idle-flush sweeper. The count trigger still rebuilds. |
| `PROFILE_REBUILD_TICK_SECS` | `300` | Sweeper interval. `0` also stops the sweeper; an unparseable value keeps the default. |
| `PROFILE_REBUILD_IDLE_SECS` | `3600` | How long a user's last new fact must have sat before the sweeper rebuilds. |
| `PROFILE_REBUILD_BATCH` | `20` | Users rebuilt per tick, at least 1. |
| `INSIGHTS_EXPORT_SECRET`, `FACTS_EXPORT_SECRET`, `USER_ERASE_SECRET` | unset | Mount their sub-trees, [above](#exports-and-deletions). |
| `SNAPSHOT_DISABLED`, `SNAPSHOT_CRON`, `SNAPSHOT_TZ` | | The daily snapshot. |

The count (20) and the 600-second claim staleness are constants, not settings. See [model-config.md](model-config.md#task-names) and [deploying.md](deploying.md#operational-notes).

## Operator notes

- **The upgrade backfills the ledger and queues one rebuild per profile row.** The backfill migration reports its counts as a Postgres `NOTICE` of the form `events=<n> seeds=<n> pending=<n>`. `events` is the number of ledger rows built from audit events; `seeds` is the number of rows seeded from current profile values (evidence type `legacy_profile`); `pending` is the number of users queued for a rebuild. Only users that currently have a profile row are touched, so a deployment with none backfills nothing.
- **The rebuild drain costs one LLM call per queued user**, so expect `pending` calls after the deploy, drained by the idle sweeper at `PROFILE_REBUILD_BATCH` users per `PROFILE_REBUILD_TICK_SECS`. Until a user's first rebuild their previous profile row keeps serving injection. With the sweeper off, each queued user is rebuilt at their next extraction run, since the queue is seeded at the count threshold. Size `[tasks.profile_rebuild]` before the deploy: with extraction turned off and no `[tasks.profile_rebuild]` block, the drain runs on the global default model. `PROFILE_REBUILD_DISABLED=1` keeps the drain from running, and with extraction off nothing else triggers a rebuild.
- **Domain reclassification is the deployment's.** Backfilled rows keep the old `sexuality` and `health` domains; the split into `kink` and `mental_health` applies to new facts. The engine ships no reclassifier: an operator who wants old rows split runs an offline pass that updates `engine.human_facts.domain`, which needs nothing beyond `UPDATE` access. A relabel does not queue a rebuild; the corrected domains are read at the user's next rebuild. To queue one by hand, set `pending_facts` above 0 and `last_fact_at` older than the idle period on the user's `human_profile_state` row.
- **A deployment that mirrors profiles, memories or facts must consume the deletion stream**, and call erase before deleting a user's sessions.

## Known limits

- The ledger keeps paraphrases as separate rows. Counts are per extraction run, and a turn that produces several assistant messages runs extraction once per message over the same user message, so one statement can count more than once and read as `repeated_in_session`.
- `session_count` can overcount a fact that alternates between two sessions.
- A fact keeps the first occurrence's labels even when a later occurrence would label it differently.
- The profile is as fresh as its last rebuild: after the last new fact, up to the idle period.
- A ledger larger than the 16,000-character budget feeds each rebuild its top-ranked prefix.
- A rebuild that returns nothing writable accepts its batch as a residual rather than retrying.
- There is no per-fact deletion; erasure is per user.
- No code path prunes `engine.human_insights_snapshot` or `engine.human_insights_events`.

## Source

- `crates/eros-engine-store/src/human_fact.rs` — vocabularies, `FactInput`, `HumanFactRepo::upsert_run`, the recurrence tier
- `crates/eros-engine-store/src/profile_state.rs` — `REBUILD_K`, `MAX_REBUILD_FAILURES`, `bump`, `claim`, `claim_due`, `finish_ok`, `finish_failed`
- `crates/eros-engine-store/src/human_insight.rs` — the 19 fields, `project_profile`, `is_placeholder`, `replace_profile`, `list_changes`
- `crates/eros-engine-store/src/erasure.rs` — `erase_user`, the deleted and blanked table lists, the deletion stream
- `crates/eros-engine-store/src/insight.rs` — `InsightEventRepo`
- `crates/eros-engine-server/src/pipeline/post_process.rs` — `extract_insights`, `extract_facts`, `parse_fact_objects`, `record_facts`
- `crates/eros-engine-server/src/pipeline/profile_rebuild.rs` — `render_ledger`, `rebuild_user`, the sweeper, `LEDGER_BUDGET_CHARS`
- `crates/eros-engine-server/src/prompt.rs` — `facts_user_message`, `PROFILE_REBUILD_SCHEMA`, `profile_rebuild_prompt`
- `crates/eros-engine-server/src/pipeline/handlers.rs` — `human_insights_to_bullets`
- `crates/eros-engine-server/src/pipeline/voice.rs` — bootstrap snapshot
- `crates/eros-engine-server/src/pipeline/snapshot.rs` — snapshot sweeper
- `crates/eros-engine-server/src/routes/svc.rs` — the three `/svc/v1/*` sub-trees; `routes/mod.rs` mounts each under its secret
- `crates/eros-engine-server/src/routes/companion.rs` — `GET /comp/user/{user_id}/profile`
- `crates/eros-engine-llm/src/model_config.rs` — `resolve_insight_extract`, `resolve_profile_rebuild`, `validate_structuring_prompt_unset`
- `crates/eros-engine-store/migrations/0079_human_facts.sql`, `0080_human_facts_backfill.sql`, `0081_insights_2_cleanup.sql`
- `examples/model_config.toml` — `[tasks.insight_extraction]` and `[tasks.profile_rebuild]`
- Design: [2026-10-26-insights-2-fact-ledger-design.md](superpowers/specs/2026-10-26-insights-2-fact-ledger-design.md)

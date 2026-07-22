# eros-engine — World Stories (per-instance persona life simulation; World System v2)

**Status**: design, pending implementation plan
**Target release**: `0.8.x` dev track. **Migration: 0038** (one ALTER + three new tables).
**Scope**: give each persona a private, continuously evolving life of its own —
work, romance (including the user), daily living — simulated per
`persona_instance` every 8 hours by a `world_stories_director` LLM round.
Stories become the substrate the World Memories director builds relationship
scripts on (World System v2 behavior), and are injected into chat prompts by
default. Personas stop being actors who only exist on stage; the off-stage
life is where 真实感 comes from.

Data flow is strictly one-way:

```
World Stories (per-instance life)  →  World Memories (persona↔persona graph)  →  World Town (stage)
```

Companion specs: `2026-07-21-world-memories-design.md` (v1 base),
`2026-07-21-world-town-design.md`. Stories layer ON TOP of the v1 base the
same way Town does; a deployment without `[tasks.world_stories_director]` (or
with owners not flagged in) keeps exact v1 behavior.

---

## 0. Motivation — outsourcing narrative reasoning

The reasoning a persona reply needs splits into two kinds: **conversational
reasoning** (understand this user message, catch the emotion, pick a tone) and
**biographical reasoning** ("what has been happening in my life, what am I to
this user, what did I say last week"). The first must stay per-turn. The
second is precisely where personas break character — asking the chat model to
improvise a life every turn means 100 turns of improvisation that inevitably
contradict each other.

World Stories moves biographical reasoning off the hot path: once per 8 hours,
with no latency pressure, on a model that can be cheaper than the chat model,
behind an activity gate so cost scales with real usage. At chat time only
"retrieve + weave" remains — and weaving pre-computed canon is both cheaper
and far more stable than inventing it.

The amortization is heavily favorable: a heavy user chatting 100 turns a day
pays 3 story rounds for the biography layer; the injection side is a resident
digest plus cosine recall that reuses the turn's already-computed query
embedding — zero extra LLM or Voyage cost on the hot path. O(per-turn)
reasoning collapses to O(per-8h), and the life canon lives in the database:
switch chat models, cross sessions, and nothing is lost. Consistency becomes
a property of the data, not of per-call model discipline.

Two boundary conditions shape the design below:

1. **Similarity recall misses.** When top-k fails to surface the relevant
   chapter, the model can still improvise. This is why the two-layer structure
   is a precondition for the outsourcing to work: the resident digest/insight
   layer guarantees the load-bearing facts (current job, relationship state
   with the user) are present **every turn**; vector recall only supplies
   episodic color. 承重靠常驻，色彩靠召回。
2. **Pre-computation can itself create discontinuity.** After a job change,
   an old "working at the café" fragment can still be recalled. This is the
   one contradiction source pre-computation introduces; it is held down by
   digest priority (resident state wins), retention pruning, and
   past-experience framing in the injection block.

So the precise claim is NOT that World Stories removes per-turn reasoning —
it pre-computes the most expensive and most error-prone layer of it and
freezes it into retrievable canon. What is saved is repeated inference; what
is gained is consistency.

---

## 1. Decisions (settled during brainstorm)

- **The v2 core-rule amendment.** At the WS layer the user is **on-stage**:
  romance progression must include the user. But the director may only *read*
  user actions out of chat records — never invent them. Relationship
  qualification is judged from chat evidence (e.g. lovers only after an
  explicit confession that the persona accepted), with affinity numbers as
  advisory reference only. The WM-layer rule is unchanged (user off-stage;
  user-related facts flow in indirectly through story events).
- **Per-instance rounds + activity gate** (chosen over one batched per-owner
  call). Unit of data AND unit of call is the `persona_instance`. Only
  instances with chat activity inside the gate window get rounds: a quiet
  world falls back to exactly v1's one director call per day, preserving the
  v1 cost invariant. The trade-off (cold personas' lives pause) is accepted.
- **Three-layer switch, mirroring Town.** `[tasks.world_stories_director]`
  section + `world_enrollments.stories_enabled` (downstream-written, default
  false, same enrollment roster) + `WORLD_STORIES_DISABLED`. Injection has
  its own valve `WORLD_STORIES_PROMPT_DISABLED` — **default is to inject**;
  opting out requires the explicit env var.
- **Three tables mirroring the companion stack.**
  `persona_story_insights` (resident structured base ↔ `companion_insights`) +
  `persona_story_events` (append-only progression log ↔ `insight_events`) +
  `persona_story_memories` (vector recall ↔ `companion_memories`).
- **1:1 mirror: the event IS the memory.** Each event's content is embedded
  verbatim into `persona_story_memories` in the same round and transaction.
  **No dreaming-lite-style recall-optimization pass**, deliberately: all
  three of dreaming-lite's motivations are absent here. (1) Source is born
  clean — story rows are authored by the director LLM under structured-output
  rules, no raw user input lands in them. (2) No per-turn myopia to
  consolidate — a WS round already writes with an 8-hour batch view.
  (3) No post-hoc classification needed — events carry categories at write
  time. The residual risk (cross-round semantic repetition) is handled by a
  prompt rule (recent events are in the payload; avoid repeating them) plus
  retention pruning. If recall quality degrades in practice, a dedup pass is
  a separate later mechanism — same posture as v1's town retention note.
- **Fixed insight schema, engine-owned, FLAT typed columns.** The field list
  is a **superset of `COMPANION_INSIGHTS_SCHEMA`** — every existing companion
  field (describing the persona instead of the user) **plus** `work_history`
  (工作经历), `romance_history` (感情史), `family_of_origin`
  (与原生家庭的关系), `user_relationship` (与用户的关系状态). It is stored as
  **flat typed columns like `human_insights`, NOT opaque JSONB** — this is
  the `human_insights` lesson applied in advance: `companion_insights`
  started as opaque JSONB and later had to grow a flat typed mirror (0015),
  write-through plumbing, and a backfill migration (0018); stories go flat
  from day one and skip that whole arc. The list ships as an engine constant
  embedded in the director payload and as a typed row struct. The operator
  `filter_prompt` controls each field's content richness — never the field
  list. (The four new fields may later graduate into the companion side;
  WS-exclusive for now.)
- **7-day context window.** Chat records and relationship evidence fed to a
  story round cover the last 7 days (human perceived-time scale is at most a
  week), capped by turn count.
- **Relative-time + life-stage anchoring.** Experience-type insight entries
  record time as relative expressions (n年前 / n个月前 / n天前) and life
  stages (x岁时 / 上大学时). The engine passes the current UTC datetime in
  every payload; the director refreshes relative expressions on each full
  rewrite. This convention is part of the engine's fixed rules AND must be
  called out to downstream deployers in docs / the example `filter_prompt`.
- **Strictly one-way data flow.** A story round never reads the WM seed or
  `world_memories` — no feedback loop. WM reads stories; stories read only
  their own state + genome + chat/affinity evidence.
- **Rides the v1 base.** Stories require `[tasks.world_director]` configured
  and enrollment, exactly as Town does; the story claim path runs inside the
  existing world sweeper loop (`WORLD_TICK_SECS`).

---

## 2. Data model (migration 0038)

### 2.1 `engine.world_enrollments` — add the stories flag

```sql
ALTER TABLE engine.world_enrollments
    ADD COLUMN stories_enabled BOOLEAN NOT NULL DEFAULT false;
```

Downstream-written, engine-read, per-owner gradual rollout — identical
contract to `town_enabled`.

### 2.2 `engine.persona_story_insights` — resident base + scheduling state

```sql
CREATE TABLE engine.persona_story_insights (
    instance_id          UUID PRIMARY KEY REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    owner_uid            UUID NOT NULL,

    -- flat life-profile columns (persona-side superset of human_insights;
    -- all optional — the director fills what the life so far supports)
    city                 TEXT,
    location             TEXT,
    hometown             TEXT,
    nationality          TEXT,
    occupation           TEXT,
    mbti_guess           TEXT,
    love_values          TEXT,
    emotional_needs      TEXT,
    life_rhythm          TEXT,
    education            TEXT,
    family               TEXT,
    relationship_history TEXT,
    social_pattern       TEXT,
    future_plans         TEXT,
    finance_status       TEXT,
    interests            TEXT[] NOT NULL DEFAULT '{}',
    personality_traits   TEXT[] NOT NULL DEFAULT '{}',
    preferred_gender     TEXT,
    age_min              INT,
    age_max              INT,
    deal_breakers        TEXT[] NOT NULL DEFAULT '{}',

    -- story-exclusive columns (candidates to graduate to the companion side later)
    work_history         TEXT,
    romance_history      TEXT,
    family_of_origin     TEXT,
    user_relationship    TEXT,

    -- resident digest + sweeper scheduling state (world_states shape)
    digest               TEXT NOT NULL DEFAULT '',
    insight_version      INT  NOT NULL DEFAULT 1,
    last_run_at          TIMESTAMPTZ,
    claimed_at           TIMESTAMPTZ,
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_persona_story_insights_owner
    ON engine.persona_story_insights (owner_uid);
```

One row per story-eligible instance; doubles as the sweeper's scheduling row
(`last_run_at` / `claimed_at` / ownership-token semantics copied verbatim
from `world_states`). `last_run_at IS NULL` marks a never-run story — the
payload takes the init branch. Flat typed columns from day one (no opaque
JSONB stage — see the `human_insights` lesson in §1); the director's
`insight` output object is deserialized into a typed row struct and written
as a full-column UPDATE each round.

### 2.3 `engine.persona_story_events` — append-only progression log

```sql
CREATE TABLE engine.persona_story_events (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_uid   UUID NOT NULL,
    instance_id UUID NOT NULL REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    category    TEXT NOT NULL,
    content     TEXT NOT NULL,
    story_date  DATE NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_persona_story_events_instance_time
    ON engine.persona_story_events (instance_id, created_at DESC);
```

`category` is director vocabulary (work / romance / life / whatever the
operator prompt defines) — the engine stores it verbatim and never validates
the vocabulary, mirroring `companion_memories.metadata`. `story_date` is the
retention key (same shape as `world_memories.script_date`).

### 2.4 `engine.persona_story_memories` — recall layer (1:1 with events)

```sql
CREATE TABLE engine.persona_story_memories (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_uid   UUID NOT NULL,
    instance_id UUID NOT NULL REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    event_id    UUID NOT NULL REFERENCES engine.persona_story_events(id) ON DELETE CASCADE,
    content     TEXT NOT NULL,
    embedding   VECTOR(512) NOT NULL,
    story_date  DATE NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_persona_story_memories_instance
    ON engine.persona_story_memories (owner_uid, instance_id);
CREATE INDEX idx_persona_story_memories_embedding
    ON engine.persona_story_memories USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);
```

`event_id` closes the 1:1 audit loop. Written in the same transaction as the
event, from the event's content, embedded in one batched Voyage call per
round.

### 2.5 Lockdown

All three new tables get the 0013 treatment (REVOKE from `anon` /
`authenticated` when present + policy-less RLS). The `world_enrollments`
ALTER inherits the table's existing lockdown.

Flipping `stories_enabled` off (or unenrolling) stops story rounds and
injection immediately but keeps accumulated data — re-enabling resumes the
same life, mirroring world unenrollment semantics.

---

## 3. Story director sweeper

### 3.1 Spawn & tick

No new tokio task: the existing world sweeper loop (`pipeline/world.rs`,
tick = `WORLD_TICK_SECS`) gains a second phase. Per tick, after the WM
director scan, it runs the story scan **iff**
`resolve_world_stories_director()` is `Some` and `WORLD_STORIES_DISABLED` is
unset. If `[tasks.world_director]` is absent the sweeper never starts —
stories require the v1 base by construction.

### 3.2 Eligibility, backfill, claim

Per tick, in order:

1. **Backfill**: insert missing `persona_story_insights` rows for every
   (owner enrolled AND `stories_enabled`) × (instance in the owner's active
   roster — first `WORLD_ROSTER_CAP` (8) active instances by `created_at`,
   the same cap and order as the WM roster). `ON CONFLICT DO NOTHING`.
2. **Claim**: one atomic
   `UPDATE ... WHERE instance_id IN (SELECT ... FOR UPDATE SKIP LOCKED)`
   over `persona_story_insights` joined to `world_enrollments`
   (`stories_enabled = true`) and `persona_instances`
   (`status = 'active'`), where the instance is due
   (`last_run_at IS NULL OR last_run_at < now() - interval_hours`), not
   freshly claimed (stale window `STORY_CLAIM_STALE` = 1800 s), **and passes
   the activity gate**:

   ```sql
   EXISTS (SELECT 1 FROM engine.chat_sessions cs
           WHERE cs.user_id = psi.owner_uid
             AND cs.instance_id = psi.instance_id
             AND cs.last_active_at > now() - $active_window)
   ```

   Inactive personas' lives pause at zero cost and resume on the next tick
   after chat resumes. Batch `STORY_PICK_BATCH` = 8. Claim returns
   `(instance_id, owner_uid, claimed_at)`; `claimed_at` is the ownership
   token threaded through release / mark_ran / persist exactly as in
   `world_states`.

Rows whose instance leaves eligibility (archived, beyond roster cap, flag
flipped off, unenrolled) are simply never claimed again; data is kept.

### 3.3 Per-instance round call

One structured LLM call per claimed instance (`world_stories_director` task):

| Input | Source |
|-------|--------|
| Current UTC datetime | engine — required for the relative-time convention |
| Persona | `persona_genomes` via instance: name, `tip_personality`, `art_metadata` (backstory = canon) |
| Current insight + digest | `persona_story_insights` row (`last_run_at IS NULL` ⇒ init branch) |
| Recent events | last `STORY_RECENT_EVENTS` (12) rows, chronological — continuity + repetition guard |
| Affinity snapshot | latest-active session's `companion_affinity` row for (owner, instance): six axes + bond + chemistry + `relationship_label` — advisory only |
| Chat evidence | messages of this (owner, instance)'s sessions within `context_days` (7), capped at the most recent `STORY_CHAT_TURNS_CAP` (60) turns, chronological |

Fixed engine-owned rules appended to every payload (the operator
`filter_prompt` carries tone / genre / category vocabulary / per-field
richness; these are the floor — same split as `WORLD_DIRECTOR_RULES`):

```
规则：
1) 用户在场：感情线应当包含用户，但用户的言行只能取自聊天记录，绝不编造用户做过的事或说过的话。
2) 关系定性以聊天记录为准（例：用户明确告白且角色答应，才能视为情侣）；亲密度数值仅供参考。
3) insight 是人生底座：只输出固定 schema 中的栏位，不要新增/改名。首轮先把 backstory 烤入再丰富；
   backstory 是 canon，不可与之冲突。每轮输出更新后的完整 insight（全量替换）。
4) 经历类内容用相对时间（n年前/n个月前/n天前）和人生阶段（x岁时、上大学时）记录；
   每轮根据 current_time 刷新相对时间表述。
5) events：当期发生的具体生活事件（工作进展、感情进展、生活进展等，类目见系统指示），
   每条一句、自成一体、适合单独召回；避免与近期事件重复。
6) digest：1-2 句该角色当前人生近况。
```

Structured output (`response_format`, strict=false, mirroring v1):

```json
{ "insight": { ... },            // full replacement, fixed field list
  "digest": "…",                 // 1-2 sentences
  "events": [ {"category": "…", "content": "…"} ] }
```

`events` capped at `STORY_EVENTS_CAP` = 6 per round (defensive truncation
with warn, mirroring `WORLD_FRAGMENTS_PER_PERSONA_CAP`). Zero events is
valid — a quiet stretch of life still updates insight/digest. The `insight`
object is deserialized into the typed row struct (known fields only; unknown
keys dropped with a warn — the fixed column list is the contract).

### 3.4 Persist (single transaction)

Retention prune on **both** `persona_story_events` and
`persona_story_memories` (`story_date < today - retention_days`) → insert
events → insert memories (contents batch-embedded via one Voyage
`embed_documents` call before the tx) → full-column UPDATE of the profile
columns / digest / `insight_version`+1 / `last_run_at` / `claimed_at = NULL`
guarded by the ownership token (`AND claimed_at = $token`; zero rows ⇒ error
before commit ⇒ full rollback). Any failure (LLM, parse, embed, DB) releases
the claim and the instance retries at its next due scan. All semantics copied
from `WorldRepo::persist_round`.

### 3.5 `PERSONA_STORY_INSIGHTS_SCHEMA` (engine constant)

A sibling of `COMPANION_INSIGHTS_SCHEMA` describing the **persona**, embedded
in the payload rules. Field list = every `COMPANION_INSIGHTS_SCHEMA` field
reworded for the persona (city / location / hometown / nationality /
occupation / mbti_guess / love_values / interests / emotional_needs /
life_rhythm / preferred_gender / age_min / age_max / deal_breakers /
personality_traits / education / family / relationship_history /
social_pattern / future_plans / finance_status — matching preferences arrive
pre-flattened; the LLM-facing schema is flat, no nested objects) plus four
story-exclusive fields:

| Field | Holds |
|-------|-------|
| `work_history` | 工作经历 — jobs over time with relative-time/stage anchors |
| `romance_history` | 感情史 — past loves incl. how they ended, anchored in time |
| `family_of_origin` | 与原生家庭的关系 — ongoing relationship, not just structure |
| `user_relationship` | 与用户的关系状态 — current state, chat-evidence-grounded (rule 2) |

All fields optional; the fixed list is the contract. The operator
`filter_prompt` may steer how rich each field's content should be — it may
NOT add, remove, or rename fields. The list exists in three representations
kept in lockstep — the prompt constant, the typed row struct (serde), and the
DDL columns — with a unit test asserting the constant covers every column.

### 3.6 Model config & boot validation

```toml
[tasks.world_stories_director]
model = "..."
filter_prompt = "..."       # REQUIRED — tone/genre, event category vocabulary, per-field richness
interval_hours = 8          # per-instance round cadence
retention_days = 30         # events + memories retention
active_window_hours = 72    # activity gate: chat within this window ⇒ life advances
context_days = 7            # chat/affinity evidence window fed to each round
```

`resolve_world_stories_director()` returns `None` when the section is absent
or `filter_prompt` is blank. Boot validation mirrors Town:
`validate_world_prompts` gains the stories check, skipped when
`WORLD_DISABLED` or `WORLD_STORIES_DISABLED` is set — a staged config can
never block boot while its feature is off.

### 3.7 Audit

New sentinel OpenRouter user `11111111-1111-1111-1111-111111111113`
(dreaming = …111, world = …112 — per-subsystem spend attribution). Token
usage logged as tracing fields via the shared `log_openrouter_usage` path,
task name `world_stories_director`.

---

## 4. World Memories director — v2 behavior

Per owner, computed inside the existing WM round (no extra queries for v1
owners): `stories_active = stories_enabled(owner) AND
resolve_world_stories_director().is_some() AND !WORLD_STORIES_DISABLED`.

When `stories_active`:

- Each roster persona's payload entry gains `recent_life`: that instance's
  `persona_story_events` since the owner's last WM run (fallback 24 h), cap
  `WM_STORY_EVENTS_PER_PERSONA` = 10, chronological, rendered as
  `{category, content}` pairs.
- The fixed rules gain a stories clause (appended like
  `WORLD_TOWN_POST_RULES`):

  ```
  各角色的个人生活以其 recent_life 为准，剧本必须与之一致，不可矛盾；
  角色与用户的关系状态以 recent_life 为准，其他角色可以自然提及，
  但仍绝不编造用户的言行。
  ```

When not `stories_active`, the WM payload and rules are exactly v1's — a
non-stories deployment behaves byte-identically to today. WM cadence is
unchanged (24 h). World Town needs **zero changes**: posts inherit the
story-consistent scripts through the WM seed automatically.

---

## 5. Chat injection

### 5.1 Env switches

`WorldConfig` gains `stories_disabled` (`WORLD_STORIES_DISABLED`) and
`stories_prompt_disabled` (`WORLD_STORIES_PROMPT_DISABLED`), parsed by
`parse_world_config` with the same "1"/"true" convention + unit tests.
Injection is ON by default for story-enabled owners; the env var is the
explicit opt-out. `WORLD_STORIES_DISABLED` stops rounds AND injection;
`WORLD_STORIES_PROMPT_DISABLED` keeps simulating but stops injecting (the
same isolation-valve pattern as `WORLD_PROMPT_DISABLED`: let lives
accumulate, inspect, then open the tap).

### 5.2 Boot flag

`AppState.stories_configured` computed once at boot
(`resolve_world_stories_director().is_some()`), so unconfigured deployments
never pay the story queries on the reply path — mirror of `world_configured`.

### 5.3 Fetch path

`fetch_stories_context(state, user_id, instance_id, query_embedding)` beside
`fetch_world_context`, same degradation ladder: gated on
`!world.disabled && !stories_disabled && !stories_prompt_disabled &&
stories_configured`; digest query joins `world_enrollments` on
`stories_enabled = true` (enrollment check rides the same query); fragment
recall reuses the turn's query embedding with `STORY_RECALL_K` = 3
(mirroring `WORLD_RECALL_K`), degrading to digest-only when the embedding was
skipped; any DB error degrades to `None` with a warn. Story data can never
block or fail a reply. Runs alongside the WM fetch; the two blocks are
independent.

### 5.4 Prompt block

New `[world_stories]` block immediately after `[world_memories]` (empty ⇒
omitted, prompt byte-identical):

```
[world_stories]
（你自己的生活：第一行是当前近况，其余是你经历过的事，时间可能较早；可自然提及）
<digest>
- <recalled episode 1>
- <recalled episode 2>
```

Resident digest carries the load-bearing current state; recalled episodes are
color (§0 boundary condition 1). Recalled episodes may predate the current
state — the block framing marks them as past experience (§0 boundary
condition 2).

---

## 6. Cost model

Per enrolled owner per day, steady state:

- v1 unchanged: 1 WM director call (+ town calls where enabled).
- Stories add: ≤ `24 / interval_hours` × (active instances) story calls +
  the same count of Voyage batch calls. "Active" = chatted within
  `active_window_hours`.
- **A world nobody touches still costs exactly one director call per day** —
  the v1 invariant survives v2, because the activity gate zeroes the story
  layer for idle worlds.

---

## 7. Testing

Unit (pure fns): payload init vs continuation branches; fixed rules always
present (user-grounding, relative-time, schema); response_format shape;
events cap truncation; schema constant covers every insight column (the
three-representation lockstep test); insight deserialization (unknown keys
dropped with warn, arrays and age bounds typed); `parse_world_config` new
flags; WM payload with/without `stories_active` (v1 owners byte-compatible —
no `recent_life`, no stories rule).

Integration (`sqlx::test`, wiremock, mirroring v1's suites):

- Backfill only for `stories_enabled` owners × active roster (cap 8).
- Claim: due + activity gate (inactive instance never claimed; activity
  resumes ⇒ claimed); stale reclaim; token-guarded release / persist
  (lost-claim round writes nothing).
- Full round: mocked director output ⇒ events + memories (1:1, `event_id`
  linked) + insight/digest/version persisted; retention prunes both tables;
  parse failure writes nothing.
- Injection: `fetch_stories_context` gating matrix (unconfigured / disabled /
  prompt_disabled / not stories_enabled / no digest ⇒ `None`); recall scoped
  to (owner, instance); prompt block renders and omits byte-identically.
- WM v2: `recent_life` appears only for stories-active owners; events since
  last run windowing.
- Boot validation: blank `filter_prompt` refuses boot unless the feature is
  switched off.

---

## 8. Out of scope

- Downstream read API for stories (insight/events/timeline endpoints) — data
  reaches only prompts and the WM director for now; a feed/profile surface is
  its own spec.
- Adding `work_history` / `romance_history` / `family_of_origin` to
  `companion_insights` — deliberately WS-exclusive for now.
- Recall-optimization / dedup pass over `persona_story_memories` — analysis
  in §1 says the dreaming-lite motivations don't apply; revisit only on
  observed recall degradation.
- World Town changes — inherits story consistency through WM.
- Per-owner cadence overrides, notifications, catch-up rounds for long-idle
  personas (life simply pauses and resumes).

# eros-engine — Prompt prefix reorder for cache stability + gender/time fixes (Spec)

**Status**: design, pending implementation plan
**Target release**: 0.3.x (prompt-string change + one `art_metadata` field + one new dep; graceful fallback when fields absent)
**Audience**: anyone implementing the `build_prompt` reorder in `crates/eros-engine-server/src/prompt.rs`

---

## 0. Background

`build_prompt` assembles the per-turn companion system prompt as a single
`messages[0]` system message; history is appended after it
(`pipeline/handlers.rs:108-121`). Implicit prefix caching (grok / OpenRouter)
caches the **longest common token prefix of the whole request**, so a stable
leading chunk of the system message caches across turns *and* across users of
the same genome.

Motivation: `eros-reports/dev-logs/2026-05-24_grok_cost_prompt_cache_research.md`.
grok is the cost driver; we want a long, byte-stable per-role prefix so its
automatic prefix cache hits.

**Current break point.** Today the order is
`你是 → 背景 → 说话风格 → 口癖 → 擅长话题 → 附加指引 → 今日情境(每分钟变) → …`.
The `今日情境` timestamp sits right after the persona block, so the cacheable
prefix ends there and everything after — including the *stable* 铁律 / 输出
blocks — is orphaned behind a volatile element.

**Two latent code bugs found while scoping this** (both real, both in
`prompt.rs`, not the persona data):

1. **`genome.system_prompt` is never used by the chat path.** It is loaded from
   DB (`store/persona.rs:154`), held on the struct (`core/persona.rs:10`), and
   exposed to the admin edit route (`routes/companion.rs:500`) — but
   `build_prompt` reconstructs the persona purely from `art_metadata` and
   ignores the authored prose. The authored block is the richest, most stable
   per-genome content; using it both lengthens the cache prefix and reinforces
   the persona (redundancy = weighting for the LLM).
2. **`gender` is never rendered.** `art_metadata` documents a `gender` field
   (`core/persona.rs:13`; e.g. `examples/personas/kenji.toml:21`
   `gender = "male"`), but `build_prompt` only ever emits name/age/mbti/backstory.
   The model has no explicit gender signal → the "male persona uses female
   anatomy" gender-confusion bug.

**Time bug.** `now_context` hands the model raw UTC and asks it to *infer* the
local timezone from the backstory residence and do the UTC→local arithmetic.
Models are bad at that and hallucinate clock times. We add a structured
`timezone` field and compute local time **server-side** so the model only reads,
never computes. (This deliberately overturns the earlier "no persona-template
change" scoping decision — the template change is folded into this PR.)

---

## 1. Goal / Non-goals

**Goal:** reorder `build_prompt` into a long, byte-stable per-role prefix, render
the two unused fields (`system_prompt`, `gender`), and fix the time bug with a
structured per-persona timezone. Concretely:

- Lead with `genome.system_prompt` as a stable head block (hybrid: prose head
  **and** the existing structured sections; redundancy reinforces).
- Render `gender` in the identity line, with a conditional 铁律 reinforcement.
- Add an optional `timezone` field to `art_metadata`; declare it in the stable
  block and use it to precompute local time in `今日情境`.
- Consolidate per-turn-volatile blocks after the persona block; move `今日情境`
  to the end of the volatile cluster (recency aids time adherence); keep 铁律 /
  输出 last (rule adherence beats caching them — they are short).

**Non-goals (explicit follow-ups / out of scope):**

- **No suffix move.** Volatile content stays in the system message; we are *not*
  relocating it to the latest user message. Caching the appended **history**
  (the same-user-multi-turn win in the report) requires that move + per-model
  branching + verifying OpenRouter forwards `x-grok-conv-id`. Separate PR, gated
  on that verification.
- **No per-model prompt branching.** The reorder applies uniformly to all models.
- **No prod persona edits.** Prod personas are hand-tuned in DB (seed-personas
  are always manual). Backfilling `timezone` / confirming `system_prompt` /
  `gender` on prod genomes is the operator's manual job. Missing fields degrade
  gracefully (see 2.x fallbacks).
- **No change to message assembly** (`assemble_chat_request`), insight/affinity
  prompts, or the memory pipeline.

---

## 2. Design

### 2.1 Final prompt layout

Stable block (byte-identical per genome → cacheable; first per-turn-volatile
element ends the cached prefix):

1. `{system_prompt}` — authored prose head (NEW), omitted if empty
2. `你是 {name}，{gender_label}，{age} 岁，{mbti} 性格。你所在时区：{timezone}。`
   — `gender_label` + `timezone` clauses NEW, each omitted if absent
3. `【背景故事】\n{backstory}`
4. `【说话风格】{speech_style}`
5. `【口癖/习惯】{quirks_str}`
6. `【擅长话题】{topics_str}`
7. `【附加指引】{traits}` — at the cache-break boundary. Items 1–6 are shared by
   **all** users of the genome regardless of trait config; traits diverge only
   between *different* configs, so placing them last in the stable block
   maximizes the cross-config shared prefix while staying far from the
   generation point (the anti-recency goal for `nsfw_boost` etc.).

**—— cache break (everything below changes per turn) ——**

8. `【本轮风格】{style_text}` — PDE-chosen, first per-turn-volatile element
9. `【你对他的了解（通用画像）】\n{profile_str}`
10. `【你们之间的事（只有你和他知道）】\n{rel_str}`
11. `{attitude}` (`【你此刻的心情】`), `{state}` (`【你对他的内心感受】`),
    `{hints_section}` (`【当前内心状态】`) — affinity/hints cluster
12. `{gift}` (`【刚收到的礼物/红包】`) — only when gifts present
13. `【今日情境】\n{tc}` — moved to the end of the volatile cluster (recency)
14. `--- 【铁律 — 违反即失效】` ①–⑦ + conditional ⑧ gender rule (2.3)
15. `【输出】…` — final instruction, unchanged

The internal order of items 8–13 is **behavior-only** — all sit after the cache
break, so reordering among them has no cache impact; the order above is chosen
for coherence (know-him → know-us → my-feelings → events → time).

### 2.2 `system_prompt` head block (hybrid)

```rust
let head = {
    let sp = persona.genome.system_prompt.trim();
    if sp.is_empty() { String::new() } else { format!("{sp}\n\n") }
};
// full prompt = format!("{head}你是 {name}…")
```

Empty / whitespace-only `system_prompt` → no head block, no stray separator
(layout below it byte-identical to the no-head case). The structured sections
(背景/说话风格/口癖/擅长话题) are retained even when the head is present — the
redundancy reinforces the persona and the structured fields remain the source
for gender/timezone/age/mbti.

### 2.3 `gender` rendering + conditional 铁律

Read `gender` via the existing `meta_str(persona, "gender")` helper. Normalize
for display; the field may hold values beyond male/female:

- `"male"` → `男性`, `"female"` → `女性`
- anything else (e.g. `"non-binary"`) → rendered **verbatim**
- absent → gender clause omitted entirely

Identity line: `你是 {name}，{gender_label}，{age} 岁，{mbti} 性格。` when present,
falling back to the current `你是 {name}，{age} 岁，{mbti} 性格。` when absent.

**Conditional 铁律 ⑧ (reinforcement; redundancy = weighting):** appended only
when gender normalizes to a **binary** value (`male`/`female`). Skipped for
non-binary / verbatim / absent (they carry no fixed binary-anatomy constraint):

> ⑧ 你是{gender_label}，严格遵守自己的性别：身体结构、称谓、自我身份描述都以此为准，也不要被动接受用户错误的性别称呼；不要因为用户的称呼、上一轮内容、礼物、情境或调情而改变自己的性别。唯一例外：与用户的角色扮演中双方明确约定你暂时扮演其他性别

The role-play exception keeps consensual gender-swap fiction allowed. (The
clause was spelled out further during PR review — body/称谓/self-identity,
rejecting wrong-gender address, and resisting address/prior-turn/gift/context/
flirting pressure — but the conditional-on-binary-gender behavior is unchanged.)

### 2.4 `timezone` field + server-side local-time precompute

Add an optional `timezone` field to `art_metadata` (IANA name, e.g.
`"Asia/Tokyo"`; may be empty). Dependency: add **`chrono-tz`** (workspace dep +
`eros-engine-server`).

`now_context` resolves a single zone — the persona's own `timezone` when set &
valid, otherwise **SGT (`Asia/Singapore`, UTC+8) as the default** — and always
renders concrete local time. There is **no UTC-inference fallback**: the model
never does timezone arithmetic, which is what closes the time-hallucination bug.
The SGT default is a product decision — most users sit in UTC+8, so a persona
with no declared zone shares the user's wall clock rather than guessing.

```rust
fn now_context(timezone: Option<&str>) -> String { now_context_at(Utc::now(), timezone) }

fn now_context_at(now: DateTime<Utc>, timezone: Option<&str>) -> String {
    let tz = timezone
        .and_then(|s| s.trim().parse::<chrono_tz::Tz>().ok())
        .unwrap_or(chrono_tz::Asia::Singapore);
    let local = now.with_timezone(&tz);
    // weekday / date / HH:MM / period all computed in LOCAL time
    // period buckets (coarse): 05–07 清晨, 08–17 白天, 18–22 傍晚, else 深夜
    format!(
        "现在你当地时间（{tz}）是 {date}（{weekday}）{hh:02}:{mm:02}，{period}。\
         这是你唯一的时间基准；用户提到「今天/今晚/明天/昨天/刚才/现在」时一律以此为准，\
         不要编造其它日期或时间。",
        tz = tz.name(), …
    )
}
```

The block also renders the resolved zone id (`{tz.name()}`) and binds relative
date words (今天/今晚/明天/…) to this time, so the model can't drift to another
zone. This also fixes a latent correctness bug: weekday/date are now computed in
**local** time (the previous code derived weekday from **UTC**). The `timezone`
declaration in the stable block (2.1 item 2) is kept for persona self-awareness
+ reinforcement even though `今日情境` now states the concrete local time.

> **Note (review amendment):** §2.4 originally specified a UTC + infer-from-
> residence fallback for the no-timezone case. During PR review that was replaced
> with the SGT default above (simpler, and fully removes model-side arithmetic).

### 2.5 `build_prompt` signature / call sites

`build_prompt` already receives `persona`, so `gender`, `timezone`, and
`system_prompt` are read internally from it — **no signature change**, no change
at the two call sites (`handlers.rs:396`, `handlers.rs:454`). `now_context`'s
new arg is internal to `prompt.rs`.

---

## 3. Testing

Unit tests in `prompt.rs` (the three order/byte assertions at `:562`, `:589`,
`:621` and the time test at `:781` are rewritten for the new layout):

- **Order invariant:** `system_prompt` head < `你是` < 背景 < 说话风格 < 口癖 <
  擅长话题 < 附加指引 < 本轮风格 < 通用画像 < 你们之间的事 < 今日情境 < 铁律 < 输出.
- **Cache-break invariant:** every per-turn-volatile header (`本轮风格`,
  `今日情境`, `通用画像`, `你们之间的事`) appears **after** every stable header.
- **system_prompt:** non-empty → prose head present + exactly one `\n\n`
  separator before `你是`; empty/whitespace → no head, layout byte-identical to
  the no-head case.
- **gender:** `male`→`男性` and `female`→`女性` in the identity line + 铁律 ⑧
  present; `non-binary` rendered verbatim + **no** ⑧; absent → identity line
  has no gender clause + no ⑧; blank/whitespace → treated as absent (no
  double-comma, no ⑧).
- **timezone:** a fixed UTC instant + `Asia/Tokyo` renders the expected local
  date/weekday/HH:MM/period, the resolved zone id, and the "唯一的时间基准"
  anti-fabrication + relative-date binding; absent/garbage → **SGT default**
  (`Asia/Singapore`, UTC+8), never UTC. Weekday/date are computed in local time.
- **cache-prefix boundaries:** stable block (everything before `【本轮风格】`)
  is byte-identical across volatile-input changes (style/profile/affinity/hints);
  different trait configs share the persona block up to `【附加指引】` but
  produce different full prompts.
- **traits:** unchanged rendering, now asserted between `擅长话题` and `本轮风格`;
  empty traits omit the section with no stray whitespace.

---

## 4. Rollout / Out of scope

- `Cargo.toml`: add `chrono-tz` to `[workspace.dependencies]` and to
  `eros-engine-server`.
- `examples/personas/*.toml`: add a `timezone` field to document the convention
  (`kenji` = `Asia/Tokyo`; `aria`/`miel` underspecified → a plausible value or
  left empty to demonstrate the fallback). Docs only — no Rust code loads these.
- **Operator action (manual):** backfill `timezone` (and confirm `gender` /
  `system_prompt`) on prod `persona_genomes`. Until then, behavior degrades
  gracefully to the fallbacks above.
- Additive / non-breaking: every new field is optional with a fallback; existing
  personas keep working with no data changes.
- **Deferred to follow-up PRs** (see Non-goals): suffix move for history
  caching, per-model prompt branching, and `x-grok-conv-id` passthrough
  verification.

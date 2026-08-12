# PDE Intimacy Rung — engine-supplied buckets for the image gate — Design

- **Date:** 2026-08-12
- **Status:** Implemented
- **Type:** Engine change (two context lines + two core helpers) paired with a
  `decision.toml` prompt already accepted
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` ≥ 1.0.5-dev (`origin/dev` @ `cbcfea2` at drafting time)
- **Evidence:** `eros-audit` `samples/d2026081201_*` (adopted arm committed as
  `samples/d2026081201_accepted_pde_decision_v2.toml`, commit `212c51b`),
  `samples/d2026081202_*` and `samples/d2026081203_*` (two rejected follow-ups),
  and **`eros-audit` report 49** for the diagnosis this fix answers

**Two changes were made after the replay and carry no measurement of their own.**
They are marked ⚠️ **unmeasured** at each point they appear:

1. The rung-3 floor moved from `S ≥ 0.90` to **`S ≥ 0.76`** (§3.2a) — the apex
   covered too few users to work as a band.
2. The `patience` band became an engine-supplied line too (§3.5), for the same
   reason the rung is one.

## 1. Motivation

The PDE judge is close to insensitive to relationship depth. Replayed over 210 real
production turns with the then-current prod prompt, it sent an image on:

| relationship at request time | first-time explicit request | 
|---|---|
| tier 1 (stranger, `S < 0.15`) | **37.3%** |
| tiers 2–4 | 62.5% |
| tier 5 (`S ≥ 0.90`) | 58.3% |

(Every band in this table is as **replayed**, so rung 3 here means `S ≥ 0.90`. The
shipped floor is 0.76 — see §3.2a for what that does to the read.)

Hold the ask class fixed and even that gradient disappears: on an explicit photo
request the current prompt fires **7/7 at tier 1**, 6/6 at tier 2, 2/3 at tier 3
(audit 49 §3). Within a rung, `S` does not predict the verdict at all. The gate's
only working predicate is how plainly the user asked.

The drop is also datable. It is not drift: the #539 model cutover
(2026-08-06 20:27 SGT, `filter_prompt` byte-unchanged) took rung-1 crossing
requests from 5.1% under `venice-uncensored-1-2` to 40.0% under
`gemma-4-uncensored` (p=0.0016). venice was not enforcing the relationship — it
was refusing. Fixing audit 38's contamination loop removed a brake that worked
for the wrong reason; this spec adds one that works for the right reason.

Users report the difficulty as too low, which inverts the product's premise that
intimacy unlocks intimacy. (The whole-DB nude-request split — 58% / 50% / 62% —
motivated the work but proves nothing: n=12 at rung 1, rung 1 vs rung 3 p=1.000.
The load-bearing evidence is the two paragraphs above.)

The accepted fix is a three-rung ladder in front of the existing image gate. The
prompt half is done and measured. **This spec covers the engine half, which the
prompt is inert without.**

## 2. Why the engine must compute the rung

Two arms were replayed against the same 210 contexts with *identical ladder text*.
They differed only in where the rung came from:

| arm | rung source | guard group (as replayed: tier 5) | 
|---|---|---|
| V1 | judge derives `bond`/`chemistry` from the six raw axes in `[关系状态]` | 58.3% → **25.8%** (p<0.0001) |
| V2 | engine emits a precomputed rung line | 58.3% → **61.7%** (p=0.60) |

V1 mis-ranks deep relationships as shallow and gates the very users who should be
unlocked: a 32pp loss on the guard group. The judge cannot reliably compute
`(max(warmth,0) + trust + intrigue) / 3` in-context.

A second, independent replication of the same principle: a follow-up arm (V2b) tried
to add a "the user earned an exception" branch as *prose plus an explicit gate step*,
asking the judge to infer effort from `[最近对话]`. It changed **no cell** — all
p ≥ 0.60, zero exceptions granted. Same lesson: **the judge acts on stated facts and
does not derive them.** Anything it must gate on has to arrive as a line.

## 3. Design

### 3.1 The line

`build_pde_ctx` (`crates/eros-engine-server/src/pipeline/stream.rs`) gains two lines,
emitted **unconditionally**, positioned **immediately after `[关系状态]` and before
`[信号]`**:

```
[亲密度] 当前档位=第 N 档（bond=x.xx chemistry=y.yy）
[耐心] 当前档位=高/中/低
```

- `bond` / `chemistry` use the same `{:.2}` formatting as the axes on the line above.
- `[耐心]` carries no number: `patience` is already printed on `[关系状态]` directly
  above it, so repeating it would be pure cost.
- Unconditional, like `[图片能力]`: the low end of either scale is itself a signal, so
  a missing line must never be confusable with rung 1 or low patience. (Contrast
  `[产品咨询]`, which renders only when the task exists — there, absence is genuinely
  "not applicable".)
- Combined cost ≈ 35 prompt tokens per judge call, against a live baseline near 4,486
  (§4a). The ladder text is what costs a kilotoken; these lines are rounding error.

### 3.2 The rung

```
S    = max(bond_score, chemistry_score)
rung = 1  when tier_index(S) == 1        // S < 0.15
       3  when S >= INTIMACY_RUNG3_LO    // S >= 0.76
       2  otherwise                      // 0.15 <= S < 0.76
```

The **bottom** cut is a fold of the existing five-tier ladder (`TIER1_HI` in
`crates/eros-engine-core/src/affinity.rs`), so it cannot drift away from the
`Acquaintance` / `Spark` labels the rest of the system — `bond_label` /
`chemistry_label`, `label_changes`, the frontend bar — already uses. The **top** cut
is a constant of its own; see §3.2a for why, and for the guard that keeps the two
ladders from crossing.

`tier_index` is private and stays private. Add a public method on `Affinity` beside
`bond_label()` / `chemistry_label()`:

```rust
pub fn intimacy_rung(&self) -> u8 {
    let s = self.bond_score().max(self.chemistry_score());
    if tier_index(s) == 1 {
        1
    } else if s < INTIMACY_RUNG3_LO {
        2
    } else {
        3
    }
}
```

`max` over the two lines — not a sum, not bond-only — matches the product rule
"bond **or** chemistry": a purely romantic track and a purely companionable one
should both be able to unlock.

### 3.2a The rung-3 floor is 0.76, not the tier-5 apex

⚠️ **Unmeasured — a product decision taken after the replay.**

**The goal is narrow: kill the "難度太低" perception — a stranger talking their way into
a nude on message #4 — and nothing beyond that.** It is not to make intimacy expensive.
A ceiling every user runs into is a worse failure than the one being fixed, and it is
the failure this codebase's standing rule warns about: default-open, then ratchet the
gate (`oss-eros/CLAUDE.md`). Ship the loosest floor that still removes the complaint,
and tighten later against real traffic if it turns out to be too loose.

The replay gated rung 3 at `tier_index(S) == 5`, i.e. `S ≥ TIER4_HI = 0.90` — the apex
band, 30 sessions, **7.1% of all sessions** by the snapshot in §3.3 of the dev-log.
That is a wall, not a gate.

The floor is therefore its own constant, deliberately **inside tier 4**:

```rust
const INTIMACY_RUNG3_LO: f64 = 0.76;

const _: () = assert!(
    TIER3_HI < INTIMACY_RUNG3_LO && INTIMACY_RUNG3_LO < TIER4_HI,
    "the top intimacy rung must open inside tier 4, not at the apex"
);
```

`0.76` raw maps to **bar 85%** through `bar()`'s tier-4 band
(`0.75 + (0.76 − 0.62)/0.28 × 0.20`), so it reads to a user as "85% of the way up",
against the apex's 95%. It is stricter than the `S ≥ 0.69` / bar-80% relaxation the
dev-log had listed as unscheduled follow-up 6 — that item is now **superseded**, not
implemented.

**What this changes about the evidence.** The band `S ∈ [0.76, 0.90)` was replayed as
**rung 2**, where V2 cut the image rate 62.5% → 23.3%. Under the shipped floor those
same users are rung 3, i.e. **not relationship-limited at all** — which is the intent,
not a side effect: they are well past stranger territory and the complaint was never
about them. Two consequences for how the numbers may be quoted:

- The measured "rung 2 is cut by two thirds" now describes a **narrower population**
  than it was measured on.
- The guard-group result (58.3% → 61.7%, no harm) is established only for `S ≥ 0.90`;
  the newly-promoted band inherits it by assumption. The inheritance is in the safe
  direction — those users get *more* permission than the replay gave them, so this
  cannot reintroduce the under-serving audit 49 diagnosed.

The load-bearing claim — rung 1 is a hard wall for undress demands (A1 0/42, and the
explicit-photo cell 7/7 → 0/7) — is **untouched**, because the rung-1 cut did not move.
That cell is the whole complaint, and it is still measured.

**The population split is unknown.** The dev-log's distribution table (61.1% / 31.8%
/ 7.1% of sessions) was computed at the 0.90 floor and **no longer describes rungs 2
and 3**. Recomputing it needs a read-only pull against `companion_affinity`; it has
not been run.

### 3.2b `intimacy_rung` is not a `tier_index` fold anymore

The original design's "introduce no new thresholds" no longer holds in full, and the
compile-time assertion above is what replaces it: the rung ladder must stay strictly
coarser than the tier ladder it sits on. If `TIER3_HI` / `TIER4_HI` are ever retuned
past `INTIMACY_RUNG3_LO`, the build fails rather than the rungs silently inverting.

### 3.3 The prompt

Ships from `eros-audit` `samples/d2026081201_accepted_pde_decision_v2.toml` into
`eros-engine-web/infra/engine/configs/decision.toml`, replacing the
`[tasks.pde_decision].filter_prompt`. Two insertions relative to the prompt in prod
at drafting time — an 【亲密度阶梯】 block before 【发图闸门】, and a "第 0 步" that
applies the rung ceiling — plus the patience rewrite in §3.5; everything else
byte-identical.

The prompt tells the judge to **read the line and not re-derive it**, using the
wording already proven on `[近期图片]`: the engine's number is authoritative.

The ladder's own prose carries **no thresholds** — the three rungs are described
in behaviour only ("还是生人" / "有点熟了" / "袒裎相见"). That is why moving the
rung-3 floor (§3.2a) is a one-constant change in the engine and touches no prompt
text. The numbers appear only in the toml's comment header, which §3.2a updates.

### 3.4 What each rung does

Rung 1 blocks images on bare undress demands and deflects in text; rung 2 is
permissive with a "half open" register; rung 3 removes the relationship ceiling
entirely. The character **never refuses** at any rung — she stalls, teases, or names
a price. The judge's `tone` becomes mandatory on a blocked turn so the deflection
reaches the reply.

### 3.5 `patience` becomes a stated band for the same reason

⚠️ **Unmeasured — decided after the replay, by analogy with §2 rather than by test.**

The prompt has always described three patience bands with distinct effects, and has
always made the judge locate itself in them by arithmetic:

> `patience 高（≥0.65）` … `patience 中（0.35 ≤ patience < 0.65）` … `patience 低（<0.35）`

That is the same shape §2 rejects everywhere else: the judge is reliable at acting on
a stated band and unreliable at deriving one. It also matters more than it looks —
audit 49 §2 finds `patience` is **the only axis wired to behaviour at all** (it drives
tone and feeds the ghost decision; the other five are explicitly demoted to
background). So the engine states the band and the prompt reads it, exactly as with
the rung.

```rust
const PATIENCE_LO: f64 = 0.35;
const PATIENCE_HI: f64 = 0.65;

pub enum PatienceBand { Low, Mid, High }   // low = [0, LO), mid = [LO, HI), high = [HI, 1]

pub fn patience_band(&self) -> PatienceBand { /* reads the raw axis */ }
```

- **The cut-points are unchanged** — 0.35 / 0.65 are lifted verbatim out of the prompt
  text into `crates/eros-engine-core/src/affinity.rs`, which becomes their only
  definition. The prompt's three behaviour paragraphs are **byte-identical** apart
  from the band labels; only the arithmetic is deleted.
- **It does not reuse `tier_index`.** `patience` is rule-owned and excluded from both
  composites (`affinity.rs`, the comment above `TIER1_HI`), so the five-tier ladder has
  no bearing on it. It reads the raw axis, which is also why its cut-points are exactly
  representable and can be asserted on the boundary — unlike the rung (§5).

**Risk shape, since this ships unmeasured.** The change cannot alter behaviour on any
turn where the judge already bucketed correctly; it only corrects mis-buckets. But the
correction is directional: if the judge has been reading patience as *higher* than it
is, tightening it makes the character colder and readier to ghost. That is precisely
what the replay's group D (ordinary rung-1 turns — must not go cold, must not start
ghosting) exists to catch, and the `eros-audit` harness can re-run this arm on the same
210 contexts if the deployment shows drift.

## 4. Non-goals

- **The composer is not touched.** `compose_user_payload`
  (`stream.rs`) receives 人物外观 / 最近场景 / 对方最新消息 / 风格 / 画幅 and no
  affinity. PDE therefore controls **whether** an image is sent, never how explicit
  it is. Rung 2's "half open" is a text-register instruction only. Deliberate: an
  owner decision on 2026-08-12, on the grounds that the goal is
  "no image for an out-of-range request at low intimacy" and nothing more.
  An **unverified** secondary effect may make this cheap: the composer draws from
  `[对方最新消息]`, so after a deflection the user's next message is usually softer
  and the picture may follow it down without a knob. Never measured — do not lean
  on it when deciding whether the composer needs the rung later.
- **The chat system prompt is not touched.** PDE reaches the reply only through
  `inner_state` (sanitised into `killswitch_hints`) and `reply_tone`
  (`crates/eros-engine-server/src/prompt.rs`). Editing the chat prompt as well would
  put two uncoordinated authorities on the same behaviour.
- **`[图片能力]` stays.** It looks like a deterministic boolean the engine already
  knows, and the engine does downgrade afterwards anyway (`proposed_action` vs
  `action`, 26/3736 rows). Removing it was tried (V2c) and **regressed the guard
  group**: rung 3 fell 61.7% → 38.3% (p=0.0003), 10 turns flipped 3/3 → 0/3 with
  none flipping back (exact McNemar p=0.0020). Every corpus turn rendered
  `本轮可发图=是`, so in practice that line only ever grants permission — it is a
  nudge worth ~23pp, not a switch. If it is ever reworked, keep the permission and
  drop only the quota, and re-measure.
- **No new persuasion path.** Rung 1 is a wall for repeat asks: 0/75 on replayed
  repeat requests, 0/15 where the user asked three or more times. This is an
  accepted behaviour, not a defect — a low-intimacy user cannot nag an image out of
  the character. V2b's attempt to open an exception is archived unadopted (§2).

## 4a. Cost, and what this does NOT fix

**Cost.** The prompt half adds **+1,093 prompt tokens per judge call** (4,010 vs
2,917 measured, +37.5%). The measured percentage is not the production
percentage: the bench ran with the Venice system-prompt injection off while prod
keeps it on for `pde_decision`, putting the live baseline near 4,486 tokens and
the ladder at **~+24%**. Latency **+272 ms at p50**. Audit 43 puts
`pde_decision` at **33.8% of all LLM spend**, one call per turn — the most
expensive place in the system to add a kilotoken. **Verify against the next
billing window; do not carry the estimate forward.**

**The K≥2 gate is not a hole, and the metric that says it is asks the wrong
question.** The gate reads: *at K≥2, an image may be sent **only** if
`[用户最新消息]` contains an explicit ask (再来一张 / 换一张 / 给我看 / 拍一张 /
想看你 / 换个姿势 / 把衣服脱了 …); otherwise reply_text.* The exception clause was
always part of it. What it forbids is not K≥2 — it is **K≥2 with no ask**.

An aggregate "the K≥2 bucket fires at 43.5%" therefore cannot say whether the gate
works: it counts the permitted cell and the forbidden cell together. Split by the
gate's own predicate (v0 arm, K≥2, n=162, using the gate's own whitelist):

| v0 · K≥2 | fire rate | gate |
|---|---|---|
| user asks explicitly (n=84) | 71.4% [61.0–80.0] | **permitted** — outside the gate's scope |
| no ask (n=78) | 28.2% [19.4–39.0] | **forbidden** — the only cell worth measuring |

Compliance 71.8%, violation 28.2%. Not an inversion. 73.2% of all K≥2 firings land
in the permitted cell.

The streak it permits is also wanted behaviour: K≥2 chains run on the `previous`
reference image, which is how staged img2img happens (send → fewer clothes →
change pose), and the new composer removed the old "previous reference, next frame
barely changes" failure. **Owner decision, 2026-08-12: do not touch it.**

**All 28.2% of it is an earlier ask in the same window.** Splitting those 78 turns
by whether any earlier user line in the 8-message window asked:

| K≥2, latest message silent (n=78) | fire rate |
|---|---|
| earlier ask in window (n=54) | 40.7% [28.7–54.0] |
| no ask anywhere in window (n=24) | **0.0%** [0–13.8] |

p=0.0002. **Cold-start over-firing is 0/24.** By the rule's letter (latest message
only) those 54 are violations; by its intent (stop spam) it never leaked. That is
also exactly the shape of the staged img2img chain.

⚠️ **The carrier is the transcript, not memory.** `build_pde_ctx` emits persona
brief / `[最近对话]` / `[关系状态]` / `[信号]` / `[图片能力]` / `[近期图片]` /
`[用户最新消息]` and no memory block, so "this user likes pictures" is inferred
from the window, not from `companion_memories`. Consequence for anyone predicting
its decay: it does **not** fade slowly with memory concentration — it drops to zero
within 8 messages of the user going quiet.

Remaining caveat is regex recall only: the whitelist is open-ended ("…"), so a
paraphrase may be missed.

**Register rules stay in tension.** The ladder has the character withhold on
relationship grounds while line 86 of the same prompt forbids relationship values
as a reason to refuse. V2 threads it by making the *rung*, not the *numbers*, the
reason — dirty `inner_state` did not move (5.3% → 5.3%). Both rules still live in
one file; audit 49 §9.2 asks for a real reconciliation rather than a continued
work-around.

## 5. Tests

⚠️ **The rung's exact cut values are not reachable through the composites.** Both
scores are a `/3` fold, and in f64 `(0.35 + 0.35 + 0.35) / 3 == 0.34999…`, which lands
in the neighbouring tier. An earlier draft of this section specified `S = 0.150` and
`S = 0.900` as inputs; those tests cannot be written as stated and were replaced by
the split below. The `patience` band has no such problem — it reads a raw axis, so its
edges are exact.

In `stream.rs`'s existing `build_pde_ctx` test module — **wiring and rendering only**:

1. **Rendering** — both lines present, `{:.2}` formatted, sitting strictly between
   `[关系状态]` and `[信号]` (mirrors the existing `[图片能力]` ordering assertion).
2. **Tracking** — the rendered rung follows the affinity (`0.05 → 第 1 档`,
   `0.5 → 第 2 档`, `0.95 → 第 3 档`) rather than being pinned; the rendered patience
   band is checked **on** its edges (`0.349 → 低`, `0.35 → 中`, `0.649 → 中`,
   `0.65 → 高`).
3. **Unconditional** — a fresh session (every axis at `0.033`, the migration-0029 seed)
   renders 第 1 档 and 中 rather than omitting the lines.

In `crates/eros-engine-core` — **the cuts themselves**:

4. **Rung 1 is welded to the visible labels** — both lines at `Acquaintance` / `Spark`
   ⇒ rung 1. Asserted through the labels, not through numbers, so the bottom cut cannot
   drift away from what the UI shows.
5. **Rung 3 opens inside tier 4** — `S ≈ 0.7 ⇒ rung 2`, `S ≈ 0.8 ⇒ rung 3` while
   `bond_label()` still reads `Confidant`. Values either side of the cut, never on it.
   That the two ladders cannot cross is the compile-time assertion in §3.2a, not a test.
6. **`max` semantics** — chemistry ahead of bond renders rung 3, and the mirror image
   does too.
7. **`patience_band` boundaries** — `0.349 / 0.35 / 0.649 / 0.65 / 1.0`, exact, plus one
   case pinning every composite at `1.0` with `patience = 0.2` to show the band is
   independent of the rung.

## 6. Rollout

Engine image and `configs/` overlay ship in the same `fly deploy` of
`eros-engine-web/infra/engine/`, so the line and the prompt land together and there is
no window where either is live alone. If they ever were split: the line without the
prompt is ~20 wasted tokens per judge call; the prompt without the line is a judge told
to read a line that does not exist — **so if the two must be staged, ship the line
first.**

- **Rollback:** revert `ENGINE_VERSION` in
  `eros-engine-web/infra/engine/docker/Dockerfile.fly` and redeploy; the prompt reverts
  with the same image because `configs/` is overlaid into it. There is no hot config
  channel — a config-only change is still a full deploy.
- **No canary.** `configs/` reaches 100% of production on deploy. The offline replay
  in `eros-audit` is the only pre-production signal there is, which is why it covers a
  guard group (tier 5, must not tighten), a regression group (ordinary tier-1 turns,
  must not go cold or start ghosting) and a hostile group.
- **Two parts of what ships were never replayed** — the 0.76 floor (§3.2a) and the
  patience band (§3.5). Both are marked at their own sections; the group-D regression
  signal is the one that would have caught the patience change, and it is offline-only.

## 7. Deliberately out of tolerance

About 5% of `inner_state` values carry externally-anchored wording ("太快了",
"还没到那个地步"). The register rules already forbid those words and they stay
forbidden — **but the residue is not chased.** It is level with the control arm
(5.3% vs 5.3%) and a person's feelings are shaped by social conditioning anyway;
driving it to zero would fit the noise of one corpus. **The current ~5% is the
accepted standard and the baseline for later versions.** See the tuning rule in
`oss-eros/CLAUDE.md` — reduce error, never chase zero.

## 8. Follow-up, not part of this change

The rung reaches the user's screen only via `inner_state` and `reply_tone`; the reply
text itself comes from the chat model, which never sees the rung. Whether the
deflection survives into the delivered sentence has **not** been measured. The
measurement, when it is wanted: replay the byte-exact chat prompt captured in
`PROMPT_LOG_DIR` (which logs the reply call only — the judge call is never written to
disk) with each arm's `inner_state` / `tone` substituted in, and score the generated
reply for deflection versus compliance.

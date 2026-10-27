# Affinity-gated profile recall — Design

- **Date:** 2026-10-27
- **Status:** Approved
- **Type:** Engine change, read side only: one pure gating function in core,
  applied at every site that injects the user profile or the memory layers
  into a prompt. No schema change, no API shape change, no prompt change.
- **Owner:** enriquephl (sole dev)
- **Target:** `eros-engine` — `eros-engine-core` (the gate) and
  `eros-engine-server` (`pipeline::handlers`, `pipeline::group`,
  `pipeline::voice`, `eros-engine-store::memory` for the recall queries).
- **Builds on:** `2026-10-26-insights-2-fact-ledger-design.md` (the dense
  derived profile), the #40 injection scopes (`core::scope`),
  `2026-08-17-affinity-41-design.md` (the five-tier ladder),
  `2026-07-15-insight-memory-enrichment-design.md` (the per-memory
  `metadata`).

## 1. Motivation

Insights 2.0 turned `human_insights` from a sparse row into a dense one: for
active users most fields are now populated, `kinks` included. Recording
everything is intended — the ledger and the derived profile exist for profile
extraction — but injection reads whatever the caller's `memory_scope` allows,
with no regard for the relationship reading it:

- Under `full` / `insights_only`, the eight intimate fields render from the
  first message of a brand-new relationship.
- The profile memory layer (`engine.companion_memories`, `instance_id IS
  NULL`) is shared by every character and recalled by similarity whenever the
  scope keeps it (`full` / `neutral_and_relationship`). What the user built up
  with one character reaches a stranger intact. On a production deployment,
  `sexuality` is the largest domain among this layer's dreaming rows — about
  28% of them, from about a quarter of the users who have any.

Together these let a user steer a low-affinity character through memory: it
is handed preferences — sexual ones included — that it has no in-relationship
reason to know, and treats them as context to act on.

People don't work like that. Someone who barely knows you doesn't know what
you've told others, doesn't go along with your preferences even when they do
know them, and doesn't pay close attention to what you say. This design gives
characters the same shape: **how much of the user's profile reaches the
prompt depends on how close this character is to the user.** What the engine
records does not change.

## 2. Decisions

1. **Read side only.** Fact extraction, the ledger, profile rebuilds,
   dreaming writes, `write_turn`, exports and erasure are unchanged. The gate
   decides what is injected, never what is stored.
2. **`memory_scope` stays the ceiling; affinity narrows inside it.** The gate
   only removes what the scope would inject; it never adds a field, a layer or
   a memory the scope excludes. A `neutral_*` scope renders no intimate field
   to begin with, so its 基础画像 is unaffected; its memory layers (where the
   scope keeps them) are gated like any other.
3. **The gate reads the relationship's current, time-decayed tiers**:
   `Affinity::bond_tier()` and `Affinity::chem_tier()` (`core::affinity`,
   1..=5), on the same decayed affinity the rest of the turn uses.
   - Not `AffinityScope` — that only picks which axes the prompt *shows*.
   - Not `intimacy_rung` — a coarser three-rung reading tuned for the image
     gate.
   - Not a peak: when a line drops a tier, what it had unlocked leaves the
     prompt again.
   - An unrecorded relationship (`Affinity::unrecorded()`, or no pair row on
     the voice path) is tier 1 on both lines.
   - Naming hazard: `AffinityScope::bond()` groups different axes than
     `Affinity::bond_score()`. That split is deliberate and stays; the gate
     uses the `Affinity` tier methods only.
4. **Intimate 基础画像 fields: one line and one minimum tier per field.** A
   field renders when the scope renders intimate fields at all *and* its
   line's tier is at or above its minimum:

   | Field | Prompt label | Line | Min tier |
   |---|---|---|---|
   | `love_values` | 感情观 | chemistry | 2 (flirtation) |
   | `relationship_history` | 感情经历 | chemistry | 3 (crush) |
   | `kinks` | 性癖 | chemistry | 4 (lover) |
   | `interests` | 兴趣 | bond | 2 (friend) |
   | `emotional_needs` | 情感需求 | bond | 3 (close_friend) |
   | `family` | 家庭 | bond | 3 (close_friend) |
   | `finance_status` | 经济状况 | bond | 4 (confidant) |
   | `mental_health` | 心理健康 | bond | 4 (confidant) |

   Each field reads only its own line. A single combined reading would let a
   user build friendship to unlock romance-side knowledge. The neutral
   fields (occupation, education, MBTI, ideology, life rhythm, social
   pattern, personality traits, future plans) are not gated.
5. **The profile memory layer as a whole scales with the higher of the two
   tiers**, `max(bond_tier, chem_tier)`. With `K` the site's current
   constant (text: `grouped_k = 2` per category, `raw_k = 4`; voice: `1`,
   `2`):

   | Higher tier | Profile-layer K |
   |---|---|
   | 1 | 0 — layer not recalled |
   | 2 | ⌈K / 2⌉ |
   | ≥ 3 | K (today's value) |

   Both halves of the layer follow it: the per-category grouped recall and
   the uncategorized fallback. At tier 1 a character knows only what this
   relationship has shown it.
6. **Within that K, rows are routed by the `domain` they already carry.**
   Dreaming rows store the extraction prompt's per-memory labels in
   `metadata`, `domain` among them. A row whose `metadata->>'domain'` is in
   the table below is recalled only when its line has reached the minimum,
   mirroring the matching 基础画像 field:

   | `domain` | Line | Min tier | Mirrors |
   |---|---|---|---|
   | `sexuality`, `kink` | chemistry | 4 | `kinks` |
   | `romance` | chemistry | 2 | `love_values` |
   | `family` | bond | 3 | `family` |
   | `finance` | bond | 4 | `finance_status` |
   | `health`, `mental_health` | bond | 4 | `mental_health` |

   - Any other domain, and rows with no `domain`, follow decision 5 only.
   - This covers existing rows the day it ships, with no prompt change: on
     the production deployment above about 96% of this layer's dreaming rows
     already carry a `domain`.
   - The engine starts reading one prompt-owned label for gating. The
     2026-07-15 rule ("engine stores shape, prompts own vocabulary") gives way
     here for recall only, as the 2.0 ledger already did for validation.
     Unknown values are simply ungated beyond decision 5, so a deployment
     with a different vocabulary — or none — degrades to the tier ramp, never
     to an error. `kink` and `mental_health` are listed because they are the
     2.0 ledger's names for the same split; a deployment that aligns its
     dreaming prompt with the ledger stays covered.
   - Uncategorized fallback rows (`category` and `metadata` NULL; users the
     dreaming sweeper hasn't reached yet) can't be routed. They are accepted
     as residual: they surface only from tier 2, and only for those users.
7. **Relationship-layer recall shrinks at low tiers and never grows.**
   `K = min(K_today, max(bond_tier, chem_tier))`: text (`K_today = 3`)
   recalls 1 / 2 / 3 at tiers 1 / 2 / ≥3; voice (`K_today = 2`) recalls
   1 / 2. This layer is what the user said to this character, so its content
   is not filtered — only how much of it the character holds onto.
8. **One gate, every injection site.** 1:1 text replies (every event that
   goes through the reply builder), group replies (each seated member gated
   by its own affinity), and voice (the 基础画像 snapshot and per-turn
   recall). The policy is one pure function in core; sites only apply its
   result.
9. **No new round trips.** Every site already holds, or concurrently loads,
   the affinity it needs. Where the affinity loads in parallel with recall
   (voice), recall fetches at today's K and the gate trims afterward; the
   affinity read must not be serialized in front of recall.
10. **Constants in code, no switch.** Thresholds live next to the tier cuts,
    like every other tier-derived rule — no env var, config key or request
    field. Moving a threshold later is a constant change in a release.

## 3. The gate

A pure function in `eros-engine-core` (placement the builder's; `scope.rs` is
the natural neighbor) from an `Affinity` to a value that says:

- which intimate 基础画像 fields may render (decision 4);
- the profile-layer K factor — none, half, full — (decision 5) and which
  domains are withheld (decision 6);
- the relationship-layer K cap (decision 7).

Sites combine it with the existing `MemoryScope::resolve()`: the scope still
decides which layers are on and the `InsightMode`; the gate narrows within
that. `human_insights_to_bullets` takes the gate as an extra input, with its
Full/Neutral split remaining the outer filter.

## 4. Injection sites

- **Text 1:1 — `handlers::build_reply_request_with`.** The turn's
  `DecisionInput` already carries the affinity it runs with. At tier 1 the
  profile-layer searches are skipped outright (fewer queries, not more).
- **Group — `pipeline::group`.** Each seat's decayed, read-only affinity,
  through the same builder.
- **Voice — `pipeline::voice`.**
  - **Decay.** The pair row from `load_latest_for_pair` gets
    `apply_time_decay()` + `refresh_endpoints()` once after the join, as text
    and group already do, and both the voice relationship line and the gate
    read that decayed value. The relationship line on an idle pair therefore
    reads decayed tiers from now on, matching text.
  - **The 基础画像 snapshot is gated per turn, not at freeze time.** The
    snapshot in `chat_sessions.metadata.voice_bootstrap` stays write-once and
    keeps its shape; each turn filters the frozen bullets through that turn's
    gate, by each bullet's label. Markers frozen before this release are
    covered the same way, and the prefix stays byte-stable while the tiers
    don't move.
  - **Recall** follows decisions 5–7 against the voice constants.
- Not injection, not gated: the profile snapshot sweeper, the user reading
  their own profile (`GET /comp/user/{user_id}/profile`), and the exports.

## 5. Recall queries

- The domain filter (decision 6) applies to the profile layer only — the
  grouped search and, for completeness, the fallback search. The
  relationship layer is never domain-filtered.
- **The fallback decision is made on the unfiltered grouped result.** Today
  an empty grouped result falls through to the raw search, which returns
  every profile-layer row, categorized ones included. Withholding rows must
  never empty the grouped result and trigger that fall-through.

## 6. Observability

`PROMPT_LOG_DIR` already captures each rendered prompt; that is the readout
for tuning the cuts. Each site's existing scope-resolution tracing line gains
the gate's inputs and outputs: both tiers, the number of intimate fields
withheld, the number of memory rows withheld by domain, and the K values
used.

## 7. Out of scope

- Every write path (decision 1), and the neutral 基础画像 fields.
- The dreaming prompt and its vocabulary; no new category.
- Peak / high-water tiers ("once known, always known").
- `memory_scope` semantics and defaults, and anything a caller sends.
- The PDE judge, world and story directors, life events — none of them reads
  the profile or the memory layers.
- Telling the character that anything is being withheld; withheld content is
  simply absent.

## 8. Testing

- **Core — the gate.** Each field and each routed domain just below and at
  its minimum on its own line, and not unlocked by the other line at tier 5;
  profile-layer K at tiers 1 / 2 / 3 / 5 against both the text and voice
  constants; the relationship cap; unrecorded affinity is tier 1 everywhere.
- **Server — text and group.** Bullets under scope × gate: a Neutral scope at
  tier 5 renders no intimate field, a Full scope at tier 1 renders none, and
  fields unlock one by one. Recall against seeded profile rows across the
  routed domains, rows with an unrouted or absent domain, and relationship
  rows, per tier. A user whose only grouped rows are withheld does not fall
  through to the raw search. Tier 1 on the text path issues no profile-layer
  query. Group seats are gated by their own affinity.
- **Server — voice.** The frozen snapshot is filtered per turn and changes
  when a tier moves; a marker in the pre-release shape is filtered the same
  way; the gate and the relationship line read the decayed affinity.

## 9. Documentation

- `docs/memory-layers.md` + `.zh.md`: the profile layer no longer reads as
  "anything any persona can know" — it reaches a persona according to that
  persona's closeness to the user. The retrieval and injection section
  describes the gate and the domain routing.
- `docs/insights.md` + `.zh.md`: the injection section — intimate fields by
  line and tier.
- `docs/affinity-model.md` + `.zh.md` + `.ja.md`: the tiers gain a consumer;
  voice reads decayed tiers.
- `docs/api-reference.md` + `.zh.md`: `memory_scope` is a ceiling; what
  reaches the prompt within it also depends on affinity.
- `docs/model-config.md` + `.zh.md`: the `memory_extraction` output's
  `domain` label now drives recall gating; the values the engine routes on.
- `docs/migrating/` note for the release: a behavior change every
  deployment gets on upgrade with no configuration; a deployment whose
  dreaming prompt emits no `domain` gets the tier ramp only.

## 10. Left to the builder

- Module, type and function names; the shape of the gate value.
- Per site, whether K trimming and domain filtering happen in SQL or in
  Rust, within decision 9 — including whether withheld rows are dropped
  before or after the per-category K is taken.
- How a frozen voice bullet is matched to its field (the label prefix is
  sufficient today).
- Test fixtures and placement.

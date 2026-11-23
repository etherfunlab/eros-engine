# Affinity 5.0: relationship status and composed relations — Design

- **Date:** 2026-11-23
- **Status:** Approved
- **Type:** Engine change. One migration (two columns on
  `engine.companion_affinity`). Wire: three fields added to every affinity
  snapshot, four life-choice kinds added, one retired, one user action added,
  one proactive occasion added.
- **Owner:** enriquephl (sole dev)
- **Builds on:**
  `2026-08-16-affinity-40-design.md` and `2026-08-17-affinity-41-design.md`
  (the two lines, tiers, endpoint derivation: all unchanged),
  `2026-10-19-life-events-shopping-gatherings-and-ask-for-space-design.md`
  (the life-choice mechanism; its `ask_for_space` is retired here),
  `2026-10-30-give-needs-romantic-choices-and-answer-receipts-design.md`
  (romantic choices, the once-only rule),
  `2026-11-23-twelve-together-actions-and-propose-design.md` (the user's
  `propose`),
  `2026-11-23-sleep-together-accept-places-event-design.md`.

Marks: *(owner's ruling)* was decided by the owner in the design
conversation. *(my call)* was filled in by the author and is open to
correction.

## 1. Intent

Affinity 4.x says how close a pair is (two lines, five tiers each) but not
what they are to each other. 5.0 adds two things on top of the unchanged 4.x
base:

- **Composed relation.** A name for the pair's current bond tier × chemistry
  tier combination. Derived, text only.
- **Relationship status.** What the pair has agreed to be, moved only by
  life choices: a confession, a partnership offer, a proposal, a break-up.
  Stored. A **cold war** can sit on top of any status.

Why now: the romantic life choices already exist but lead nowhere. An
accepted proposal changes nothing after the reply that answers it.

## 2. How it stands

- The text-chat prompt never sees the tier labels. `[relationship]` is one
  noun from a warmth × patience quadrant (`relationship_ground`,
  `crates/eros-engine-server/src/prompt.rs`). Tier labels reach only the
  voice prompt and the group director.
- Her asks are rows in `engine.life_choices`, triggered after each judged
  1:1 text turn in `pipeline/life_choice.rs::maybe_ask`, answered through
  `routes/life_choices.rs`. Kinds: `ask_for_space`, `sleep_together`,
  `propose`, `travel_together`, `bathe_together`.
- The user's `propose` is a chat action decided by the PDE judge; nothing is
  stored beyond the message's `metadata.action`.
- `ask_for_space` fires on a chemistry-over-bond gap of two tiers plus three
  judged turns with derived `patience < 0.5`. The patience half barely
  filters: the judge's middle patience level derives to at most `0.5`.
- A kind that was accepted or ignored once is never asked again in the
  session (`LifeChoiceRepo::settled`).

## 3. Goals, non-goals, anti-goals

**Goals.**
- Bond and chemistry, their axes, tiers, labels and the write pipeline stay
  exactly as in 4.x *(owner's ruling)*.
- A composed relation whenever either line is at tier 2 or above; it changes
  her prompt text and nothing else *(owner's ruling)*.
- A stored relationship status per session, moved by life choices, with a
  cold-war overlay.
- New asks: `confess`, `partner_up`, `break_up` (hers); `break_up` (the
  user's). `propose` gains a prerequisite and a consequence.
- `ask_for_space` stops being a choice: the pair goes straight into a cold
  war *(owner's ruling)*.

**Non-goals.**
- **Marriage.** `engaged` is the top status and there is no `married`
  *(owner's ruling)*. Reasons: a user engaged to several personas carries
  little weight, a marriage would not; and an engagement leaves the
  relationship open to imagination, which is the point of AI companionship.
  Do not add a `marry` choice later.
- Composed relations gating anything: no event, life-choice or action
  threshold reads them *(owner's ruling)*.
- Any other life choice moving the status *(owner's ruling)*:
  `travel_together`, `bathe_together`, `sleep_together`, together actions.
- Voice and group prompts. They keep their tier-label lines.
- Journal milestones or anniversaries for status changes *(my call: a
  follow-up)*.
- Backfilling status from choices already answered. Every session starts at
  `none` *(my call, the forward-only precedent)*.
- User gender or orientation. The engine still receives neither.

**Anti-goals.**
- A second source for the status. It is not re-derived from `life_choices`
  rows or chat messages anywhere; the column is the authority.
- A cold war that cannot end (see §6.3).

## 4. Approaches

Where the status lives:

1. **Derive on read** from `life_choices` rows, accepted user actions in
   chat messages, and a replay of judged turns for the cold war. No
   migration. But the cold war needs the turn history replayed every turn,
   three sources must be ordered, the stacked statuses make the fold
   order-sensitive, and SQL consumers cannot read it.
2. **Two columns on the session's affinity row**, written in the
   transaction that settles the choice or commits the turn, with one affinity
   event row per change as the audit trail. One migration. The row is already
   loaded every turn, so no extra round trip. *Taken (owner's ruling).*
3. **A status-event table**, current status = latest row. Cleanest history,
   but one more read per turn and one more table, recording what option 2
   puts in affinity events.

## 5. Composed relation

A pure function of `(bond_tier, chem_tier)` in
`crates/eros-engine-core/src/affinity.rs`, beside the labels. `None` at
`(1, 1)`. One name per cell *(owner's ruling: candidate A of three)*.

Working copy; the owner reviews the names one by one before merge, so keep
them in one table that is cheap to edit:

| bond \ chem | 1 | 2 | 3 | 4 | 5 |
|---|---|---|---|---|---|
| **1** | none | 有好感的陌生人 `fond_stranger` | 让你上头的陌生人 `heady_stranger` | 露水情人 `fling` | 一见倾心的人 `love_at_first_sight` |
| **2** | 普通朋友 `casual_friend` | 有点暧昧的朋友 `flirty_friend` | 暗恋的朋友 `secret_crush` | 朋友以上的情人 `friend_and_lover` | 热恋的人 `new_flame` |
| **3** | 好朋友 `good_friend` | 友达以上 `more_than_friends` | 恋人未满 `almost_lovers` | 亲密的情人 `close_lover` | 难分难舍的人 `inseparable` |
| **4** | 知己 `confidant` | 心照不宣的知己 `knowing_confidant` | 暗生情愫的知己 `smitten_confidant` | 知心爱人 `heart_lover` | 挚爱 `beloved` |
| **5** | 灵魂之交 `kindred_spirit` | 动了心的灵魂之交 `stirred_kindred` | 柏拉图式的爱人 `platonic_love` | 灵魂爱人 `soul_lover` | 命中注定的人 `destined` |

Names avoid the status words (伴侣, 伙伴, 同事, 前任, 未婚) so the two never
read as the same claim.

**Incompatible pairs** *(owner's ruling)*. Chemistry accrues for every pair,
since scope only gates the read side. In the prompt, a pair whose scope is
not `romance_compatible` takes its name from the chemistry-1 column
(普通朋友, 好朋友, 知己, 灵魂之交; none at bond tier 1), the same reading as
"an incompatible pair is never confessed to". The prompt builder passes the
turn's own scope. The API reports the cell of the real tiers: a snapshot
carries no scope, and the caller knows whether it marked the pair
compatible.

## 6. Relationship status

### 6.1 States

Eight statuses, one per session: `none`, `dating` (伴侣), `partner` (伙伴),
`peer` (同事), `fwb`, `situationship`, `engaged` (未婚), `ex` (前任). The
cold war is a separate flag over any of them, so ending it uncovers the
status underneath unchanged.

The *breakable* statuses are `dating`, `partner`, `fwb`, `situationship`,
`engaged`.

### 6.2 Transitions *(owner's ruling unless marked)*

| From | What happens | To |
|---|---|---|
| `none`, `ex` | confession accepted | `dating` |
| `none`, `ex`, `peer` | partnership accepted | `partner` |
| `none`, `ex`, `peer` | partnership declined | `peer` |
| `partner` | confession accepted | `fwb` |
| `peer` | confession accepted | `situationship` |
| `dating`, `fwb`, `situationship` | proposal accepted | `engaged` |
| any breakable status | break-up accepted | `ex`, and the cold war ends *(my call)* |
| any breakable status | break-up refused | status kept, cold war on |

A declined confession or proposal leaves the status as it is. `none` is left
once and never returned to; `ex` is the starting point after a break-up and
offers the same asks as `none`. `dating`, `engaged`, `fwb` and
`situationship` are never offered a partnership, even when bond later
overtakes chemistry.

The machine is closed: every status is reachable from `none`, every status
has a way out, and `ex` leads back to the start.

### 6.3 Cold war

- **Enters** when, among the judged turns since the anchor (§7.1), the
  newest three all have the judge's lowest patience level, or five of the
  newest seven do *(owner's ruling)*; or when a break-up is refused. No tier
  gap, no compatibility and no status requirement *(owner's ruling)*.
- **While it lasts** she keeps replying, coldly: `[relationship]` says so
  *(owner's ruling)*. Every positive committed delta on the four line axes is
  halved; nothing drops on entry *(owner's ruling)*. She makes no warm ask
  (§7.3) *(my call)*.
- **Ends** when the newest three judged turns since it began all have a
  patience level above the lowest *(my call)*, or when a break-up is
  accepted.

The levels are the judge's own (`patience_grade`, 1 to 3, already in each
turn event's `state_after`), not the derived `patience` value. The derived
value is the level times a boost from the bond line, so with a low bond even
the top level stays under `0.5`, and a threshold on it could lock a new
relationship into a cold war for good. The exit is deliberately easy: her
cold tone drags the judge's read down, and a hard exit could lock itself.
Raise the bar later if cold wars prove too short.

The halving replaces the 72-hour slowdown after an accepted separation
(`LifeChoiceRepo::growth_slowed`, `growth_scale_for`): same scale, same
call sites (the turn commit and the date settlement), now keyed on the cold
war being on. Slowdown windows open at deploy end there *(my call)*.

### 6.4 Storage

Migration `0088`, on `engine.companion_affinity`:

- `relationship_status text NOT NULL DEFAULT 'none'`, with a `CHECK` on the
  eight keys.
- `cold_war_since timestamptz` (`NULL` = no cold war).

Every change of either writes one row in `engine.companion_affinity_events`
in the same transaction, `event_type = 'relationship'`, with `context`
carrying `from`, `to`, `cold_war` (`entered` | `ended`, when it changed) and
its cause (`choice_id`, or the row's `user_message_id` for a user action;
neither for a streak-driven cold war). Like `life_choice` rows it is never
returned by the per-turn delta route and is not a judged turn. The migration
also widens `companion_affinity_events_event_type_check` (last set in
`0085`) to admit `relationship`. No new foreign keys: both columns sit on a
row that already cascades with its session.

### 6.5 BYOC sessions

A BYOC session stays outside the status system *(my call)*: her asks never
fire there and the answer endpoint does not serve it, so nothing could ever
move it out of `none`. Its status is always `none`, no cold war starts
there, the user's `propose` is judged exactly as today and moves nothing,
and the user's `break_up` is refused with no effect. The gates in §8 and the
cold-war check therefore read the instance kind first.

## 7. Triggers and asks

### 7.1 Shared rules

- **Leading line.** The line with the higher tier leads; on equal tiers, the
  higher score *(my call)*.
- **Compatibility** is `romance_compatible` over the session's latest
  recorded scopes, as today. It is read when she asks. A status already
  reached is not undone when the scopes change *(my call)*.
- **Anchor.** Streaks count judged turns after the later of the existing
  streak anchor (`LifeChoiceRepo::streak_anchor`) and the session's latest
  `relationship` event *(my call)*: any change of status or cold war starts
  a fresh streak.
- **Asking again.** The once-only rule (`settled`) does not apply to
  `confess`, `partner_up`, `propose` or `break_up`, or a second lap after a
  break-up could never happen. They are gated by the current status, and
  `confess` and `partner_up` additionally wait 14 days after the session's
  latest status change and after that kind's last answer of any sort *(my
  call)*. `propose` keeps its existing re-ask behaviour after a decline and
  waits 14 days after an ignore *(my call)*. `sleep_together`,
  `fall_asleep_together`, `bathe_together` and `travel_together` keep their
  current rules.
- Runway, session and delivery rules are unchanged: one pending ask per
  instance, 1:1 text session of a genome instance, 24-hour window, the
  `life_choice` field on the delta, typed replies convert to `ignored`.

### 7.2 Her asks

| Kind | Status she asks from | Trigger | Accepted | Declined |
|---|---|---|---|---|
| `confess` | `none`, `ex`, `partner`, `peer` | compatible; chemistry leads and is at tier 3+; the newest 3 judged turns have `warmth > 0.7` | §6.2 | status kept; chemistry drops one tier *(my call, as `sleep_together`)* |
| `partner_up` | `none`, `ex`, `peer` | bond leads and is at tier 3+; the newest 3 judged turns have `patience > 0.7`; compatibility not read | `partner` | `peer`; nothing drops |
| `propose` | `dating`, `fwb`, `situationship` | today's: compatible, both lines at tier 4+, 5 warm turns of the newest 8 | `engaged` | status kept; both lines drop one tier (today's) |
| `break_up` | any breakable status, cold war on | the newest 3 judged turns since the anchor all have the lowest patience level *(my call)* | `ex` | status kept; the cold war stays |

- An incompatible pair whose chemistry leads is asked nothing; once bond
  overtakes, she offers a partnership *(owner's ruling)*.
- `confess` reads only warmth and `partner_up` only patience *(my call)*.
  The existing "warm turn" needs both above `0.7`, but warmth is boosted by
  chemistry and patience by bond: with the other line under about `0.415`
  that endpoint cannot reach `0.7`, and a clearly one-sided pair would never
  be asked.
- A typed reply converts any of the four to `ignored` at no cost, with no
  event *(my call: no `fell_out` for an ignored break-up)*.
- Accepting `confess`, `partner_up` or `propose` ends the incident under
  way, as accepting `propose` does today.
- The target of an accepted `confess` is decided from the status at answer
  time, under the row lock.
- An action turn leaves a pending ask open, so the status can move while an
  ask waits (her `propose` is pending, then the user's `break_up` is
  accepted). An answer that arrives when the status is no longer one the
  kind is asked from gets `409` and writes nothing; the ask lapses on its
  own *(my call)*. Only the status is checked: a `break_up` asked during a
  cold war that has since ended can still be answered.
- Order within `maybe_ask`: cold-war entry or exit, then `break_up`,
  `propose`, `confess`, `partner_up`, then today's sleep, bath and travel
  checks *(my call)*.

Stage cues, working copy (same register as the existing ones; the owner
reviews them with the rest of the copy):

- `confess`: `（你发现自己喜欢上对方了。你决定郑重地向对方告白，问对方愿不愿意和你在一起，希望对方给你一个答复。）`
- `partner_up`: `（你越来越觉得对方是值得托付的人。你决定郑重地提出和对方结成伙伴，希望对方给你一个答复。）`
- `break_up`: `（这段冷战让你心灰意冷。你决定郑重地向对方提出分手，希望对方给你一个答复。）`
- answers: one accepted and one declined cue per kind, stating the fact only.

### 7.3 Her "sleep together" ask

One trigger, today's (compatible, chemistry at tier 3+, the newest 3 judged
turns warm). What she asks depends on the status when she asks *(owner's
ruling)*:

- `dating`, `engaged`, `fwb`, `situationship`, or `partner` with chemistry
  at tier 4+: kind `sleep_together`, unchanged. It asks for sex and an
  accept places `having_sex_together`.
- otherwise (`none`, `peer`, `ex`, or `partner` with chemistry at tier 3): a
  new kind `fall_asleep_together`. It asks to literally sleep side by side,
  and an accept places `asleep_together`, as the user's
  `fall_asleep_together` does. A decline drops chemistry one tier, as
  `sleep_together` *(my call)*.

*(My call)* on the wire names: `sleep_together` keeps its current meaning
so stored rows and existing clients stay correct, and the literal ask
mirrors the user action of the same name.

The once-only rule applies to each kind on its own *(my call)*: a pair that
was asked to fall asleep together can still be asked for sex once its status
qualifies.

No warm ask while a cold war is on: `confess`, `partner_up`, `propose`,
`sleep_together`, `fall_asleep_together`, `bathe_together`,
`travel_together`.

### 7.4 The cold-war message

When a streak starts a cold war she says so in one proactive message, through
the same path as an ask, with no choice row *(owner's ruling)*:
`metadata.proactive = 'cold_war'`, cue
`（你对对方的耐心被磨光了。你决定不再给对方好脸色，告诉对方你现在不想好好说话。）`.
The status change is committed first; a failed or blank generation leaves it
in place and she says nothing. A cold war started by a refused break-up has
no extra message: her refusal is that turn's reply. Ending a cold war writes
no message.

### 7.5 `ask_for_space`, retired

No new row of this kind is written. Stored rows stay readable (history,
receipts, the story director). A row still pending at deploy lapses, or is
converted by a typed reply at no cost; the answer endpoint returns `409` for
the kind. Separations already under way run out; `taking_space` and
`fell_out` stay as event kinds for stored rows. `tier_gap` and its
constraint stay.

## 8. The user's actions

- **`propose`** (existing). New rule before the judge: unless the status is
  `dating`, `fwb` or `situationship`, it is refused, nothing drops, and the
  refusal is not a rejection of the user (same shape as `world_ruled_out`).
  From those three the judge decides as today; an accept sets `engaged` in
  the turn's affinity transaction.
- **`break_up`** (new `UserAction::BreakUp`, no fields). Before the judge:
  refused with no effect unless the status is breakable. Otherwise the judge
  decides: an accept sets `ex` and ends the cold war; a refusal keeps the
  status and starts a cold war if none is on. Nothing drops either way
  (`refusal_line` is `None`). It is answered in the `emotional` tone when
  the deployment has it on. It is not a together action and places no event.

Inside a date the same actions act on the inviting session's affinity row,
where date turns already write.

## 9. Where it shows

### 9.1 `[relationship]`

Up to three lines, each present only when it has something to say:

```
[relationship]
你是用户的未婚妻
论感情，你们现在是知心爱人
你们正在冷战
```

- **Status line**, when the status is not `none`: `你是用户的{noun}`. Nouns:
  `dating` 女朋友 / 男朋友, `engaged` 未婚妻 / 未婚夫, by the persona's
  `gender` (伴侣 / 订婚对象 for any other value); `partner` 伙伴; `peer`
  同事; `ex` 前任. `fwb` and `situationship` have no settled Chinese wording
  yet; working copy 伙伴，也是情人 and 同事，私下在交往.
- **Composed line**, when there is a composed relation (for an incompatible
  pair, the chemistry-1 name of §5). With no status line it is the identity
  itself: `你是用户的{composed}`.
- **Cold-war line** while it lasts.
- With status `none`, no composed relation and no cold war, the block is
  today's quadrant noun, under today's scope gate.

The status, composed and cold-war lines are not gated by individual axes of
`affinity_scope`: they are settled conclusions about the pair, not an axis
steering the reply. They are omitted when there is no affinity or the scope
is `none()` *(my call)*.

### 9.2 PDE judge

The judge's context gains one line with the status noun and whether a cold
war is on, so its accept or refuse decisions agree with the relationship:
`[关系] 状态={zh}；冷战中={是|否}`, `engaged` reading 已订婚. At `none` the
line is `[关系] 冷战中=是` during a cold war and absent otherwise, since
`none` is every session at deploy and says nothing about the pair. It does
not get the composed relation.

### 9.3 API

`AffinitySnapshot` (every route that returns one) gains:

- `relationship_status`: one of the eight keys.
- `cold_war`: boolean.
- `composed_relation`: the key of the real tiers' cell, or `null` at
  `(1, 1)`; compatibility is not applied here (§5).

Life-choice DTOs carry the new kind strings; options stay `accept` and
`decline`. The history routes' `occasion` can now be `cold_war`. Declare
all of it in the `utoipa` contract first.

### 9.4 Story director

The evidence gains the current status and cold-war flag as an
engine-recorded fact, under the rule that already covers `life_events` and
`life_choices`: `user_relationship` must not contradict it. The
`life_choices` evidence list covers the four status kinds beside the stored
`ask_for_space` rows *(my call)*.

## 10. Deployment

- Migrate before the new image starts.
- Every existing session reads `none`, no cold war. Nothing is backfilled,
  including proposals accepted before.
- A downstream client that does not know the new kinds renders her ask as an
  ordinary proactive message and the user answers in words: the existing
  degrade path. A cold war needs no client support to take effect.
- **Release order.** A typed "yes" to her confession is `ignored`, moves
  nothing and starts the 14-day wait; and the user's `propose`, which works
  today on the judge alone, is refused until the pair is `dating`, `fwb` or
  `situationship`. So until a client can accept `confess` through the
  endpoint, no session can get engaged. The client's support for the new
  asks ships before this image is deployed.
- Rollback: the old image ignores the two columns. Rows of the new kinds
  would be unreadable to it at the answer endpoint (`stored life choice kind
  unreadable`), so answer or let lapse any pending new-kind ask before
  rolling back.
- The web client needs UI for the new asks, the `break_up` action and the
  three snapshot fields: one issue on the web repo, not part of this plan.

## 11. Left to the builder

- Function and module layout; whether the status enum lives beside the
  labels in `core::affinity` or in its own module.
- How `last_judged_endpoints` grows to return the patience level.
- The exact shape of the `relationship` event's `context`, within §6.4.
- Wording of the answered cues, in the register of the existing ones.
- Test plan. Pure functions (composed table, leading line, transition table,
  cold-war enter and exit) get unit tests; the triggers and the answer route
  follow the existing `life_choice.rs` and `life_choices.rs` test scenes.
- Docs (`affinity-model`, `life-events-needs`, `api-reference`,
  `model-config`, a migrating guide) go in their own PR after the code PR.

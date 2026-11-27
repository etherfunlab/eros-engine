# Dancing and massage together, and the passion attribute — Design

- **Date:** 2026-11-28
- **Status:** Approved
- **Type:** Engine change. No migration. Wire changes: one action type
  removed (`ask_for_dance`), four added (`ask_massage`, `massage_together`,
  `massage_naked_together`, `dance_naked_together`), three event kinds
  added (placed, never drawn). Behaviour changes: five gestures gain a
  romance gate and an `erotic` reply when accepted.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-27-together-action-groups-design.md` (the groups,
  what a naturism action is), `2026-11-27-passion-group-design.md` (the
  passion group's gate), `2026-11-27-erotic-tone-design.md` (the fixed
  moments, the common conditions),
  `2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-custom-together-design.md`
  (`dance_together`), `2026-11-12-emotional-action-replies-design.md` (the
  listed actions).

## 1. Intent

Three things:

- Asking her to dance moves into the together class. The gesture
  `ask_for_dance` goes; dancing together stays in the general group, where
  it already is, and gains a naturism twin.
- Massage arrives in three forms: a gesture asking her for one, giving her
  one (general group), and one given to each other (naturism group).
- Gestures gain what the together class has as a group: a **passion
  attribute**. A together action belongs to a group; a gesture carries an
  attribute. A gesture with the attribute is gated on romance as the
  passion group is, and answered in the `erotic` tone when she accepts.

## 2. Goals, non-goals, anti-goals

**Goals.** §4–§7.

**Non-goals.** A change to `ask_dancing` (she dances for the user) or to
`dance_together`. A bypass for the attribute's gate. The attribute on
`strip`, `kiss`, `hug_from_behind` or any gesture not listed in §6. A
massage that takes her offline, costs or gives a need, or leaves a
lingering effect. Group chat.

**Anti-goals.** A second list of the passion gestures anywhere but the one
property in `eros-engine-core`. An explicit word in the repository: the
markers and labels of §4 and §5 are the whole wording.

## 3. Approaches

**The attribute.** Three ways: (a) one property on `UserAction`, a fixed
list in `eros-engine-core` beside `speaks_emotionally` and
`refusal_lines`; (b) a non-together "group" enum mirroring
`TogetherGroup`; (c) a per-action table carried in config. (a) is taken:
the attribute answers one yes-or-no question, and every other action
property in the engine is written that way. (b) builds a second taxonomy
for one member. (c) lets a deployment move a gate, which no other action
rule allows.

**`ask_for_dance`.** The owner weighed deleting it, keeping it as an alias
of `dance_together`, and leaving both; and picked deletion.

**What the attribute does.** Offered: the `erotic` reply on accept, the
romance gate, a label with no behaviour. The owner picked the first two.

**Which gestures carry it.** Eight were offered (§6); the owner picked
five.

## 4. Dancing

### 4.1 `ask_for_dance` is removed

The action type is no longer accepted: a request that sends it answers
`422`, as any unknown type.

A stored user row whose `metadata.action.type` is `ask_for_dance` reads as
`dance_together`: in the history window, the judge's transcript and the
history routes' action view. It was an invitation to dance together, and
that is what the row keeps saying. Nothing is placed for it, as nothing is
placed by reading any stored action. *(Engine's call. The alternative, a
row that reads as no action, leaves an action turn with no text and no
marker in the history sent to the model.)*

`ask_dancing` (想看你跳支舞) and `dance_together` (想和你一起跳舞) are
unchanged.

### 4.2 `dance_naked_together`

| | |
|---|---|
| action `type` | `dance_naked_together` |
| marker | `想和你一起裸舞` |
| places | `naked_dancing_together` |
| label | `和对方一起裸舞` |
| group | naturism |
| lasts | 1 h |
| clothed counterpart | `dancing_together` |
| fits | both worlds |

It is a naturism action in every respect the groups spec gives one: no
romance gate; the judge decides; her reply is `emotional`, accepted or
refused; a refusal drops chemistry (its counterpart is not one of the
outdoor five, so not bond); an accept places the kind, ends the incident
under way and comes with her picture; she wears nothing in that picture;
the event ends early when chemistry falls. Not wet: no roll on her period.

## 5. Massage

| | `ask_massage` | `massage_together` | `massage_naked_together` |
|---|---|---|---|
| what | she massages the user | the user massages her | each massages the other |
| class | gesture, passion attribute (§6) | together, general group | together, naturism group |
| marker | `想让你帮忙按摩` | `想给你按摩` | `想和你脱了衣服互相按摩` |
| places | nothing | `massaging_together` | `naked_massaging_together` |
| label | — | `让对方给自己按摩` | `和对方脱了衣服互相按摩` |
| lasts | — | 30 min | 30 min |
| picture on accept | no | yes | yes |
| refusal drops | chemistry | chemistry | chemistry |

- `ask_massage` behaves as `touch` does apart from §6: no payload, the
  judge decides, nothing is placed, no picture. It is not one of the
  actions answered in the `emotional` tone.
- `massage_together` behaves as `dance_together` does: no romance gate,
  not answered emotionally, placed on accept, picture turn.
- `massage_naked_together` is a naturism action as §4.2 describes;
  its clothed counterpart is `massaging_together`.
- All three fit both worlds and are not ruled out for a machine persona.
- The three new kinds (`naked_dancing_together`, `massaging_together`,
  `naked_massaging_together`) are `SPECIAL` as every placed kind is:
  weight 0, no cooldown, `Neutral` place, never told, not offline.
  `massaging_together` reaches the story director as the other general
  kinds do; the two naked kinds are kept out of it, as every naturism kind
  is.
- A marker is read after 「对方」 by the judge and the reply
  (`对方想让你帮忙按摩`), so none of them says 我.

*(Owner's rulings: the three massages and where each sits. Engine's
calls: every name, marker, label and duration; who massages whom in each;
no picture for the gesture; `ask_massage` outside the emotional list.)*

## 6. The passion attribute

`UserAction::is_passion(&self) -> bool`, true for exactly these five
gestures and false for every other action, the together actions included
(they have groups):

| action | marker |
|---|---|
| `french_kiss` | 凑过来想和你舌吻 |
| `tease` | 伸手想挑逗你 |
| `lick` | 想舔你 |
| `touch` | 伸手想摸你 |
| `ask_massage` | 想让你帮忙按摩 |

`strip`, `kiss` and `hug_from_behind` were offered and left out.

### 6.1 The gate

A gesture with the attribute, from a pair that is not romance-compatible
(`scope::romance_compatible` of the turn's affinity scope), is refused
before the judge. There is no bypass: these actions carry no payload, and
`ignore_orientation` stays a field of `sex_together` alone.

The forced refusal is an ordinary refusal: it costs what the action's
refusal costs (chemistry), the reply is written as a refused one, and the
user row stores the answer. `strip` keeps its own gate, which behaves the
same way: with a judge configured the judge is still called and is told
the outcome, and its line for these five ends `，这次你不会答应`, the suffix
the together actions use.

### 6.2 The `erotic` reply

Her reply to an **accepted** gesture with the attribute is a fixed moment
of the `erotic` tone, beside the accepted `sex_together`: one roll against
`tones.erotic`, under the common conditions of the tone's spec (§4.1
there: a compatible pair, no cold war, the virgin's floor at chemistry
tier 4). Won, the reply is `erotic`. Lost, or a condition failing, or the
gesture refused, the reply is what it is today:

| gesture | today |
|---|---|
| `french_kiss`, `tease`, `touch` | `emotional` under `tones.emotional`, accepted or refused |
| `lick`, `ask_massage` | no tone |

A lost roll on `lick` or `ask_massage` records `metadata.tone = "default"`,
as every lost roll on a message with no tone of its own does.

The naturism actions of §4.2 and §5 are not fixed moments: they stay
`emotional`, as every naturism action does.

*(Owner's rulings: the attribute, its two effects, the five gestures.
Engine's calls: no bypass; the forced refusal costs chemistry;
`ask_massage` and `lick` gain no `emotional` reply.)*

## 7. What changes for a live user

- `ask_for_dance` answers `422`. A client must stop sending it before the
  engine is upgraded; `dance_together` is its replacement.
- **An incompatible pair can no longer `french_kiss`, `tease`, `lick` or
  `touch` with a chance of a yes.** Each was the judge's call; each is now
  refused, and each refusal drops chemistry a tier, where there is a tier
  to drop.
- With `tones.erotic` set, an accepted one of the five reads alluring
  between a compatible pair.
- Four new actions and three new event kinds.

## 8. Tests

- Core: `is_passion` is true for exactly the five; the round-trip of the
  four new types; `ask_for_dance` in stored form deserialises as
  `DanceTogether`; `together_kind`, `together_group`, `clothed_counterpart`
  and `refusal_lines` of the three together actions; the three kinds are
  `SPECIAL`, last what §4.2 and §5 say and fit both worlds; the gate
  refuses each of the five for an incompatible pair and none for a
  compatible one, and refuses nothing else that it did not refuse before.
- Server DTO: `ask_for_dance` in a request is a `422`; the four new types
  are accepted; the history view of a stored `ask_for_dance` row returns
  `dance_together`.
- Chat turn: a gated gesture is refused whatever the judge or the rule
  engine would have answered (as `strip`'s gate is tested) and drops
  chemistry; an accepted one of each of the five is `erotic` at
  `tones.erotic = 1.0` and, with the key absent, exactly what it is today;
  a refused one is never `erotic`; `massage_together` and the two naked
  actions place their kinds, come with a picture and keep their tones.
- Pictures: the two naked kinds read 什么都没穿; the labels of §4.2 and §5.
- Markers and labels verbatim.

## 9. Order of work

One code pull request into `dev`, then a docs pull request: the action
lists and the together-actions tables in `docs/life-events-needs.md`
(+ `.zh`) and `docs/api-reference.md` (+ `.zh`), the fixed moments in
`docs/model-config.md` (+ `.zh`), a migrating note. `openapi.json` is
regenerated (the action enum changes).

The passion attribute is documented in the docs and the migrating guide
only, as the passion group and the `erotic` tone are; GitHub release notes
do not list it. *(Engine's call, following the owner's rulings on those
two.)*

A web-side issue lists the removed type and its replacement, the four new
types, the three kinds, the gate on the five gestures and the deploy
order (the client first).

## 10. Decisions

Ruled by the owner:

- `ask_for_dance` is deleted; dancing together is the general-group action
  that exists, plus a naturism `dance_naked_together`.
- Gestures have a passion attribute; together actions have groups.
- The attribute gates on romance and makes an accepted reply `erotic`.
- It is carried by `french_kiss`, `tease`, `lick`, `touch` and the new
  `ask_massage`; not by `strip`, `kiss` or `hug_from_behind`.
- Three massages: a gesture asking for one, giving one (general), each
  other (naturism).

Calls made in this document, open to correction:

- A stored `ask_for_dance` row reads as `dance_together`, not as no action.
- Every name, marker, label and duration of §4.2 and §5; who massages whom.
- 30 minutes for a massage, 1 hour for the naked dance.
- No bypass for the attribute's gate; its forced refusal costs chemistry;
  the judge's line for it ends `，这次你不会答应`.
- The two naked kinds stay out of the story director; `massaging_together`
  reaches it.
- `ask_massage` and `lick` are not added to the `emotional` list.
- `ask_massage` shows no picture; the two together massages do.
- Docs and migrating guide only; not in the release notes.

## 11. Left to the builder

- Where the attribute's gate sits beside `strip_ruled_out` and
  `together_ruled_out`, and whether the three merge.
- How the stored-form alias is written so that the request DTO still
  refuses `ask_for_dance`.
- What a queued turn that carries `ask_for_dance` does on the first image
  that no longer knows it.
- Whether the erotic fixed-moment check reads the passion group and the
  attribute through one helper.

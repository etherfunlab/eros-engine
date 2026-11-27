# Diving, skiing and surfing together — Design

- **Date:** 2026-11-28
- **Status:** Approved
- **Type:** Engine change. No migration. Wire change, additive: six
  together action types and the six event kinds they place (placed, never
  drawn). No new route, frame, config key or card key.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-27-together-action-groups-design.md` (the groups,
  the wet kinds, a nude kind and its clothed counterpart),
  `2026-11-27-swim-outfit-ladder-and-fish-together-design.md` (the swimsuit
  ladder, the outdoor actions),
  `2026-11-28-dance-styles-design.md` §7 (the `naturism` key).

## 1. Intent

Three more things to do together, each with its naturism twin. Each
action is an existing one under another name: it has every rule of the
action it mirrors.

## 2. The six

| action | event | words (action / event) | group | mirrors |
|---|---|---|---|---|
| `scuba_dive_together` | `scuba_diving_together` | 想和你一起水肺潜水 / 和对方一起水肺潜水 | general | `swim_together` |
| `surf_together` | `surfing_together` | 想和你一起冲浪 / 和对方一起冲浪 | general | `swim_together` |
| `ski_together` | `skiing_together` | 想和你一起滑雪 / 和对方一起滑雪 | general | `hike_together` |
| `free_dive_together` | `free_diving_together` | 想和你一起裸体自由潜水 / 和对方一起裸体自由潜水 | naturism | `skinny_dip_together` |
| `surf_naked_together` | `naked_surfing_together` | 想和你一起裸体冲浪 / 和对方一起裸体冲浪 | naturism | `skinny_dip_together` |
| `ski_naked_together` | `naked_skiing_together` | 想和你一起裸体滑雪 / 和对方一起裸体滑雪 | naturism | `hike_naked_together` |

What mirroring gives each:

- **A dive and a surf** (as a swim): one hour; a water kind, so her period
  rolls on it and she holds back her bath and sleep asks while it lasts;
  in a picture she wears the swimsuit of the ladder; a refusal drops
  chemistry.
- **A ski** (as a hike): two to three hours; an outdoor action, so a
  refusal drops bond; no clothing rule.
- **The three naked ones** (as a skinny dip or a naked hike): the naturism
  group, so no romance gate, an `emotional` reply, nothing on in a picture,
  ended early by a chemistry drop, and read by the `naturism` key. The
  naked dive and surf are water kinds. Each is the naked version of its
  clothed twin: `free_dive_together` of `scuba_dive_together`,
  `surf_naked_together` of `surf_together`, `ski_naked_together` of
  `ski_together`, so a refused naked ski drops bond and chemistry.

No payload on any of them. Both worlds have all six: a persona in a
fantasy world is asked like any other, and the judge decides. *(Owner's
rulings: the six, scuba dressed as a swim, the naked dive in the naturism
group, no world rule on the scuba dive. Engine's calls: the names, the
words, a surf as a water action dressed as a swim, a ski as a hike.)*

## 3. Tests

Each kind and each action is checked against the one it mirrors, property
by property, so a rule added to the model later is a rule the mirror must
get too.

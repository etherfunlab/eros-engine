# Migrating to v2.8.4: together-action groups, five together actions, the swimsuit ladder and the passion group

**Applies to:**
- clients that keep their own catalog of action types or event kinds, or that match one of them exhaustively;
- clients that offer `sex_together`, read stored actions from the history routes, or read an event's `message_id`;
- clients that show tier changes, or that schedule anything on an event's `ends_at`;
- clients that hide some together actions from pairs that are not romance-compatible;
- deployments that have a custom composer `filter_prompt` or a custom judge prompt, or that read the composer's or the affinity event's audit;
- crate users who match on `EventKind` or `UserAction`, build or match `UserAction::SexTogether`, call `UserAction::refusal_line` or `UserAction::romance_gated`, rely on the length of `EventKind::SPECIAL`, `ENDS_ON_CHEMISTRY_DROP` or `HOLDS_BACK_INTIMATE_ASKS`, or call `AffinityRepo::persist_with_event`.

No migration and no new setting. The wire change is additive (five action types, five event kinds, five optional fields on `sex_together`, one key in the composer's audit `inputs`, one value of the affinity audit's `tier_drop`), but several behaviours change against v2.8.3: which together actions are refused before the judge, what a refusal costs, which actions her period rolls, which events hold her asks back or end on a chemistry drop, and what her picture wears.
**Design:** [`2026-11-27-swim-outfit-ladder-and-fish-together-design.md`](../superpowers/specs/2026-11-27-swim-outfit-ladder-and-fish-together-design.md), [`2026-11-27-together-action-groups-design.md`](../superpowers/specs/2026-11-27-together-action-groups-design.md), [`2026-11-27-passion-group-design.md`](../superpowers/specs/2026-11-27-passion-group-design.md)

The rules are in [together actions](../life-events-needs.md#showering-bathing-and-the-sauna-together), [together-action groups](../life-events-needs.md#together-action-groups), [what a refusal costs](../life-events-needs.md#what-a-refusal-costs), [the passion group](../life-events-needs.md#the-passion-group), [what she wears in the water](../life-events-needs.md#what-she-wears-in-the-water), [when chemistry falls, the intimate event ends](../life-events-needs.md#when-chemistry-falls-the-intimate-event-ends), [the composer](../model-config.md#taskschat_image_prompt_compose--image-prompt-composer-required-for-image-turns) and [the send route](../api-reference.md#post-compchatsession_idmessagestream).

## What users will notice

- **The user can ask her to go fishing, or to ride a horse, do yoga, hike or soak in a hot spring naked together.** None of the five carries a field, and each fits both worlds:

  | action | marker | kind | label | category | lasts |
  |---|---|---|---|---|---|
  | `fish_together` | 想和你一起钓鱼 | `fishing_together` | 和对方一起钓鱼 | `mid` | 2–3 h |
  | `ride_horse_naked_together` | 想和你一起裸体骑马 | `naked_horse_riding_together` | 和对方一起裸体骑马 | `short` | 1 h |
  | `yoga_naked_together` | 想和你一起裸体做瑜伽 | `naked_yoga_together` | 和对方一起裸体做瑜伽 | `short` | 1 h |
  | `hike_naked_together` | 想和你一起裸体爬山 | `naked_hiking_together` | 和对方一起裸体爬山 | `mid` | 2–3 h |
  | `hot_spring_naked_together` | 想和你一起裸体泡温泉 | `naked_hot_spring_together` | 和对方一起裸体泡温泉 | `short` | 1 h |

  The judge decides each. An accepted one places its kind in the write that stores the answer, ends the incident under way (inside a date nothing is placed) and comes with her picture when the turn carries `image` and the deployment has the composer; the kind is never told, never drawn and never offline. `fish_together` behaves as `hike_together` does and decides nothing about a catch; it reaches the story director. The four naked ones are naturism actions (below).
- **Every together action is in a group.** General (ordinary things done together, `custom_together` included), naturism (the same done naked, or naked by nature: `shower_together`, `bath_together`, `streak_together`, `skinny_dip_together` and the four naked actions above), thrill (`adventure_together`, `commit_crime_together`, `play_with_fire_together`, `fight_together`) and passion (`sex_together` alone). Independently, seven kinds happen in water: `swimming_together`, `hot_spring_together`, `sauna_together`, `showering_together`, `bathing_together`, `skinny_dipping_together` and `naked_hot_spring_together`. Six naturism kinds are the naked counterpart of a general one: streaking of jogging, the naked hike of hiking, skinny-dipping of swimming, the naked hot spring of the hot spring, naked yoga of yoga, the naked ride of horse riding. What follows is derived from these three properties; the [groups section](../life-events-needs.md#together-action-groups) has the table.
- **Naturism is no longer refused before the judge for an incompatible pair.** In v2.8.3 a `shower_together`, `bath_together`, `streak_together` or `skinny_dip_together` from a pair whose `affinity_scope` is not romance-compatible was refused before the judge; now the judge decides them for any pair, since naturism is not a romantic act, and it decides the four new naked actions the same way. Without the LLM judge, or when its call fails, the rule engine accepts, so on that path an incompatible pair's naturism action is accepted. `sex_together` is still refused before the judge for such a pair, unless it says `ignore_orientation: true`. `strip`, a machine's `toilet_together`, the world rules and her own asks' compatibility are unchanged.
- **Some refusals cost other lines.** Against v2.8.3:

  | refused | v2.8.3 | now |
  |---|---|---|
  | `jog_together`, `hike_together`, `cycle_together`, `ride_horse_together` | chemistry | bond |
  | `fish_together` (new) | — | bond |
  | `adventure_together`, `play_with_fire_together`, `fight_together` | chemistry | bond, as `commit_crime_together` already was |
  | `streak_together` | chemistry | bond and chemistry |
  | `hike_naked_together`, `ride_horse_naked_together` (new) | — | bond and chemistry |
  | `sex_together` with a `name` (new field) | — | bond and chemistry |
  | `yoga_naked_together`, `hot_spring_naked_together` (new) | — | chemistry |

  Every other together action costs chemistry, as before; a refusal her world or her period rules still costs nothing, and the gate's refusal of a `sex_together` costs what the judge's would. Each line drops by the one-tier drop of any refused action. A refusal that drops bond alone no longer ends an intimate event under way, since only a chemistry drop does.
- **Her period rolls the actions in water.** During her period, `sex_together` and every together action whose kind happens in water are refused two times in three before the judge, at no cost: `swim_together`, `hot_spring_together`, `sauna_together`, `shower_together`, `bath_together`, `skinny_dip_together`, `hot_spring_naked_together`. `shower_together` and `hot_spring_naked_together` join the v2.8.3 list; the dry naked actions are not rolled.
- **She holds back her bath and bed asks in any water.** While a kind in water or `having_sex_together` is the foreground she asks neither `bathe_together` nor `sleep_together`. `swimming_together`, `skinny_dipping_together` and `naked_hot_spring_together` join the v2.8.3 list of `bathing_together`, `showering_together`, `hot_spring_together`, `sauna_together` and `having_sex_together`.
- **A chemistry drop ends every naturism and passion event.** `naked_horse_riding_together`, `naked_yoga_together`, `naked_hiking_together` and `naked_hot_spring_together` join `showering_together`, `bathing_together`, `having_sex_together`, `streaking_together` and `skinny_dipping_together`. The rule is otherwise as in v2.8.3.
- **Her reply to every naturism and passion action is `emotional`** when the deployment has `tones.emotional` above 0: the four new naked actions join `shower_together`, `bath_together`, `streak_together`, `skinny_dip_together` and `sex_together`. `fish_together`'s is not.
- **The story director leaves out every naturism and passion kind**, the four new naked kinds included; `fishing_together` reaches it.
- **Her picture's clothing follows the foreground.** The engine writes the composer's `[此刻穿着]`, over the judge's `clothing` and a named `outfit`, for more kinds than v2.8.3's hot spring and sauna:

  | foreground | v2.8.3 | now |
  |---|---|---|
  | any naturism kind (shower, bath, streak, skinny dip, the naked ride, yoga, hike, hot spring) | the judge's clothing or the named outfit, the state winning | 什么都没穿 |
  | `having_sex_together` | the judge's clothing or the named outfit, the state winning | 什么都没穿; on a 1:1 chat picture, her side of the act (below) |
  | `swimming_together` | the judge's clothing or the named outfit | the swimsuit ladder |
  | `hot_spring_together` | 什么都没穿 for a compatible pair, 身上只裹着一条浴巾 otherwise | the swimsuit ladder; a persona whose gender is neither `female` nor `male` wears the sauna's towel |
  | `sauna_together` | 什么都没穿 for a compatible pair, 身上只裹着一条浴巾 otherwise | 身上只松松地裹着一条浴巾 for a compatible pair, 身上只裹着一条浴巾 otherwise |

  The swimsuit ladder reads `art_metadata.gender`, the pair's romance compatibility, her virginity and the chemistry tier. A female persona of a compatible pair wears, by tier: `Tankini top` or `Bralette bikini` (tier 1, a fair coin per picture), `Triangle bikini` (tier 2), `Bandeau bikini` (tier 3), `Halter bikini` (tier 4), `String bikini` (tier 5 below 0.95); a virgin wears `navy one-piece swimsuit with name patch and a front drain flap` through tier 3. A male persona of a compatible pair wears `rash guard with board shorts`, `board shorts`, `swim trunks`, `square-cut swim trunks`, `swim briefs`. From chemistry 0.95 either has nothing on (什么都没穿). An incompatible pair: `plain one-piece swimsuit` for a female persona, `board shorts` for a male one. Any other gender, or none: a swim is left to the judge's clothing. A virgin is `is_virgin` with no `having_sex`, `just_have_sex` or `having_sex_together` row ever; the chemistry is the one the picture's path holds (a chat turn's after decay and before its deltas, a group speaker's seat, an event message's decayed affinity). It applies to every picture the chat pipeline draws while the kind is the foreground (chat, group, event message), and only to the composer's slot: her reply's clothing line, the judge's `clothing` and her wardrobe are untouched.
- **The user can name the act, the leader and the clothing of a `sex_together`.** All optional: `name` (or `name_base64`), `active` (`"user"` or `"persona"`), `remain_clothing` (`false`, `true`, or per side `full_naked`, `half_naked`, `{"custom": …}` or `{"custom_base64": …}`) and `ignore_orientation`. The engine ships no act names; the client passes its own (an example such as `"姿势甲"` is a placeholder). With a name, the marker the judge, the reply and the history read is 想和你上床：「{name}」, and a 1:1 chat picture's `[此刻状态]` reads 和对方上床：「{name}」. On every 1:1 chat picture while a `having_sex_together` lasts, `[此刻穿着]` is her side (什么都没穿, 半裸, the custom text, or 穿着衣服 for `true`), and a `[亲密]` section after `[对方]` names who leads (角色 or 对方) and what the user wears. Without `active`, a male `user_gender` with a female persona has the user lead, a female one with a male persona has the persona lead, and the user leads otherwise. A plain `sex_together`'s 1:1 picture reads 什么都没穿 and carries the `[亲密]` section, both naked and the default leader. Group and event pictures read 什么都没穿, the plain label and no `[亲密]`.

## Deployments

Nothing to run: no migration, no new setting. Deploy the image as for any release.

- **A custom composer `filter_prompt`.** `[此刻穿着]` is engine-written for the kinds in the table above; the garment names of the swimsuit ladder arrive in English. On a 1:1 chat picture of a passion act the payload gains a `[亲密]` section right after `[对方]` (or after `[此刻状态]` when there is no `[对方]`):

  ```
  [亲密]
  主动的一方：对方
  对方穿着：什么都没穿
  ```

  The built-in prompt gains one line for it: the character wears what `[此刻穿着]` says and the partner what `[亲密]` says, over what `[此刻状态]` would imply, and the composition follows who leads. A custom prompt that lets the state win over `[此刻穿着]` should add such a line. `[此刻状态]` can carry the client's act name in 「」.
- **A custom judge prompt.** The `[用户动作]` line carries the new markers: `[用户动作] 对方想和你一起钓鱼`, `对方想和你一起裸体骑马`, `对方想和你一起裸体做瑜伽`, `对方想和你一起裸体爬山`, `对方想和你一起裸体泡温泉`, and for a named act `对方想和你上床：「{name}」`, the client's text quoted with 「」. A shower, a bath, a streak or a skinny dip from an incompatible pair no longer ends the line with `，这次你不会答应`; a gated `sex_together` still does, and so does a period roll's refusal, now also of a `shower_together` or a `hot_spring_naked_together`. A prompt that treats actions generically needs nothing.
- **The `emotional` tone** answers four more actions; a deployment with `tones.emotional` at 0 sees no change.
- **Audit.** On the composer's rows in `engine.chat_images_events`, `inputs` gains `intimacy`: the `[亲密]` section as sent, its header line included, or `""` without one (always `""` on the standalone endpoint, a group turn and an event message). `inputs.clothing` carries the engine's clothing for every kind in the table above. On `engine.companion_affinity_events`, `context.tier_drop` is `bond`, `chemistry`, or `both` for a two-line drop. A reader that expects a fixed key set or the two old values should accept them.

### Rollback

These changes need no database change; roll the image back. On the v2.8.3 image:
- an event of one of the five new kinds reads as no event, as any kind the build does not know;
- an action of a new type on a stored row reads as no action, and a request that sends one answers `422`;
- a `sex_together` that carries the new fields is taken as the plain one: the fields are ignored, `ignore_orientation` included, so an incompatible pair is refused before the judge again;
- the history routes return a stored `sex_together`'s act as plain `name` and drop its other fields;
- a `having_sex_together` row the new image placed keeps the user's message as its `message_id`, which the previous image reads as her telling: a draw asked under its `event_key` returns that user row as `message`;
- the v2.8.3 gates, refusal lines, period roll, held asks, chemistry-drop list, tone list and picture clothing apply again, and there is no `[亲密]` section and no `intimacy` key.

## Clients

1. **Add the five action types** if your client offers them. None carries a field. The answer arrives as for any action: the `action_response` frame, then `response` on the stored row in both history routes.
2. **Offer the `sex_together` fields if you want them.** All optional; the name is your own, plain (`name`) or base64 of UTF-8 text (`name_base64`), 1 to 100 characters after trimming and decoding, never both; custom clothing follows the same rules. `name` with `name_base64`, base64 that does not decode, bytes that are not UTF-8, a value out of bounds or an unknown key in the `remain_clothing` object is a `422`. `ignore_orientation: true` is for debugging: it skips the romance gate and changes no cost. A request without the fields is the plain `sex_together` and serialises as `{"type": "sex_together"}`. The async send route keeps the request as sent in its queue row, the fields included, so a queued or revived turn runs as asked; no route reads that copy back.
3. **Read stored acts as base64.** Both history routes return a `sex_together`'s act as `name_base64` and custom clothing as `custom_base64`, whatever the request sent; they never return the act in plain text. The event view carries no act: a `having_sex_together`'s `text` is `null`.
4. **Expect a picture on an accepted one**, as on any accepted together action: with `image` in the request and the composer configured, the frames run `action_response → meta(reply_text_image) → delta* → done → image_request → final`. A refusal, and a deployment without the composer, are text only, except that `image.force = true` still makes a refused turn `reply_image`, its picture following the refusal.
5. **Add the five event kinds to your catalog** before or with the upgrade: `fishing_together`, `naked_horse_riding_together`, `naked_yoga_together`, `naked_hiking_together` and `naked_hot_spring_together` (labels and lengths in the table above). Each is placed by its accepted action and never drawn. They appear as `event` on `GET /v2/comp/session/{session_id}/effects` while they last, with `"offline": false`. A client that drops kinds it does not know shows nothing for them.
6. **Keep the new kinds out of `kinds`.** Naming one in a `kinds` filter is a `422`, as for `jogging_together`. A `disallow` list needs nothing.
7. **Adult themes.** `naked_horse_riding_together`, `naked_yoga_together`, `naked_hiking_together` and `naked_hot_spring_together` carry them, as `streaking_together` and `skinny_dipping_together` do, and only their accepted actions place them. A caller that serves adult themes only to some users does not offer those four actions to the others.
8. **A user-placed `having_sex_together`'s `message_id` is the user's message that asked**, as a `custom_together`'s is; a row her own `sleep_together` placed has none. A client that fetches a told message or an image request by an event's `message_id` skips such a row. A draw on the events route asked under its `event_key` returns the event with `message: null`.
9. **Gates you mirror.** A client that hid `shower_together`, `bath_together`, `streak_together` or `skinny_dip_together` from incompatible pairs because the engine always refused them can offer them now: the judge decides. Keep `sex_together` gated, or send `ignore_orientation: true` only for debugging.
10. **Tier changes.** A client that shows which line a refusal moved sees bond for the outdoor and thrill actions and both lines for a streak, a naked hike, a naked ride and a named act (table above).
11. **Re-read events after a turn.** The four new naked kinds can end before the `ends_at` you first read, as the other naturism kinds and `having_sex_together` can. A client that scheduled anything on their `ends_at` re-reads `GET …/effects` after every turn and after answering a life choice.
12. **Match enums leniently.** `action.type` gains five values, an event's `kind` five, and `tier_drop` one (`both`).

## Crate users

- `EventKind` gains `FishingTogether`, `NakedHorseRidingTogether`, `NakedYogaTogether`, `NakedHikingTogether` and `NakedHotSpringTogether`. `EventKind::SPECIAL` is now `[EventKind; 37]`, `ENDS_ON_CHEMISTRY_DROP` `[EventKind; 9]` (the naturism kinds and `HavingSexTogether`) and `HOLDS_BACK_INTIMATE_ASKS` `[EventKind; 8]` (the kinds in water and `HavingSexTogether`): a signature or a constant that spells a length changes, and an exhaustive `match` adds the arms. New methods: `together_group() -> Option<TogetherGroup>`, `is_wet()` and `clothed_counterpart() -> Option<EventKind>`; `is_shared()` now derives from `together_group()` and answers as before for the old kinds.
- The new `eros_engine_core::events::TogetherGroup` has `General`, `Nude` (naturism), `Thrill` and `Sex` (passion).
- `UserAction` gains `FishTogether`, `RideHorseNakedTogether`, `YogaNakedTogether`, `HikeNakedTogether` and `HotSpringNakedTogether`. `UserAction::SexTogether` is now a struct variant, `SexTogether { name: Option<String>, active: Option<Party>, remain_clothing: RemainClothing, ignore_orientation: bool }`: a pattern becomes `UserAction::SexTogether { .. }`, and `UserAction::sex_together()` builds the plain one, which still serialises as `{"type":"sex_together"}`. The new `eros_engine_core::types::Party` (`User`, `Persona`), `SideClothing` (`FullNaked`, `HalfNaked`, `Custom(String)`) and `RemainClothing` (`Flag(bool)` or `Sides { active, passive }`, each an `Option<SideClothing>`; the default is `Flag(false)`) carry its fields. The stored shape holds the act and custom clothing in plain text.
- `UserAction::refusal_line() -> Option<AffinityLine>` is replaced by `refusal_lines() -> &'static [AffinityLine]`: empty where `None` was, one line, or `[Bond, Chemistry]` for a two-line action. `UserAction::romance_gated()` is removed; the only compatibility gate left is `sex_together`'s, inside `pde::together_ruled_out`, which keeps its signature. The new `together_group()` returns the group of the kind the action places; `together_text()` also returns a `sex_together`'s name; `speaks_emotionally()` is `true` for every naturism and passion action.
- `pde::period_refuses` keeps its signature and rolls `sex_together` and every together action whose kind `is_wet()`.
- `eros_engine_store::affinity::AffinityRepo::persist_with_event` takes `tier_drop: &[AffinityLine]` instead of `Option<AffinityLine>`: pass `&[]` for none, `&[line]` for one line. With two lines the event row's `context.tier_drop` is `"both"`.
- The new `eros_engine_store::events::points_at_the_user(kind)` answers whether a row a user's action places points at the user's message: `CustomTogether` and `HavingSexTogether`. `EventRepo::place_together_in_tx` stores `user_message_id` as `message_id` for both. The new `EventRepo::action_of(message_id)` returns that message's `metadata.action`. `EventRepo::current` and `by_key` fill `EventRecord.text` for a `custom_together` only.
- `StoryRepo::life_events` leaves out every kind whose `together_group()` is `Nude` or `Sex`, beside `using_toilet_together`, `asleep_together`, `custom_together` and `on_a_date`.

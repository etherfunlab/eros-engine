# Migrating to v2.8.3: more together actions, the water outfit, the chemistry-drop end and `custom_together`

**Applies to:**
- clients that keep their own catalog of action types or event kinds, or that match one of them exhaustively;
- clients that read an event's `message_id`, or that schedule anything on an event's `ends_at`;
- deployments that have a custom composer `filter_prompt` or a custom judge prompt;
- crate users who match on `EventKind` or `UserAction`, rely on the length of `EventKind::SPECIAL`, build `EventRecord` by hand, or call `EventRepo::place_together_in_tx`, `AffinityRepo::persist_with_event` or `AffinityRepo::persist_life_choice_drop{,_in_tx}`.

No migration. Every field on the wire is additive (seven action types, seven event kinds and one field on the event view, `text`), but three behaviours change: a picture in a hot spring or a sauna gets an engine-written outfit, five intimate event kinds can end before their `ends_at`, and she holds back two asks while the pair is in the water or in bed. There is no new setting.
**Design:** [`2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-custom-together-design.md`](../superpowers/specs/2026-11-26-more-together-actions-bath-outfit-chemistry-drop-and-custom-together-design.md)

The rules are in [together actions](../life-events-needs.md#showering-bathing-and-the-sauna-together), [together, in the user's own words](../life-events-needs.md#together-in-the-users-own-words), [what she wears in the water](../life-events-needs.md#what-she-wears-in-the-water), [when chemistry falls, the intimate event ends](../life-events-needs.md#when-chemistry-falls-the-intimate-event-ends), [the composer](../model-config.md#taskschat_image_prompt_compose--image-prompt-composer-required-for-image-turns) and [the send route](../api-reference.md#post-compchatsession_idmessagestream).

## What users will notice

- **The user can ask her to dance, go adventuring, play with fire, sing, swim naked or commit a crime together.** Each action carries no field:

  | action | marker | kind | label | category | lasts |
  |---|---|---|---|---|---|
  | `dance_together` | 想和你一起跳舞 | `dancing_together` | 和对方一起跳舞 | `short` | 1 h |
  | `adventure_together` | 想和你一起去冒险 | `adventuring_together` | 和对方一起冒险 | `mid` | 2–3 h |
  | `play_with_fire_together` | 想和你一起玩火 | `playing_with_fire_together` | 和对方一起玩火 | `short` | 1 h |
  | `sing_together` | 想和你一起唱歌 | `singing_together` | 和对方一起唱歌 | `short` | 1 h |
  | `skinny_dip_together` | 想和你一起裸泳 | `skinny_dipping_together` | 和对方一起裸泳 | `short` | 1 h |
  | `commit_crime_together` | 想和你一起去犯罪 | `committing_crime_together` | 和对方一起犯罪 | `short` | 1 h |

  - Each behaves as `jog_together` does. The judge decides. A refusal drops chemistry one tier (a refused `commit_crime_together` drops bond instead, below) and places nothing; there is no species gate and no world rule. An accepted one puts her in the event, ending the incident under way and leaving a background going; inside a date nothing is placed and the date goes on. None takes her offline, and an accepted one comes with her picture when the turn carries `image` and the deployment has the composer.
  - `play_with_fire_together` is literal fire: a campfire, torches, sparklers.
  - `commit_crime_together` names no crime: what the pair does, the user says in the turn's text, and nothing follows from the event. A refusal drops the bond line, not chemistry; it is the one together action that does.
  - `skinny_dip_together` differs. A pair whose `affinity_scope` is not romance-compatible is refused before the judge, as for `streak_together`, and that refusal drops chemistry. During her period it is rolled as `swim_together` is: two times in three it is refused at no cost. While the deployment has `tones.emotional` above 0 her reply to it is `emotional`, accepted or refused. Its kind carries adult themes, so a caller that serves those only to some users does not send the action.
  - Five of the six kinds reach the story director's events; `skinny_dipping_together` stays out, as `streaking_together` does.
- **The user can ask her to do something together in their own words.** `custom_together` carries a `text` of 1 to 100 characters, validated as `custom`'s is. Nothing rules it out before the judge; a refusal drops chemistry; her reply is not `emotional`. Accepted, it places `custom_together` (`short`, 1 h, both worlds, never told, not offline), with her picture as for the others. Her state block, the judge's `[近期事件]` and the composer's `[此刻状态]` print it as 和对方一起：「{text}」. The user's words live in the message that asked and nowhere else, so the event's `message_id` names that message; if the message is gone, `text` is `null` and the label reads 和对方一起做一件事. The kind stays out of the story director's events.
- **A picture in a hot spring or a sauna dresses her for it.** While `hot_spring_together` or `sauna_together` is what she is doing, the engine writes the composer's `[此刻穿着]` itself, over the judge's `clothing` and a named `outfit`: 什么都没穿 for a romance-compatible pair, 身上只裹着一条浴巾 otherwise. A turn's picture reads the turn's `affinity_scope`, a group speaker's picture her `member_scopes` from the send, an event message's picture the event request's `affinity_scope`; a request that names none reads as not compatible. Her reply's clothing line and her wardrobe are unchanged, and no other kind is covered.
- **A fall of chemistry ends an intimate event.** `showering_together`, `bathing_together`, `having_sex_together`, `streaking_together` and `skinny_dipping_together`, whoever asked for them, end the moment the chemistry tier falls from something the user did after the event started: a refused action, a graded turn that carries chemistry across a tier edge, or her ask that the user declines. The event's `ends_at` becomes that moment; nothing is told and no message is written, and a background and her cycle go on. A fall that time decay alone made, the turn that placed the event, an earlier turn whose write lands late, a bond drop, a turn inside a date and a date's settlement end nothing, and no other together event ends this way.
- **She does not ask for a bath or for bed while the pair is in the water or in bed.** While a foreground `bathing_together`, `showering_together`, `hot_spring_together`, `sauna_together` or `having_sex_together` is under way she asks neither `bathe_together` nor `sleep_together`. The ask only waits: nothing is settled and no cooldown starts, so the next judged turn after the event ends may ask. `propose`, `fall_asleep_together`, `travel_together` and the status asks are asked as before, and an ask already pending stands.

## Deployments

Nothing to run: no migration, no new setting. Deploy the image as for any release.

- **A custom composer `filter_prompt`.** In a hot spring or a sauna `[此刻穿着]` is engine-written (the two wordings above), not the judge's or the request's outfit. The built-in prompt needs nothing; a custom prompt that describes `[此刻穿着]` as the judge's or the request's outfit should say that the engine writes it there. `[此刻状态]` can now hold the user's own words, in 「」, for a `custom_together`.
- **A custom judge prompt.** The judge's `[用户动作]` line carries the new markers: `[用户动作] 对方想和你一起跳舞`, `对方想和你一起去冒险`, `对方想和你一起玩火`, `对方想和你一起唱歌`, `对方想和你一起裸泳` and `对方想和你一起去犯罪`. A `custom_together` reads `[用户动作] 对方想和你一起做一件事：「去海边放烟花」`, the user's text quoted with 「」. A `skinny_dip_together` that the romance gate or the period roll refuses ends the line with `，这次你不会答应`, as a `streak_together` does. A prompt that treats actions generically needs nothing.
- **Audit.** On the composer's rows in `engine.chat_images_events`, `inputs.clothing` is the clothing as sent: the engine's own in a hot spring or a sauna.

### Rollback

These changes need no database change; roll the image back. On the previous image:
- an event of one of the seven new kinds reads as no event, as any kind the build does not know;
- an action of a new type on a stored row reads as no action, and a request that sends one answers `422`;
- the composer's `[此刻穿着]` is the judge's or the request's outfit again;
- an intimate event runs to its stored `ends_at`, and her two asks are no longer held back;
- the event view has no `text`.

## Clients

1. **Add the seven action types** if your client offers them. Six carry no field. `custom_together` requires `text`, 1 to 100 characters, validated as `custom`'s is; a violation is a `422`. The answer arrives as for any action: the `action_response` frame, then `response` on the stored row in both history routes. A client that renders a stored `custom_together` takes the words from the row's `action.text`.
2. **Expect a picture on an accepted one**, as on any accepted together action: with `image` in the request and the composer configured, the frames run `action_response → meta(reply_text_image) → delta* → done → image_request → final`. A refusal, and a deployment without the composer, are text only, except that `image.force = true` still makes a refused turn `reply_image`, its picture following the refusal.
3. **Add the seven event kinds to your catalog** before or with the upgrade. Each is placed by the accepted action and never drawn: the six in the table above, and `custom_together` (和对方一起做一件事, or 和对方一起：「{text}」 with its words, `short`, 1 h). They appear as `event` on `GET /v2/comp/session/{session_id}/effects` while they last, with `"offline": false`. A client that drops kinds it does not know shows nothing for them.
4. **Read `text` on the event view.** Every event object, on the events route, its streamed `event` frame and `…/effects`, now carries `text`: the user's words of a `custom_together` whose message still exists, `null` for every other kind and once that message is gone.
5. **A `custom_together`'s `message_id` is the user's message that asked**, where a told event's is hers. A client that fetches a told message or an image request by an event's `message_id` must skip this kind. A draw on the events route asked under that event's `event_key` returns the event with `message: null`.
6. **Re-read events after a turn.** The five intimate kinds above can end before the `ends_at` you first read, at the write of a turn or of a declined ask. A client that scheduled anything on the `ends_at` of one of them re-reads `GET …/effects` after every turn and after answering a life choice.
7. **Keep the new kinds out of `kinds`.** Naming one in a `kinds` filter is a `422`, as for `jogging_together`. A `disallow` list needs nothing.
8. **Match enums leniently.** `action.type` gains seven values and an event's `kind` seven.

## Crate users

- `EventKind` gains `DancingTogether`, `AdventuringTogether`, `PlayingWithFireTogether`, `SingingTogether`, `SkinnyDippingTogether`, `CommittingCrimeTogether` and `CustomTogether`. `EventKind::SPECIAL` is now `[EventKind; 32]`: a signature or a constant that spells the length changes, and an exhaustive `match` adds the arms. `EventKind::ENDS_ON_CHEMISTRY_DROP` and `EventKind::HOLDS_BACK_INTIMATE_ASKS` name the two five-kind lists above.
- `UserAction` gains `DanceTogether`, `AdventureTogether`, `PlayWithFireTogether`, `SingTogether`, `SkinnyDipTogether`, `CommitCrimeTogether` and `CustomTogether { text: String }`. `together_kind()` names the event each places and `together_text()` returns a `custom_together`'s words. `romance_gated()` is the new test `pde::together_ruled_out` reads: it is `true` for `ShowerTogether`, `BathTogether`, `SexTogether`, `StreakTogether` and `SkinnyDipTogether`. `refusal_line()` is bond for `CommitCrimeTogether`, and `speaks_emotionally()` is `true` for `SkinnyDipTogether`.
- `eros_engine_store::events::EventRecord` gains `text: Option<String>`: a struct literal adds the field. `EventRepo::current` and `EventRepo::by_key` fill it for a `custom_together`; every other read leaves it `None`.
- `EventRepo::place_together_in_tx` takes a third parameter, `user_message_id: Uuid`, the user's message that asked. Only a `custom_together` row stores it, in `message_id`; a row of any other kind points at no message.
- `AffinityRepo::persist_with_event` returns `Result<bool, sqlx::Error>` instead of `Result<(), sqlx::Error>`, and `AffinityRepo::persist_life_choice_drop` and `persist_life_choice_drop_in_tx` return `Result<(Affinity, bool), sqlx::Error>` instead of `Result<Affinity, sqlx::Error>`. The `bool` is whether the write left the chemistry tier below where the post-decay snapshot had it. A caller that writes affinity itself ends the event with `EventRepo::end_intimate_after_message` (the cause is a user message) or `EventRepo::end_intimate_after` (the cause is a moment), after the affinity write commits and never while an affinity row is locked.

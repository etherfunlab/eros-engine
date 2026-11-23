# Migrating to v2.8.1: twelve together actions and a proposal from the user

**Applies to:**
- clients that keep their own catalog of action types or event kinds, or that match one of them exhaustively;
- callers that serve adult themes only to some users;
- deployments with a custom `filter_prompt` on `[tasks.pde_decision]` that names actions by their `[用户动作]` marker;
- crate users who call `EventRepo::ever` or `pde::together_ruled_out`, match on `EventKind` or `UserAction`, or rely on the length of `EventKind::SPECIAL`.

No migration and no new setting. Every HTTP change is additive: new values in enums a client may match exhaustively, and a picture on an accepted together action, as in v2.8.0.
**Design:** [`2026-11-23-twelve-together-actions-and-propose-design.md`](../superpowers/specs/2026-11-23-twelve-together-actions-and-propose-design.md)

The rules are in [together actions](../life-events-needs.md#showering-bathing-and-the-sauna-together), [the romantic choices](../life-events-needs.md#the-romantic-choices) and [the send route](../api-reference.md#post-compchatsession_idmessagestream).

## What users will notice

- **The user can ask her to do twelve more things together**: go to the toilet, smoke, fall asleep, go to bed, walk, smoke weed, jog, do yoga, watch the stars, ride horses, streak or drink with the user. Each behaves as `sauna_together` does:
  - The judge decides. A refusal drops chemistry one tier and places nothing.
  - `sex_together` and `streak_together` are refused before the judge when the pair is not romance-compatible, as `strip` is. `toilet_together` is refused before the judge for a robot or android persona. The other ten have no such rule.
  - An accepted one puts her in an event (table below). The incident under way ends; a background keeps going. Inside a date nothing is placed and the date goes on.
  - None of the events takes her offline: during `asleep_together` she still answers.
  - An accepted one comes with her picture when the turn carries `image` and the deployment has the composer. The picture shows the event.
  - Her reply to `sex_together` and `streak_together` is in the `emotional` tone when the deployment has [`tones.emotional`](../model-config.md#tones--chat-tones-chat-task-only) above 0; the other ten's is not.
- **Smoking, smoking weed and drinking together count as a give of one** `cigarette`, `herb` or `alcohol` for three things: the habit roll (a persona whose `smoking`, `drug` or `alcoholic` key is `false` refuses unless a one-in-three roll lets it through), the needs an accepted one moves (`social` and `indulgence` up one level), and the lingering effect it leaves. A refusal drops chemistry, not bond, and no journal moment is written.
- **The user can propose to her** (`propose`). The judge decides; a refusal drops nothing. It places no event and is not a picture action. Her reply is in the `emotional` tone when the tone is on.
- **She does not ask what she has already accepted.** Once she has accepted the user's `sex_together` in a session, or in a date of it, she never asks `sleep_together` herself in that session; once she has accepted the user's `propose`, she never asks `propose`. A refused one changes nothing, and an ask of hers already pending stays as it is.
- **Bed with the user counts as sex.** A `having_sex_together` row ends her virginity as the draw sees it and lifts `pregnancy_scare` out of its fifth weight, as `having_sex` and `just_have_sex` do, and makes a pregnancy scare likelier while it is recent. An accept inside a date places no row and so changes none of these.
- **The story director** does not see `having_sex_together`, `streaking_together`, `using_toilet_together` or `asleep_together`; the other eight reach it, as `sauna_together` does.

## Deployments

Nothing to run: no migration, no new setting. Deploy the image as for any release.

- **A custom judge prompt.** The judge's `[用户动作]` line carries the new markers, such as `[用户动作] 对方想和你上床` and `[用户动作] 对方向你求婚`. A new together action refused before the judge ends the line with `，这次你不会答应`, whatever the reason: the romance gate, a machine persona, or the habit roll on a smoke, weed or drink together. A prompt that treats actions generically needs nothing; one that lists actions by marker can add the thirteen.

### Rollback

Roll back the image; there is no database change to undo. On the `v2.8.0` image:
- an event of a new kind written by the new image reads as no event, as any event kind the build does not know, so a `having_sex_together` row no longer ends her virginity;
- an action of a new type on a stored row reads as no action, so her own `sleep_together` and `propose` are no longer settled by the user's accepted ask and can be asked again;
- a request that sends a new action type answers `422`.

## Clients

1. **Add the thirteen action types** if your client offers them. None carries a field. The answer arrives as for any action: the `action_response` frame, then `response` on the stored row in both history routes.
2. **Expect a picture on an accepted together action**, as in v2.8.0: with `image` in the request and the composer configured, the frames run `action_response → meta(reply_text_image) → delta* → done → image_request → final`. A refusal, and a deployment without the composer, are text only. `propose` is not a together action: it follows whatever the judge chose, as any other action turn does.
3. **Add the twelve event kinds to your catalog** before or with the upgrade. Each is placed by the accepted action and never drawn:

   | action | kind | label | category | lasts |
   |---|---|---|---|---|
   | `toilet_together` | `using_toilet_together` | 和对方一起上厕所 | `short` | 30 min |
   | `smoke_together` | `smoking_together` | 和对方一起抽烟 | `short` | 30 min |
   | `fall_asleep_together` | `asleep_together` | 和对方睡在一起 | `mid` | 1–4 h |
   | `sex_together` | `having_sex_together` | 和对方上床 | `short` | 1 h |
   | `walk_together` | `walking_together` | 和对方一起散步 | `short` | 1 h |
   | `smoke_weed_together` | `smoking_weed_together` | 和对方一起抽大麻 | `short` | 30 min |
   | `jog_together` | `jogging_together` | 和对方一起慢跑 | `short` | 1 h |
   | `yoga_together` | `yoga_together` | 和对方一起做瑜伽 | `short` | 1 h |
   | `stargaze_together` | `stargazing_together` | 和对方一起看星星 | `short` | 1 h |
   | `ride_horse_together` | `horse_riding_together` | 和对方一起骑马 | `short` | 1 h |
   | `streak_together` | `streaking_together` | 和对方一起裸奔 | `short` | 30 min |
   | `drink_together` | `drinking_together` | 和对方一起喝酒 | `short` | 1 h |

   They appear as `event` on `GET /v2/comp/session/{session_id}/effects` while they last, with `"offline": false`, and `changes_at` counts their end. A client that drops kinds it does not know shows nothing for them. Do not show `asleep_together` as her being out of reach: she answers throughout.
4. **Keep the new kinds out of `kinds`.** Naming one in a `kinds` filter is a `422`, as for `sauna_together`. A `disallow` list needs nothing: no draw yields them.
5. **Adult themes.** `having_sex_together` and `streaking_together` carry them, and only an accepted `sex_together` or `streak_together` places them. A caller that serves adult themes only to some users does not offer those two actions to the others.
6. **Expect a lingering effect from a habit action.** After an accepted `smoke_together`, `smoke_weed_together` or `drink_together`, the `body_state` frame and `…/effects` list a `cigarette`, `herb` or `alcohol` effect of quantity 1, with no `name`.
7. **Match enums leniently.** `action.type` gains thirteen values and an event's `kind` twelve.

## Crate users

- `EventKind` gains `UsingToiletTogether`, `SmokingTogether`, `AsleepTogether`, `HavingSexTogether`, `WalkingTogether`, `SmokingWeedTogether`, `JoggingTogether`, `YogaTogether`, `StargazingTogether`, `HorseRidingTogether`, `StreakingTogether` and `DrinkingTogether`. `EventKind::SPECIAL` is now `[EventKind; 19]`: a signature or a constant that spells the length changes, and an exhaustive `match` adds the arms. The new `EventKind::SEX` lists the kinds that end her virginity.
- `UserAction` gains `ToiletTogether`, `SmokeTogether`, `FallAsleepTogether`, `SexTogether`, `WalkTogether`, `SmokeWeedTogether`, `JogTogether`, `YogaTogether`, `StargazeTogether`, `RideHorseTogether`, `StreakTogether`, `DrinkTogether` and `Propose`. `together_kind()` names the event each of the first twelve places, and `None` for `Propose`. The new `consumed()` returns what an action has her take: a `give`'s item and quantity, one `Cigarette`, `Herb` or `Alcohol` for the three habit actions, `None` otherwise. `pde::refuses_give` and `effects::active_effects` read it, so they now also cover the habit actions.
- `eros_engine_core::pde::together_ruled_out` takes a second parameter, `machine: bool`: whether the persona is a robot or android. Pass `false` for a persona that is not.
- `eros_engine_store::events::EventRepo::ever` takes `&[EventKind]` instead of one `EventKind` and answers whether the instance has a row of any of them. Pass `&[kind]` for the old behaviour.
- `LifeChoiceRepo::settled` also answers `true` once the session, or a date's sub-session of it, holds a user row whose action is the kind's mirror with the answer `accept`; `LifeChoiceKind::user_ask()` names that mirror (`sex_together` for `SleepTogether`, `propose` for `Propose`).
- `StoryRepo::life_events` leaves out `having_sex_together`, `streaking_together`, `using_toilet_together` and `asleep_together`.

# Migrating to v2.8.2: the focused tone, four together actions and the user's gender in a picture

**Applies to:**
- deployments that want the `focused` chat tone, or that have a custom composer `filter_prompt` or a custom judge prompt that names actions by marker;
- deployments with `fantasy` personas (five kinds are no longer drawn for them);
- clients that keep their own catalog of action types or event kinds, or that match one of them exhaustively;
- clients that want the user drawn as their own gender in a picture with the persona;
- readers of `metadata.tone` on stored rows, or of the composer's audit `inputs`;
- crate users who match on `EventKind`, `UserAction` or `Tone`, build `Tones` by hand, rely on the length of `EventKind::SPECIAL`, or call `pde::world_ruled_out`.

No migration. Every field on the wire is additive (four action types, four event kinds, one optional request field `user_gender`, one new `metadata.tone` value and one new key in the composer's audit `inputs`), but one behaviour changes: a `fantasy` persona stops drawing five kinds. One new setting, `tones.focused`, is off when absent.
**Design:** [`2026-11-24-focused-tone-four-together-actions-and-user-gender-design.md`](../superpowers/specs/2026-11-24-focused-tone-four-together-actions-and-user-gender-design.md)

The other notes for this release are [her accepted bed and bath asks place their events](v2-8-2-life-choice-events.md), [Affinity 5.0, the relationship status and the cold war](v2-8-2-affinity-5-0.md), [her period on a 28-day cycle](v2-8-2-period-cycle.md) and [fact groups](v2-8-2-fact-groups.md).

The rules are in [her world](../life-events-needs.md#her-world), [together actions](../life-events-needs.md#showering-bathing-and-the-sauna-together), [at work](../life-events-needs.md#at-work), [tones](../model-config.md#tones--chat-tones-chat-task-only), [the composer](../model-config.md#taskschat_image_prompt_compose--image-prompt-composer-required-for-image-turns) and [the send route](../api-reference.md#post-compchatsession_idmessagestream).

## What users will notice

- **A fantasy persona no longer draws modern-life kinds.** `saw_bag`, `saw_clothes`, `out_shopping`, `want_to_quit_job` and `admiring_art` fit a `modern` persona only (`art_metadata.setting` absent reads `modern`). For a `fantasy` persona they are dropped before any weighting, whatever the request's `kinds` allows, and `prefer_events` naming `out_shopping` has no effect. Stored rows of these kinds stay as they are. A `modern` persona draws as before.
- **The user can ask her to work out, swim, cycle or hike together** (`workout_together`, `swim_together`, `cycle_together`, `hike_together`). Each behaves as `jog_together` does:
  - The judge decides. A refusal drops chemistry one tier and places nothing. There is no romance gate and no species gate.
  - An accepted one puts her in an event (table below). The incident under way ends; a background keeps going. Inside a date nothing is placed and the date goes on. None takes her offline.
  - An accepted one comes with her picture when the turn carries `image` and the deployment has the composer. The picture shows the event.
  - The reply is never in the `emotional` tone. The four events reach the story director, as `jogging_together` does.
- **An action her world lacks is refused for free.** A together action whose event does not fit the persona's world is refused before the judge: a `fight_together` to a persona that is not `fantasy`, as before, and a `cycle_together` to a `fantasy` one. The refusal places nothing and drops no line, with or without the LLM judge.
- **At work she answers like someone at work**, once the deployment sets `tones.focused`. She is at work while a `temp_work` incident is under way, or a `business_trip` background with no incident; only a `modern` persona has either. Such a turn is `focused` with the key's probability. A persona whose `art_metadata.employed` is `full_time` is career-minded: off work, with no incident under way, 1 turn in 8 is `focused`, in either world. A `focused` reply keeps the history, the profile, the memories and the relationship, leaves out `[feelings]` and `[emotional_context]`, is at most one sentence and 40 characters, and its `[this_turn]` holds one work line. Only a 1:1 text turn without an action draws, and a turn that came up `emotional` stays `emotional`.
- **A picture of something she does with the user draws the user as the gender the caller sends.** With `user_gender` on the chat request, and a kind under way that she shares with the user (`traveling_together`, `on_a_date`, any `…_together` kind, the one an accepted together action places this turn included), the composer is told the user's gender. Whether the user appears in the picture is still the composer's reading of the scene. Without `user_gender`, pictures draw as before.

## Deployments

Nothing to run: no migration. Deploy the image as for any release; [Affinity 5.0](v2-8-2-affinity-5-0.md) has its own deploy order.

- **The `focused` tone is off until you set it.** `tones = { emotional = 0.05, focused = 1.0 }` on `[tasks.chat_companion]`; either key works without the other, and a value outside `[0.0, 1.0]` refuses to boot. The key is the probability at work. It also switches on the off-work 1/8 for `full_time` personas, which it does not scale: a deployment that sets the key for turns at work gets that too. The assistant rows of a turn that made either draw carry `metadata.tone`, now `"emotional"`, `"focused"` or `"default"`.
- **`employed = full_time` has a second reading.** It still weighs the work kinds three times; with `tones.focused` on, it also makes the persona career-minded. Review the cards that set it before you turn the tone on.
- **A custom composer `filter_prompt`.** When the chat request carries a `user_gender` of `male` or `female` and a kind named in `[此刻状态]` is one she shares with the user (the one an accepted together action places this turn included), the composer's payload gains a `[对方]` section right after `[此刻状态]`, holding `男性` or `女性`. The built-in prompt names it. A custom prompt should add a line such as: *When a [对方] section is present and the partner appears in the picture, draw the partner as that gender.* Without it the model still sees the section. Without the section the payload is byte-identical to before.
- **A custom judge prompt.** The judge's `[用户动作]` line carries the new markers, as in `[用户动作] 对方想和你一起健身`, `对方想和你一起游泳`, `对方想和你一起骑单车` and `对方想和你一起爬山`. A fantasy persona's `cycle_together` ends the line with `，这次你不会答应` (`[用户动作] 对方想和你一起骑单车，这次你不会答应`), as a modern persona's `fight_together` does. A prompt that treats actions generically needs nothing.
- **Audit.** Every `engine.chat_images_events` row of the image-prompt composer (every `source` but `image_edit`) carries `partner_gender` in `inputs`: the `[对方]` section as sent, or `""` without one (always `""` on the standalone compose endpoint, on a group turn's picture, and on an event message's, the one proactive message that carries a picture). A reader that expects a fixed key set should accept it. `user_gender` itself is kept in the turn's `chat_turn_queue.params`, beside the locale fields, and is not written to the user row.

### Rollback

These changes need no database change; roll the release back as [Affinity 5.0](v2-8-2-affinity-5-0.md#rollback) says. On the previous image:
- a `tones` table that names `focused` refuses to boot: remove the key first;
- an event of one of the four new kinds reads as no event, as any kind the build does not know;
- an action of a new type on a stored row reads as no action, and a request that sends one answers `422`;
- `user_gender` is ignored, in a request and in a queued row's params;
- a `fantasy` persona draws the five modern-life kinds again.

## Clients

1. **Add the four action types** if your client offers them. None carries a field. The answer arrives as for any action: the `action_response` frame, then `response` on the stored row in both history routes. A `cycle_together` to a `fantasy` persona is always `refuse` and costs nothing; a client may offer it to `modern` personas only, as it offers `fight_together` to `fantasy` ones only.
2. **Expect a picture on an accepted one**, as on any accepted together action: with `image` in the request and the composer configured, the frames run `action_response → meta(reply_text_image) → delta* → done → image_request → final`. A refusal, and a deployment without the composer, are text only, except that `image.force = true` still makes a refused turn `reply_image`, its picture following the refusal.
3. **Add the four event kinds to your catalog** before or with the upgrade. Each is placed by the accepted action and never drawn:

   | action | kind | label | category | lasts |
   |---|---|---|---|---|
   | `workout_together` | `working_out_together` | 和对方一起健身 | `short` | 1 h |
   | `swim_together` | `swimming_together` | 和对方一起游泳 | `short` | 1 h |
   | `cycle_together` | `cycling_together` | 和对方一起骑单车 | `short` | 1 h |
   | `hike_together` | `hiking_together` | 和对方一起爬山 | `mid` | 2–3 h |

   They appear as `event` on `GET /v2/comp/session/{session_id}/effects` while they last, with `"offline": false`. A client that drops kinds it does not know shows nothing for them.
4. **Keep the new kinds out of `kinds`.** Naming one in a `kinds` filter is a `422`, as for `jogging_together`. A `disallow` list needs nothing.
5. **Send `user_gender`** on `POST /comp/chat/{session_id}/message/stream` and `POST /v2/comp/session/{session_id}/message/async` if you know the user's gender: `"male"` or `"female"`, top-level beside `user_timezone`. Any other value, a non-string included, reads as absent and never fails the request. It is read from the turn alone, so send it on every turn. It shapes pictures only: the judge, the reply prompt, romance compatibility and every gate ignore it. No other route takes it.
6. **A per-world catalog.** A client that lists which kinds a persona's world allows moves `saw_bag`, `saw_clothes`, `out_shopping`, `want_to_quit_job` and `admiring_art` to `modern` only, and `cycling_together` with them.
7. **Match enums leniently.** `action.type` gains four values, an event's `kind` four, and `metadata.tone` one (`focused`).

## Crate users

- `EventKind` gains `WorkingOutTogether`, `SwimmingTogether`, `CyclingTogether` and `HikingTogether`. `EventKind::SPECIAL` is now `[EventKind; 25]`: a signature or a constant that spells the length changes, and an exhaustive `match` adds the arms. `fits(Setting::Fantasy)` is now `false` for `SawBag`, `SawClothes`, `OutShopping`, `WantToQuitJob`, `AdmiringArt` and `CyclingTogether`. The new `EventKind::is_shared()` is `true` for `TravelingTogether`, `OnADate` and every kind a together action places.
- `UserAction` gains `WorkoutTogether`, `SwimTogether`, `CycleTogether` and `HikeTogether`. `together_kind()` names the event each places, and `refusal_line()` is chemistry.
- `eros_engine_core::pde::world_ruled_out(event, setting)` keeps its signature and answers whether the turn's action is a together action whose event does not fit `setting`: a `fight_together` outside `Setting::Fantasy`, and a `cycle_together` in it. Such a refusal drops no line.
- `eros_engine_core::tone::Tone` gains `Focused` (`as_str()` is `"focused"`); an exhaustive `match` adds the arm. The module gains `at_work(background, foreground)`, `focused_probability(key, background, foreground, employed)`, `draw_focused(p, roll)` and the constant `CAREER_MINDED_P` (`0.125`).
- `eros_engine_llm::model_config::Tones` gains `focused: f64` (`0.0` when absent): a struct literal adds the field. `ModelConfig::tone_focused_probability()` reads it, and `validate_tones` checks both keys.

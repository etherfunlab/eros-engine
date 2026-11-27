# Migrating to v2.9.1: dessert and soup, taste habits, the mean and drowsy tones

**Applies to:**
- clients that list the `give` items, or that read `metadata.tone` on stored rows;
- downstreams that author character cards;
- deployments that want the `mean` or the `drowsy` chat tone;
- crate users who match on `GivenItem` or `Tone`, or build `Habits` or `Tones` by hand.

No migration, no route and no request or response field. Two new `give` items, four new card keys, two new `model_config.toml` keys and two new `metadata.tone` values. With no card carrying the new keys and the two tone keys absent, every prompt, request and stored row is what it was.

**Design:** [`2026-11-28-dessert-soup-taste-habits-mean-and-drowsy-tones-design.md`](../superpowers/specs/2026-11-28-dessert-soup-taste-habits-mean-and-drowsy-tones-design.md)

The rules are in [basic needs](../life-events-needs.md#persona-settings), [tones](../model-config.md#tones--chat-tones-chat-task-only) and the [character card](../eros-character-v1.2.md).

## What users will notice

- **Two more things to hand her.** A `dessert` and a `soup`. Both linger for a few turns as food does. A dessert loosens her up as a good dish does; a soup moves nothing.
- **Nothing else until a card or the deployment opts in.** With a card key set:
  - `sugar`, `salty`, `spicy` set to `true` are things she craves: her craving comes back sooner, and when it is high she may say she wants something sweet, salty or spicy. `sugar` set to `false` makes her turn down a dessert two times in three.
  - `diet` set to `true` makes her turn down plain food, a dish and a dessert two times in three, whatever she craves. Soup and drinks pass.
- **With `tones.mean` set, she is short and sharp during a cold war.** A text turn without a user action is written with the chat history in front of her and one line telling her she is still angry; the closer the pair, the less she explains.
- **With `tones.drowsy` set, she wakes up sleepy.** Her first text reply after the pair slept together or went to bed together reads like someone just awake: short, and without the earlier conversation in front of her.

## Deployments

Nothing to run: no migration. Deploy the image as for any release.

- **Both tones are off until you set them.** `tones = { emotional = 0.05, focused = 1.0, erotic = 1.0, mean = 1.0, drowsy = 1.0 }` on `[tasks.chat_companion]`; each key works without the others, and a value outside `[0.0, 1.0]` refuses to boot.
- **Order.** `mean`, `drowsy`, `erotic`, `emotional`, `focused`: the first roll to win names the message.
- **A cold war may last longer with `mean` on.** A cold war ends when three judged turns in a row are above the judge's lowest patience level, and her replies are part of what the judge reads. The rule is unchanged; if cold wars run long, lower the key. Every `mean` reply is on record (`metadata.tone`), as is every start and end of a cold war (the `relationship` events).
- **`drowsy` needs her first line.** The moment lasts from the end of an `asleep_together` or `having_sex_together` event until her first line with text in that session, for at most 12 hours, on a turn without a user action and with no incident under way. A reply to an action, or a proactive message, uses it up without a `drowsy` reply.
- **A date during a cold war rolls `mean` too.** Its turns read the inviting session's cold war.
- **Review the cards before you set the taste keys.** Three or more habit keys set to `true` (the tastes count with `smoking`, `alcoholic`, `drug`, `coffee` and `gluttony`) put her craving on its fastest pace.
- **Audit.** `metadata.tone` on the message's assistant rows is `mean` or `drowsy` on a won roll, and `default` when the roll was made and lost and no later one won.

### Rollback

Remove the two keys and restart, or redeploy the previous image. On the previous image a `tones` table that names `mean` or `drowsy` refuses to boot: remove them first. A stored turn whose `give` is a `dessert` or a `soup` no longer reads as an action there, so its lingering effect is not rendered. A card that says `"spec_version": "1.2"` is refused by the previous image; the four keys on a `"1.1"` card are ignored by it. Stored rows whose `metadata.tone` is `mean` or `drowsy` stay as they are; nothing reads the value back except the speech markup, which says nothing for a tone it does not know.

## Clients

1. **Offer the two items.** `give` accepts `item: "dessert"` (unit 份) and `item: "soup"` (unit 碗), with the same `name` and `quantity` as any consumable. The `body_state` frame and the effects route may carry them.
2. **Match `metadata.tone` leniently.** It gains `mean` and `drowsy`.
3. **Cards.** `spec_version` `"1.2"` is accepted beside `"1"` and `"1.1"`, and all three are read the same way. The four keys are booleans under `art_metadata`; any other value reads as unset.
4. Nothing else changes: the frames, the routes and the event catalog are as they were.

## Crate users

- `eros_engine_core::types::GivenItem` gains `Dessert` and `Soup`; an exhaustive `match` adds the arms.
- `eros_engine_core::needs::Habits` gains `sugar`, `salty`, `spicy` and `diet` (each `Option<bool>`): a struct literal adds the fields or `..Habits::default()`. `count_true()` counts the three tastes and not `diet`. `Habits::rolls_on(item)` says whether a give of the item takes the habit roll; `pde::refuses_give` reads it.
- `eros_engine_core::tone::Tone` gains `Mean` and `Drowsy` (`as_str()` is `"mean"`, `"drowsy"`). `on_emotional_template()` is `true` for `Drowsy` too; the new `on_focused_template()` is `true` for `Focused` and `Mean`. The module gains `mean_probability(key, cold_war)`, `drowsy_probability(key, just_woke, foreground)` and the constant `WOKE_WITHIN_HOURS`.
- `eros_engine_core::events::EventKind::WAKES_FROM` lists the two kinds she wakes from.
- `eros_engine_llm::model_config::Tones` gains `mean: f64` and `drowsy: f64` (`0.0` when absent): a struct literal adds the fields. `ModelConfig::tone_mean_probability()` and `tone_drowsy_probability()` read them, and `validate_tones` checks all five keys.
- `eros_engine_store::events::EventRepo::just_woke(session_id, instance_id, at)` is the read behind `drowsy`.

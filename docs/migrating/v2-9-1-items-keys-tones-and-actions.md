# Migrating to v2.9.1: new give items, card keys, tones, actions and dance styles

**Applies to:**
- clients that list the `give` items, the action types or the event kinds, or that read `metadata.tone` on stored rows or `message_id` on an event;
- downstreams that author character cards;
- deployments that want the `mean` or the `drowsy` chat tone;
- crate users who match on `GivenItem`, `Tone`, `UserAction` or `EventKind`, or build `Habits`, `Tones` or `PersonaTraits` by hand.

No migration and no route. Two new `give` items, seven new card keys, two new `model_config.toml` keys, two new `metadata.tone` values, seven new action types with six event kinds, and one optional field (`style`) on the three dance actions. With no card carrying the new keys, the two tone keys absent and no client sending the new types or the field, every prompt and request is what it was. One stored value changes for existing traffic: a dance event now points at the user's message that asked (below).

**Designs:** [`dessert, soup, taste habits, mean and drowsy`](../superpowers/specs/2026-11-28-dessert-soup-taste-habits-mean-and-drowsy-tones-design.md), [`dance styles and the taste keys`](../superpowers/specs/2026-11-28-dance-styles-design.md), [`ask_strip`](../superpowers/specs/2026-11-28-ask-strip-design.md), [`diving, skiing and surfing together`](../superpowers/specs/2026-11-28-dives-ski-and-surf-together-design.md)

The rules are in [basic needs](../life-events-needs.md#persona-settings), [tones](../model-config.md#tones--chat-tones-chat-task-only) and the [character card](../eros-character-v1.2.md).

## What users will notice

- **Two more things to hand her.** A `dessert` and a `soup`. Both linger for a few turns as food does. A dessert loosens her up as a good dish does; a soup moves nothing.
- **Nothing else until a card or the deployment opts in.** With a card key set:
  - `sugar`, `salty`, `spicy` set to `true` are things she craves: her craving comes back sooner, and when it is high she may say she wants something sweet, salty or spicy. `sugar` set to `false` makes her turn down a dessert two times in three.
  - `diet` set to `true` makes her turn down plain food, a dish and a dessert two times in three, whatever she craves. Soup and drinks pass.
- **With `tones.mean` set, she is short and sharp during a cold war.** A text turn without a user action is written with the chat history in front of her and one line telling her she is still angry; the closer the pair, the less she explains.
- **Dances have a style.** With a client that sends one, she, the judge and the picture know it is a tango or a pole dance. Nothing else about a dance changes.
- **She can love or dislike dancing, being naked together, and risk.** With `dance`, `naturism` or `risky` set to `love` on her card she takes such an action two times in three whatever the judge would say; with `dislike` she turns it down two times in three.
- **She can be asked to undress.** `ask_strip` has her take her own clothes off, as far as the pair's chemistry goes: a coat at the lowest tier, everything at the highest.
- **Six more things to do together.** Scuba diving, surfing and skiing, and each of them naked (free diving for the dive).
- **With `tones.drowsy` set, she wakes up sleepy.** Her first text reply after the pair slept together or went to bed together reads like someone just awake: short, and without the earlier conversation in front of her.

## Deployments

Nothing to run: no migration. Deploy the image as for any release.

- **Both tones are off until you set them.** `tones = { emotional = 0.05, focused = 1.0, erotic = 1.0, mean = 1.0, drowsy = 1.0 }` on `[tasks.chat_companion]`; each key works without the others, and a value outside `[0.0, 1.0]` refuses to boot.
- **Order.** `mean`, `drowsy`, `erotic`, `emotional`, `focused`: the first roll to win names the message.
- **A cold war may last longer with `mean` on.** A cold war ends when three judged turns in a row are above the judge's lowest patience level, and her replies are part of what the judge reads. The rule is unchanged; if cold wars run long, lower the key. Every `mean` reply is on record (`metadata.tone`), as is every start and end of a cold war (the `relationship` events).
- **`drowsy` needs her first line.** The moment lasts from the end of an `asleep_together` or `having_sex_together` event until her first line with text in that session, for at most 12 hours, on a turn without a user action and with no incident under way. A reply to an action, or a proactive message, uses it up without a `drowsy` reply.
- **A date during a cold war rolls `mean` too.** Its turns read the inviting session's cold war.
- **Review the cards before you set the taste keys.** Three or more habit keys set to `true` (the tastes count with `smoking`, `alcoholic`, `drug`, `coffee` and `gluttony`) put her craving on its fastest pace.
- **`love` overrides the judge.** A card that says `dance`, `naturism` or `risky` is `love` has her accept two times in three even when the judge would refuse, at any affinity; the rules that refuse before the judge (a pair that is not romance-compatible, her world, her period) still come first. Set it on the personas it fits.
- **A dance event now points at the user's message.** `dancing_together` and `naked_dancing_together` rows carry the asking message's id in `message_id`, with or without a style, as `custom_together` rows do. Rows written before the upgrade keep pointing at none and read without a style.
- **Audit.** `metadata.tone` on the message's assistant rows is `mean` or `drowsy` on a won roll, and `default` when the roll was made and lost and no later one won.

### Rollback

Remove the two keys and restart, or redeploy the previous image. On the previous image a stored turn whose action is one of the seven new types, or a dance with a `style`, does not parse and reads as a turn without an action; an event of one of the six new kinds does not read (stop placing them before rolling back, or let them end: none lasts more than three hours). On the previous image a `tones` table that names `mean` or `drowsy` refuses to boot: remove them first. A stored turn whose `give` is a `dessert` or a `soup` no longer reads as an action there, so its lingering effect is not rendered. A card that says `"spec_version": "1.2"` is refused by the previous image; the four keys on a `"1.1"` card are ignored by it. Stored rows whose `metadata.tone` is `mean` or `drowsy` stay as they are; nothing reads the value back except the speech markup, which says nothing for a tone it does not know.

## Clients

1. **Offer the two items.** `give` accepts `item: "dessert"` (unit 份) and `item: "soup"` (unit 碗), with the same `name` and `quantity` as any consumable. The `body_state` frame and the effects route may carry them.
2. **Match `metadata.tone` leniently.** It gains `mean` and `drowsy`.
3. **Cards.** `spec_version` `"1.2"` is accepted beside `"1"` and `"1.1"`, and all three are read the same way. The four keys are booleans under `art_metadata`; any other value reads as unset.
4. **Dance styles.** `ask_dancing`, `dance_together` and `dance_naked_together` accept an optional `style` (one of 23 values, or `custom` with a `name` of 1–32 characters). A `style` on any other action, a `name` without `custom`, or an unknown style is a 422. The history routes return both fields as sent.
5. **Seven action types.** `ask_strip` (no field; a picture turn like `strip`), and the together actions `scuba_dive_together`, `surf_together`, `ski_together`, `free_dive_together`, `surf_naked_together`, `ski_naked_together`. Each together action places an event of a new kind (`scuba_diving_together`, `surfing_together`, `skiing_together`, `free_diving_together`, `naked_surfing_together`, `naked_skiing_together`): add their labels to the event catalog. None can be drawn, so none is valid in a `kinds` filter.
6. **A dance event's `message_id` and `text`.** On the events routes a `dancing_together` or `naked_dancing_together` event now carries the user's message id in `message_id` (it was `null`) and, when the dance named a style, the style's words in `text` (as `custom_together` carries the user's words). If you treat an event's `message_id` as her message, exclude these two kinds as you exclude `custom_together`.
7. **The judge can be overruled both ways.** With a taste key on the card, an `action_response` of `accept` or `refuse` may be the engine's roll and not the judge's.
8. Nothing else changes: the frames and the routes are as they were.

## Crate users

- `eros_engine_core::types::GivenItem` gains `Dessert` and `Soup`; an exhaustive `match` adds the arms.
- `eros_engine_core::needs::Habits` gains `sugar`, `salty`, `spicy` and `diet` (each `Option<bool>`): a struct literal adds the fields or `..Habits::default()`. `count_true()` counts the three tastes and not `diet`. `Habits::rolls_on(item)` says whether a give of the item takes the habit roll; `pde::refuses_give` reads it.
- `eros_engine_core::tone::Tone` gains `Mean` and `Drowsy` (`as_str()` is `"mean"`, `"drowsy"`). `on_emotional_template()` is `true` for `Drowsy` too; the new `on_focused_template()` is `true` for `Focused` and `Mean`. The module gains `mean_probability(key, cold_war)`, `drowsy_probability(key, just_woke, foreground)` and the constant `WOKE_WITHIN_HOURS`.
- `eros_engine_core::events::EventKind::WAKES_FROM` lists the two kinds she wakes from.
- `eros_engine_llm::model_config::Tones` gains `mean: f64` and `drowsy: f64` (`0.0` when absent): a struct literal adds the fields. `ModelConfig::tone_mean_probability()` and `tone_drowsy_probability()` read them, and `validate_tones` checks all five keys.
- `eros_engine_store::events::EventRepo::just_woke(session_id, instance_id, at)` is the read behind `drowsy`.
- `eros_engine_core::types::UserAction`: `AskDancing`, `DanceTogether` and `DanceNakedTogether` are now struct variants with `style: Option<DanceStyle>` and `name: Option<String>`; `UserAction::ask_dancing()`, `dance_together()` and `dance_naked_together()` build the style-less ones, and a pattern takes `{ .. }`. New unit variants: `AskStrip`, `ScubaDiveTogether`, `SurfTogether`, `SkiTogether`, `FreeDiveTogether`, `SurfNakedTogether`, `SkiNakedTogether`. `UserAction::together_text()` returns `Option<Cow<str>>` and includes a styled dance's words; `UserAction::dance_words()` and `types::dance_words(style, name)` give them.
- `eros_engine_core::types::DanceStyle`: the 23 styles and `Custom`, with `as_str`, `parse`, `ALL` and `zh`.
- `eros_engine_core::events::EventKind` gains `ScubaDivingTogether`, `SurfingTogether`, `SkiingTogether`, `FreeDivingTogether`, `NakedSurfingTogether` and `NakedSkiingTogether`; `SPECIAL`, `ENDS_ON_CHEMISTRY_DROP` and `HOLDS_BACK_INTIMATE_ASKS` grow with them.
- `eros_engine_core::events::Taste` (`Love` / `Neutral` / `Dislike`) and `PersonaTraits { dance, naturism, risky }`; `pde::taste_roll(event, &persona, roll)` and `pde::LOVER_ACCEPT`.
- `eros_engine_store::events::points_at_the_user` is true for the two dance kinds too.

# Migrating to v2.9.0: the erotic tone

**Applies to:**
- deployments that want the `erotic` chat tone;
- deployments with personas whose card sets `sex_addict` or `is_virgin` (each gains a second reading once the tone is on);
- clients that answer her `sleep_together` or `bathe_together` ask, or that read `metadata.tone` on stored rows;
- crate users who match on `Tone` or build `Tones` by hand.

No migration, no route and no request or response field. One new `model_config.toml` key, off when absent, and one new `metadata.tone` value. With the key absent every prompt, request and stored row is what it was.

**Design:** [`2026-11-27-erotic-tone-design.md`](../superpowers/specs/2026-11-27-erotic-tone-design.md)

The other note for this release is [dancing and massage together, and the passion attribute](v2-9-0-dance-massage-passion.md), which adds the accepted passion gestures to the fixed moments below.

The rules are in [tones](../model-config.md#tones--chat-tones-chat-task-only) and [when she wants you](../life-events-needs.md#when-she-wants-you).

## What users will notice

Nothing until the deployment sets `tones.erotic`. With it set:

- **Three moments read alluring.** Her reply to a `sex_together` she accepts, her reply when the user accepts her `sleep_together` or `bathe_together` ask, and the message `sudden_desire` sends when it starts are written in the `erotic` tone: an `emotional` message that is also told who the user is to her, with one line saying she wants them. Only that one message of each; the turns that follow are ordinary turns.
- **A strong-desire persona answers that way now and then.** A persona whose card says `sex_addict = true` has a 1 in 8 chance of it on an everyday 1:1 text turn, from chemistry tier 3. On such a turn she does not have the chat history in front of her, as on any `emotional` turn.
- **A virgin persona waits for chemistry tier 4.** A persona whose card says `is_virgin = true` is never `erotic` below tier 4, at the three moments or in everyday chat, and that does not change once she has had sex. Below the line the three moments are answered as before.
- **Not between an incompatible pair, and not during a cold war.** Both are answered as before.

## Deployments

Nothing to run: no migration. Deploy the image as for any release.

- **The tone is off until you set it.** `tones = { emotional = 0.05, focused = 1.0, erotic = 1.0 }` on `[tasks.chat_companion]`; each key works without the others, and a value outside `[0.0, 1.0]` refuses to boot. `erotic` is the probability at the three moments and the tone's switch; the everyday 1 in 8 is a constant it does not scale.
- **Order.** `erotic` wins over `emotional`, and `emotional` over `focused`. A persona at work with `tones.focused = 1.0` can therefore answer an everyday turn in the `erotic` tone when her card says `sex_addict = true`.
- **`sex_addict = true` and `is_virgin = true` have a second reading.** They still do what they do to her life events. Review the cards that set them before you turn the tone on. The tone reads `is_virgin` from the card alone; the life-event draw still ends her virginity with her first time.
- **A fixed moment below chemistry tier 3.** A persona who is not a virgin has no chemistry floor at the three moments (a hygienic persona asks for a bath from tier 2). Tiers 1 to 3 read the same line.
- **Audit.** `metadata.tone` on the message's assistant rows is `erotic` on a won roll. Her reply to an accepted ask that lost its roll carries `default`; it carried no `tone` key before, and still carries none with the key absent.

### Rollback

Remove the key and restart, or redeploy the previous image. On the previous image a `tones` table that names `erotic` refuses to boot: remove the key first. Stored rows whose `metadata.tone` is `erotic` stay as they are; nothing reads the value back.

## Clients

1. **Send `affinity_scope` when you answer her ask.** Her reply to `POST /v2/comp/session/{session_id}/life-choices/{choice_id}` is `erotic` only for a romantically compatible pair, read from the scope that request carries. A request without one reads the default scope, which is not, and the reply is then the default one. Send the scope your chat turns send.
2. **Match `metadata.tone` leniently.** It gains `erotic`.
3. Nothing else changes: the frames, the routes and the action and event catalogs are as they were.

## Crate users

- `eros_engine_core::tone::Tone` gains `Erotic` (`as_str()` is `"erotic"`); an exhaustive `match` adds the arm. `Tone::on_emotional_template()` is `true` for `Emotional` and `Erotic`. The module gains `EroticGate` with `open()`, `fixed_erotic_probability(gate)`, `everyday_erotic_probability(gate, sex_addict, foreground)`, `won(p, roll)` and the constant `STRONG_DESIRE_P`.
- `eros_engine_llm::model_config::Tones` gains `erotic: f64` (`0.0` when absent): a struct literal adds the field. `ModelConfig::tone_erotic_probability()` reads it, and `validate_tones` checks all three keys.

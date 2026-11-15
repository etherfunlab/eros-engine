# Migrating to v2.7.1: four needs

**Applies to:**
- clients that read `needs` from the `body_state` frame or `GET …/effects`;
- deployments that run the LLM judge (`[tasks.pde_decision].filter_prompt` set).

No migration. The `needs` object loses two keys, so a client that requires them must change before the new image serves.
**Design:** [`2026-11-16-remove-hunger-stamina-design.md`](../superpowers/specs/2026-11-16-remove-hunger-stamina-design.md)

The persona's needs are four: `social`, `emotional`, `outing` and `indulgence`. `hunger` and `stamina` are gone from the judge, the reply prompt, the event draw, the emotional tone and the API. The rules are in [life-events-needs.md](../life-events-needs.md#needs).

## What users will notice

- **She stops talking about being hungry or tired.** The reply's `此刻的需要` line no longer says 饿了, 不饿, 很累 or 精神很好. Before, hunger rested at `high`, so outside the hours right after a meal nearly every reply carried 饿了.
- **Food gives move no need.** An accepted `food` still leaves its 吃饱 body effect. A cigarette, alcohol, herb or a dish still raises `social` and `indulgence`; medicine and coffee still lower them.
- **Some events come up at their plain weight.** Three needs factors are gone: low stamina halving `temp_work` and `working_out`, low stamina doubling `unwell`, `sick`, `bathing`, `asleep`, `want_hot_spring` and `want_sauna`, and high hunger doubling `grocery_shopping`.

## What to change

- **Stop requiring `hunger` and `stamina` in the needs object, before the upgrade.** `needs` on the `body_state` frame and on `GET /v2/comp/session/{session_id}/effects` is `{social, emotional, outing, indulgence}`. A client that treats a needs object without `hunger` or `stamina` as no reading will show no needs at all against the new image. Once it accepts the four keys, the same client works against both the old and the new image: the old one still sends six keys, and the extra two can be ignored. Drop any labels or icons for the two needs at the same time.
- **Drop `hunger` and `stamina` from your judge prompt, together with the upgrade.** The strict `response_format` now requires `social`, `emotional`, `outing` and `indulgence`. Do not change the prompt before the upgrade: the old image's strict schema still requires the two keys, and a prompt that no longer describes them leaves the judge guessing. A non-strict judge on an old prompt that still rates them keeps working: the two keys are ignored. The reference prompt is in [`examples/model_config.toml`](../../examples/model_config.toml).

## What to check

- **The emotional tone fires less often** in a deployment with `tones.emotional` above 0. The ×3 needs factor now applies when `emotional` or `indulgence` is high. Hunger resting at `high` used to apply it on most turns. Raise the base if you want the old rate.
- **Stored ratings need nothing.** Ratings and voice snapshots written before the upgrade still read, with `hunger` and `stamina` ignored.

## Rollback

The `v2.7.0` image requires `hunger` and `stamina` in a stored rating:
- a rating the new image wrote reads as no rating, so the judge sees `[需求] 尚未判定` and rates again on the next judged turn;
- a voice call whose bootstrap snapshot the new image froze with needs reads as malformed, so that call starts with no injected context (insights, previous call, effects, needs, story). The snapshot is written once; rolling forward again restores it.

Roll the judge prompt back with the image.

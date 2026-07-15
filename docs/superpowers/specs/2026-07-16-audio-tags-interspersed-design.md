# Voice audio tags: interspersed multi-tag delivery (`AUDIO_TAGS_ADDENDUM` rewrite)

**Date:** 2026-07-16
**Status:** Design approved, ready for implementation plan
**Related:** `docs/superpowers/specs/2026-07-11-voice-tts-audio-tags-design.md`
(the feature this tunes; shipped in PR #152)

## Summary

Production observation: with `tts_audio_tags = true` (prod voice runs
grok-4.20), every completion emits exactly **one** audio tag, always at the
**start** of the reply. Wanted: several tags woven **through** the sentences,
at the emotional beats.

Root cause is the addendum's own text (`AUDIO_TAGS_ADDENDUM`,
`crates/eros-engine-llm/src/model_config.rs:711`):

1. "**Use them sparingly**, only when they fit the moment" — the model
   minimizes to a single tag.
2. Both inline examples put the tag at utterance start (`[laughs] that's so
   funny`, `[whispers] come closer`) — the model pattern-matches "one leading
   tag".

Fix: rewrite the addendum's guidance and examples. **Prompt-text only** — no
config key, no composition change, no behavior change anywhere else in the
voice path (tags still flow through verbatim; the four-state
`resolve_voice` matrix from the #152 spec is untouched).

Explicitly **not** in scope: a density config knob, engine-side
post-processing that injects or repositions tags, per-persona tag styles, any
change to `VOICE_SPEECH_BASE_AUDIO_TAGS` / `DEFAULT_VOICE_DIRECTIVE` or the
`(filter_prompt × tts_audio_tags)` composition logic.

## The change

`AUDIO_TAGS_ADDENDUM` is rewritten with four deltas; everything else in the
constant is kept verbatim (the 15-tag list, the "not limited to this list"
clause, the "write tags in English even when speaking another language"
clause, and the "everything outside the brackets is spoken aloud" closer):

1. **Drop** "Use them sparingly, only when they fit the moment."
2. **Soft density target:** "Aim for two to four tags per reply."
3. **Placement directive:** tags go at the emotional beats; mid-sentence
   placements are better than tagging only the start; never bunch them all at
   the beginning.
4. **Examples demonstrate interspersal**, replacing the two leading-tag
   examples. Two full samples, one Chinese-with-English-tags (mirrors prod
   usage and demonstrates the English-tags rule in context), one English:
   - `今天全搞砸了 [sighs] 不想说了…… [giggles] 骗你的啦，你怎么当真了`
   - `wait [gasp] you actually did it? [laughs] no way`

### Reference text (implementation may polish wording, not semantics)

> Weave inline audio tags through your speech to make it expressive. An audio
> tag is a short cue in square brackets placed right before the words it
> affects. Aim for two to four tags per reply, placed at the emotional beats —
> mid-sentence placements are better than tagging only the start, and never
> bunch them all at the beginning. For example: 今天全搞砸了 [sighs]
> 不想说了…… [giggles] 骗你的啦，你怎么当真了 — or: wait [gasp] you actually
> did it? [laughs] no way. Commonly supported tags: [amazed], [crying],
> [curious], [excited], [sighs], [gasp], [giggles], [laughs], [mischievously],
> [panicked], [sarcastic], [serious], [shouting], [tired], [whispers]. You are
> not limited to this list — you may use other short emotion or action tags in
> the same bracket form when they suit the delivery. Write tags in English
> even when speaking another language. Everything outside the brackets is
> spoken aloud, so keep it natural and short.

## Testing

- The #152 composition tests (`resolve_voice` four-state matrix) reference
  the constant and follow automatically — no edits expected unless one pins
  addendum substrings.
- New/updated content assertions on `AUDIO_TAGS_ADDENDUM`:
  - contains the density phrase (`two to four`);
  - does **not** contain `sparingly` (regression guard against the old
    minimizing instruction);
  - contains both interspersal examples (pin one distinctive substring each,
    e.g. `[sighs] 不想说了` and `[gasp] you actually did it`);
  - still contains the unchanged clauses (tag list spot-check, `Write tags
    in English`, `spoken aloud`).
- Voice-path passthrough tests (tags stream/persist verbatim) are untouched —
  no behavior change.

Standard pre-PR gate: fmt / clippy / workspace tests / openapi (no drift
expected).

## Rollout

Constant-text change; rides the same unreleased dev train as #161/#162. Prod
voice (grok-4.20, `tts_audio_tags = true`, no custom voice filter_prompt
override of the addendum path) picks it up on the next engine image — no
downstream config change needed. If grok still under-delivers after this,
the next lever is a custom `filter_prompt` downstream (already supported),
not more engine code.

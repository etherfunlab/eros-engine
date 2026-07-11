# Voice channel: inline TTS audio tags (`tts_audio_tags`)

**Date:** 2026-07-11
**Status:** Design approved, ready for implementation plan
**Related:** `docs/superpowers/specs/2026-07-07-voice-call-parts-design.md` (the voice channel this extends)

## Summary

Add an opt-in `[tasks.chat_voice].tts_audio_tags` boolean that makes the voice
channel invite the model to emit **inline audio tags** — short bracketed cues
like `[laughs]`, `[whispers]`, `[excited]` — that Gemini's TTS models
(Gemini 3.1 Flash TTS Preview and the 2.5 Flash/Pro Preview TTS models)
interpret as delivery/emotion directions.

Reference for the tag mechanism:
<https://ai.google.dev/gemini-api/docs/speech-generation#transcript-tags>.
Gemini lists ~15 "commonly used" tags and explicitly states there is no
exhaustive list, encouraging improvisation; the same tags are used even for
non-English transcripts.

This is a **pure prompt-directive change**. The engine only swaps the voice
directive it appends to the persona prompt. The bracketed markup then flows
through the voice path **verbatim** — streamed to the client, persisted, and
replayed as history exactly as generated — because the voice path already does
no post-processing. The client is responsible for feeding the text to a
TTS model that understands the tags.

## Background: how the voice directive works today

The voice turn (`crates/eros-engine-server/src/pipeline/voice.rs`) assembles a
deliberately thin prompt via `build_voice_prompt(genome, directive, affinity)`:
persona `system_prompt` + a **directive** + one optional relationship line. The
`directive` is the only knob that shapes *how* the model speaks.

`ResolvedVoice.directive` is produced by `ModelConfig::resolve_voice()`
(`crates/eros-engine-llm/src/model_config.rs`):

- custom `[tasks.chat_voice].filter_prompt` (when non-blank), else
- the built-in `DEFAULT_VOICE_DIRECTIVE`.

Crucially, `DEFAULT_VOICE_DIRECTIVE` today **forbids brackets**:

> "Do not use markdown, lists, emoji, asterisks, or bracketed stage directions:
> everything you write is read aloud verbatim by a text-to-speech voice, so
> write only words meant to be spoken."

So "support audio tags" is fundamentally about replacing that no-brackets
instruction with one that *invites* inline `[tag]` markup.

The voice path streams and persists the assistant text **verbatim** — it has no
`output_regex` and no post-process step (those are `chat_companion`-only). So
bracketed tags require **no** new plumbing to pass through.

## Requirements

1. A new opt-in boolean `tts_audio_tags` under `[tasks.chat_voice]`, default
   **off** (absent ⇒ today's behaviour, byte-for-byte).
2. When **on**, the effective voice directive invites inline audio tags:
   explains the `[tag]` syntax, lists the commonly-used tags, and explicitly
   permits the model to improvise other tags.
3. When on **and** a custom `filter_prompt` is set, keep the operator's prose
   verbatim and **append** the audio-tag guidance to it.
4. No parsing, stripping, validation, or transformation of the emitted tags.
   Verbatim everywhere (stream, DB, history replay).
5. No changes to the wire protocol, HTTP surface, DB schema, or OpenAPI spec.

### Non-requirements (explicitly out of scope)

- **Tag stripping / dual-text tracking.** Decided against: the stored transcript
  and history replay keep the tags. (If a clean transcript is ever wanted, that
  is a separate feature, likely a frontend concern.)
- **Overriding the persona `system_prompt`.** If a deployment's persona prompt
  itself says "don't use brackets", that is operator content and this toggle
  does not touch it.
- **Transcript display.** How a frontend renders a literal `[laughs]` is a
  presentation concern outside the OSS engine (engine manages chat, not
  presentation).
- **Validating that the configured voice model is actually a Gemini TTS model.**
  The toggle only shapes the prompt; which model consumes the tags is the
  operator's responsibility.

## Design

### Config surface

Add one field to `TaskConfig`
(`crates/eros-engine-llm/src/model_config.rs`), read only by `resolve_voice`:

```rust
/// chat_voice-only: opt into inline TTS audio tags (Gemini transcript tags
/// like `[laughs]`, `[whispers]`). Absent/`false` ⇒ the built-in directive
/// keeps forbidding brackets (today's behaviour). `true` ⇒ the voice directive
/// invites inline `[tag]` markup; the emitted tags flow through verbatim (no
/// engine-side parsing/stripping). See
/// docs/superpowers/specs/2026-07-11-voice-tts-audio-tags-design.md.
#[serde(default)]
pub tts_audio_tags: Option<bool>,
```

`Option<bool>` matches the file's convention for opt-in booleans (`output_filter`,
`ghosting`, `structured_output`). `resolve_voice` treats `None` and `Some(false)`
identically (off).

### Two built-in text constants

Define the tag guidance **once** and reuse it, so the tag list lives in a single
place:

```rust
/// Appended to the effective voice directive when `tts_audio_tags` is on. Names
/// the inline-tag syntax, the commonly-supported tags, and explicit permission
/// to improvise. Kept identity-free (no product/brand). Used both to build the
/// audio-tags default directive and to augment a custom `filter_prompt`.
pub const AUDIO_TAGS_ADDENDUM: &str = "You may add inline audio tags to make your speech more expressive. An audio tag is a short cue in square brackets placed right before the words it affects — e.g. [laughs] that's so funny, or [whispers] come closer. Use them sparingly, only when they fit the moment. Commonly supported tags: [amazed], [crying], [curious], [excited], [sighs], [gasp], [giggles], [laughs], [mischievously], [panicked], [sarcastic], [serious], [shouting], [tired], [whispers]. You are not limited to this list — you may use other short emotion or action tags in the same bracket form when they suit the delivery. Write tags in English even when speaking another language. Everything outside the brackets is spoken aloud, so keep it natural and short.";
```

The audio-tags default directive is a **bracket-neutral** speech base (the same
"live voice call / natural / short / no markdown-lists-emoji" spirit as
`DEFAULT_VOICE_DIRECTIVE`, but with the no-brackets sentence removed) followed by
the addendum. Build it by concatenation so the addendum text is not duplicated:

```rust
/// Voice directive used when `tts_audio_tags` is on and no custom
/// `filter_prompt` is set. Same plain-speech guidance as
/// `DEFAULT_VOICE_DIRECTIVE`, minus the no-brackets clause, plus
/// `AUDIO_TAGS_ADDENDUM`.
pub const DEFAULT_VOICE_DIRECTIVE_AUDIO_TAGS: &str = concat!(
    "You are on a live voice call. Speak the way people talk out loud. Keep replies short — usually one or two sentences. Do not use markdown, lists, or emoji — everything you write is read aloud by a text-to-speech voice.",
    "\n\n",
    // AUDIO_TAGS_ADDENDUM — inlined here because `concat!` needs literals; keep
    // in sync with the const above (asserted by a unit test).
    "You may add inline audio tags to make your speech more expressive. …"
);
```

> Implementation note: `concat!` requires string literals, so the addendum text
> is physically repeated inside `DEFAULT_VOICE_DIRECTIVE_AUDIO_TAGS`. To avoid a
> silent drift between the two copies, add a unit test asserting
> `DEFAULT_VOICE_DIRECTIVE_AUDIO_TAGS.ends_with(AUDIO_TAGS_ADDENDUM)`. (If a
> preferred alternative avoids the duplication — e.g. building the default at
> resolve time with `format!` from a shared base + `AUDIO_TAGS_ADDENDUM` — that
> is acceptable; the requirement is that the tag list is authored once.)

`DEFAULT_VOICE_DIRECTIVE` is left **unchanged** (toggle-off path, and it is a
public const referenced by existing tests).

### Resolution logic (`resolve_voice`)

The directive is chosen from a 2×2 over (has non-blank `filter_prompt`?) ×
(`tts_audio_tags` on?):

| | audio tags off | audio tags on |
|---|---|---|
| **no / blank `filter_prompt`** | `DEFAULT_VOICE_DIRECTIVE` (today) | `DEFAULT_VOICE_DIRECTIVE_AUDIO_TAGS` |
| **custom `filter_prompt`** | custom (today) | custom + `"\n\n"` + `AUDIO_TAGS_ADDENDUM` |

Sketch:

```rust
pub fn resolve_voice(&self) -> Option<ResolvedVoice> {
    const VOICE_TASK: &str = "chat_voice";
    let task_cfg = self.tasks.get(VOICE_TASK)?;
    let audio_tags = task_cfg.tts_audio_tags.unwrap_or(false);

    let custom = task_cfg
        .filter_prompt
        .clone()
        .filter(|s| !s.trim().is_empty());

    let directive = match (custom, audio_tags) {
        (Some(c), true) => format!("{c}\n\n{AUDIO_TAGS_ADDENDUM}"),
        (Some(c), false) => c,
        (None, true) => DEFAULT_VOICE_DIRECTIVE_AUDIO_TAGS.to_string(),
        (None, false) => DEFAULT_VOICE_DIRECTIVE.to_string(),
    };

    let m = self.resolve(VOICE_TASK, None);
    Some(ResolvedVoice {
        model: m.model,
        fallback_model: m.fallback_model,
        temperature: m.temperature,
        max_tokens: m.max_tokens,
        reasoning: m.reasoning,
        directive,
    })
}
```

`ResolvedVoice`, `build_voice_prompt`, `run_voice_turn`, and the route are
**unchanged** — they carry an opaque `directive` string end-to-end.
`validate_voice_model` is unaffected (a bool needs no validation).

### Data flow (unchanged from today)

1. Route resolves `ResolvedVoice` (now possibly with the audio-tags directive).
2. `build_voice_prompt` appends the directive to the persona system prompt.
3. Model streams reply text (may contain `[tag]` markup).
4. `delta` frames carry the text verbatim → client → client's TTS.
5. Assistant row persisted verbatim; future turns replay it verbatim as history.

## Alternatives considered

- **Multi-shape `tts_audio_tags` (bool *or* custom string).** Let `true` use the
  built-in addendum and a string supply bespoke tag prose (the `DisplayOverride`
  pattern). Rejected as YAGNI: an operator wanting fully custom tag text can put
  it in `filter_prompt` and leave the toggle off.
- **Strip tags from storage/history, keep only in the live stream.** Rejected in
  design discussion — adds dual-text tracking for a clean-transcript benefit that
  is really a frontend concern.
- **Edit `DEFAULT_VOICE_DIRECTIVE` in place to be bracket-neutral and always
  append guidance.** Rejected: changes today's default (toggle-off) behaviour and
  the public const other tests depend on.

## Testing

Unit tests in `model_config.rs` covering the 2×2 plus the drift guard:

1. **default + off** → `resolve_voice().directive == DEFAULT_VOICE_DIRECTIVE`
   (today's behaviour; regression guard).
2. **default + on** (`tts_audio_tags = true`, no `filter_prompt`) → directive
   equals `DEFAULT_VOICE_DIRECTIVE_AUDIO_TAGS`; contains `[laughs]`; does **not**
   contain "bracketed stage directions".
3. **custom + on** → directive starts with the custom prose and ends with
   `AUDIO_TAGS_ADDENDUM` (contains both the custom text and `[whispers]`).
4. **custom + off** → directive equals the custom prose, no tag guidance.
5. **drift guard** → `DEFAULT_VOICE_DIRECTIVE_AUDIO_TAGS.ends_with(AUDIO_TAGS_ADDENDUM)`.

The existing voice-pipeline and route tests already exercise the plumbing that
carries `directive` through; no changes needed there beyond confirming they
still pass.

## Documentation

Update the commented `[tasks.chat_voice]` block in `examples/model_config.toml`
(currently lines ~413–433) to document `tts_audio_tags`, e.g.:

```toml
# `tts_audio_tags` (OPTIONAL, default false): when true, the voice directive
# invites inline audio tags — bracketed cues like [laughs], [whispers] that
# Gemini TTS models interpret as delivery/emotion. The tags pass through
# verbatim (streamed, persisted, replayed); the client must feed the text to a
# TTS model that understands them. Works with a custom filter_prompt too (the
# tag guidance is appended to it).
#tts_audio_tags = true
```

No README or OpenAPI changes (server-side config only; no API surface change).

## Files touched

- `crates/eros-engine-llm/src/model_config.rs` — add `TaskConfig.tts_audio_tags`;
  add `AUDIO_TAGS_ADDENDUM` + `DEFAULT_VOICE_DIRECTIVE_AUDIO_TAGS`; update
  `resolve_voice`; add unit tests.
- `examples/model_config.toml` — document the new field.

No other crates, routes, migrations, or generated artifacts are affected.

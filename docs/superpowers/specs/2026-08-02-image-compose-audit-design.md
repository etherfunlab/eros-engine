# eros-engine — compose-call audit fields on the delegated image marker

Follow-up to `2026-07-31-image-prompt-compose-variants-design.md`. That spec's
`metadata.image` marker deliberately excluded the composer's model, generation
id, and the composed wire prompt. This PR reverses the exclusion for **three
audit fields only** — the prompt-variant actually used, the composer model, and
the compose call's generation id — so a stored image turn can be reconciled
against provider logs without replaying the stream. The composed prompt itself
stays out: storing it is the consumer's job (the reference deployment already
does), and the seed subject is already double-recorded
(`companion_decision_events.payload.image_prompt` and `metadata.image.prompt`).

---

## 0. Decisions (settled during brainstorm)

- **Three fields, nothing else.** `compose_variant`, `compose_model`,
  `compose_generation_id`. No usage/cost (reconcile via generation id against
  the OpenRouter log, the `chat_vision` precedent) and no composed prompt
  (consumer-side by design).
- **Success-only writes; absence = no successful compose.** `raw` skip, a
  fail-open degradation (error/timeout/empty → seed passthrough), and a missing
  `[tasks.chat_image_prompt_compose]` block all leave the marker exactly as
  today. No `compose_skip_reason` — same NULL semantics as the affinity-events
  audit trio.
- **`compose_variant` is written only when a Keyed/Indexed key actually
  selected the prompt** (`"b"`, `"0"`). `Plain` and the built-in fallback have
  a single prompt — there is no "which variant" to answer — so the key is
  absent; a requested-but-missed variant already warns via
  `compose_variant_log_event`.
- **Fields live inside the existing `metadata.image` marker**, not a new
  top-level key. Both persistence paths (insert and merge) then carry them for
  free — zero new store API. Naming follows the `vision_model` /
  `vision_generation_id` style.

---

## 1. Data shape

On a delegated image turn whose compose call succeeded:

```json
{
  "image": {
    "prompt": "<PDE seed subject>",
    "aspect_ratio": "3:4",
    "compose_variant": "b",
    "compose_model": "moonshotai/kimi-k2",
    "compose_generation_id": "gen-abc123"
  }
}
```

| key | value | absent when |
|---|---|---|
| `compose_model` | model that answered the compose call: `resp.model`, falling back to the attempted model id (mirrors `VisionOutcome`, `stream.rs:2077`) | no successful compose this turn |
| `compose_generation_id` | the compose call's generation id verbatim | provider returned none, or no successful compose |
| `compose_variant` | the Keyed key / Indexed index that selected the prompt | `Plain` spec, built-in fallback, or no successful compose |

`prompt` (seed subject) and `aspect_ratio` are unchanged.

---

## 2. Changes

All in `eros-engine-server` + `eros-engine-llm`; no store or migration changes.

| Site (pre-PR) | Change |
|---|---|
| `ResolvedImagePromptCompose`, `model_config.rs:1152` | new `variant_key: Option<String>`, set by `resolve_image_prompt_compose` (`:1882`) when `PromptSpec::select` (`:204`) hits a Keyed/Indexed entry; `None` for `Plain`/built-in |
| `run_image_prompt_compose`, `stream.rs:2291` | returns `Option<ComposeOutcome>` instead of `Option<String>`: `{ text, model, generation_id: Option<String>, variant: Option<String> }`, populated at the success exit (`:2346-2352`) where `resp.model`/`resp.generation_id` are currently dropped |
| `DelegatedImagePrompt`, `stream.rs:2407` | three new `Option` fields; the fail-open seed path in `build_delegated_image_prompt` (`:2424`) fills `None` |
| `build_delegated_image_marker`, `stream.rs:166` | takes the audit values, writes keys only when `Some`; doc comment rewritten — the deliberate-exclusion list shrinks to: composed prompt, url, draw outcome |
| `stream.rs:3253` (ReplyImage insert), `stream.rs:3576` (ReplyTextImage merge) | untouched — the enlarged marker rides `AssistantInsert.metadata` and `merge_assistant_image_meta` as-is |
| `assistant_transcript_line`, `stream.rs:2118` | untouched — reads only `prompt` / `aspect_ratio` |

## 3. Error handling

No new failure surface. Success-only semantics means the only new code on
error paths is `None`-filling. Persistence failures keep the existing
warn-and-continue behavior of `insert_assistant_batch` / merge.

## 4. Tests

- Unit: `build_delegated_image_marker` with and without audit values (key
  presence/absence, no `null`s).
- Unit: `resolve_image_prompt_compose` `variant_key` across Keyed hit, Indexed
  hit, miss→built-in, `Plain`.
- Integration (existing image-turn sqlx tests): after a successful compose,
  `metadata.image` contains the three keys; with `prompt_variant = "raw"` it
  contains none of them.

## 5. Docs

- `docs/api-reference.md:303` — extend the `metadata.image` shape table.
- `docs/model-config.md` — one paragraph in the
  `chat_image_prompt_compose` section: what is audited, absence semantics.

## 6. Out of scope

Composed-prompt persistence (consumer-side by design), usage/cost columns,
`companion_decision_events` changes, version/release timing (maintainer's
call at release time).

# eros-engine — engine-counted image facts, composer-authored captions, seedless generate-mode composer

Three changes to the PDE image path, one PR. The engine becomes the single
source of truth for "how many images went out recently", so the judge stops
counting transcript markers it cannot count reliably. The composer starts
emitting a short `caption` alongside the image prompt, and every
history-facing render switches to it — separating "what the picture showed"
(short, for looking back) from "what to draw" (long, for the image provider).
And the judge stops writing image-prompt seeds entirely:
`chat_image_prompt_compose` flips from seed-EXPAND to context-GENERATE,
becomes a required stage of the image path, and runs concurrently with the
chat call on `reply_text_image`.

Closes #212. Background: eros-audit report 38 and the issue's replay bench
(n=32 turns/arm, production traffic): engine-counted facts rescued the judges
that cannot count (venice 8/10 → 3/10 over-send with ≥2 images in window);
judges soften explicit user requests when asked to write seeds (hermes 7/12,
glm 8/11 softened); 0/32 verdicts leaked image descriptions once the schema
stopped asking; and image-prompt echo was 71% of the judge's
conversation-history characters from only 12.8% of turns.

---

## 0. Decisions (settled during brainstorm)

- **A + B + C ship together.** B without A would make over-sending worse (the
  issue measured the existing hold-back rule keying on the marker being
  *visible*), so the facts line lands in the same change that shortens what
  the markers carry.
- **No backwards compatibility with the old PDE contract.** The verdict schema
  drops `image_prompt` unconditionally. A deployment that has not configured
  `[tasks.chat_image_prompt_compose]` loses image capability: the judge is
  told 可发图=否 and image actions are guarded away. **Breaking change**,
  flagged in release notes and the shipped config example. Rationale:
  production already proved the judge-written seed unusable (especially on
  NSFW turns — acceptable image output essentially always required the
  composer anyway), so making the composer mandatory removes the wasted double
  composition rather than preserving it as a degraded fallback. It also
  single-sources the audit trail: with one composition there is no longer a
  "was it the seed or the composer?" question to investigate, and a downstream
  operator controls image content through exactly one knob — the composer's
  `filter_prompt`.
- **The composer emits `{prompt, caption}`, not plain text.** This reverses the
  earlier "composer output stays plain text" position. The reason that
  position existed — not wanting to break deployed composer `filter_prompt`s —
  is already spent: the EXPAND→GENERATE rewrite obliges every deployment to
  rewrite that prompt regardless, so a second output field costs nothing
  extra. Producing the caption here is also the only place it can be produced
  without either an extra LLM call or a lossy after-the-fact summarisation:
  the model writing the picture already knows what the picture shows.
- **The engine does NOT persist the composed (wire) prompt.** It goes out on
  the `image_request` frame; whether to store it is the downstream consumer's
  call (ours keeps it in its own images table). The engine persists only what
  its own chat pipeline needs to read back.
- **`aspect_ratio` and `image_ref` stay judge-owned.** The measured harm was
  all in the seed (softening + double composition); the two enums are cheap
  decisions the judge handles fine. They are also *not* moved into the
  composer's JSON — a picture's framing is a turn-level interaction decision,
  not part of describing the picture.
- **The seed concept is deleted outright, not just relocated.** With the judge
  no longer writing one, the only other seed source was the client
  (`image.image_prompt`) — and clients neither need nor are permitted to hand
  the engine a prompt. So that request field goes too, the composer payload
  loses its `[画面主题种子]` section, and `resolve_image_turn_inputs` stops
  resolving a subject at all. The composer generates from context, full stop.
- **`"raw"` stops being a reserved `prompt_variant`.** It existed to mean "skip
  the composer and draw the seed verbatim", which is incoherent once no seed
  exists. After this change `"raw"` is an ordinary variant name: it hits only
  if a deployment actually configures a variant keyed `raw`, and misses like
  any other unknown key. The boot gate that refused such a key is removed with
  it.
- **`reply_text_image` must fan out**: composer and chat fire concurrently and
  assemble at the end. Today they are serial (the composer starts only after
  the text stream's `done`), adding a full LLM round trip to the turn.
- **`ReasoningConfig.effort` is a non-goal**: provider-specific reasoning
  params are already coverable via `[[providers.<name>.body]]` custom body
  params (#213).

## 1. A — engine-counted image facts in the judge context

`build_input_filter_transcript` (`stream.rs:2158`) currently returns `String`.
It becomes a struct:

```rust
struct JudgeTranscript {
    transcript: String,
    /// Assistant rows in the window carrying `metadata.image`
    /// (channel-marked rows already excluded by the existing skip).
    images_in_window: usize,
    /// The newest assistant row in the window is an image turn.
    last_assistant_is_image: bool,
}
```

Both facts are computed from the same `history()` rows the transcript is
already built from — zero extra round trips.

`build_pde_ctx` (`stream.rs:1886`) gains the two facts and always emits, right
after `[图片能力]` (the negative is a signal too, same precedent):

```
[近期图片] 最近{INPUT_FILTER_CONTEXT_TURNS}条消息内已发图={k} 张；上一条 AI 消息是图片={是|否}（以本行计数为准，对话记录里的图片标记仅供参考）
```

**The unit is messages, not turns.** `ChatRepo::history(session_id, limit,
offset)` applies `LIMIT` to rows (`chat.rs:244`), so
`INPUT_FILTER_CONTEXT_TURNS = 8` is the last 8 *messages* — roughly 4
exchanges. The constant's name is a pre-existing misnomer; renaming it is out
of scope here, but the rendered line must not repeat the error, and a comment
at the constant should record it. The window size is deliberately unchanged:
the issue's bench replayed `build_pde_ctx` and therefore inherited this exact
8-row window, so its measured numbers transfer only as long as the window
does.

The trailing clause is the engine-side half of "these numbers override the
transcript"; judge prompts (config-owned) can lean on it but don't have to.

## 2. B — history-facing renders read `caption`

Two renders currently read `metadata.image.prompt`:

- `assistant_transcript_line` (`stream.rs:2135`) → the PDE judge and the input
  filter, as `（发送了一张图片：{prompt}，画幅 {ar}）`.
- `model_facing_assistant_text` (`handlers.rs:116`) → **the chat model's own
  conversation history**, as `[你给对方发送了一张照片：{prompt}]`, untruncated.

Both switch to `metadata.image.caption`. That is the whole of change B — there
is no truncation step. A caption is short because it was written to be short,
so the 71%-of-history-characters problem is fixed at the point of production
rather than by cutting a long string at a fixed offset. Truncation was also
never safe here: under §3 the composer's `prompt` leads with the style preset
and appearance (`compose_image_prompt`, `handlers.rs:277`, joins
`{style_preset}\n{appearance}\n{subject}`), so a fixed-length head of it is
boilerplate identical across every image of every persona in that style.

The aspect-ratio clause in the transcript render stays (it is ~8 chars and
occasionally decision-relevant).

**When `caption` is absent, both renders emit the bare marker** —
`（发送了一张图片）` / `[你给对方发送了一张照片]` — and never fall back to
`prompt`. Three situations reach this path, and the same rule covers all
three: rows persisted before this change; a composer reply that parsed but
carried no usable caption; and a fully-failed compose (§4). Falling back to
`prompt` would reintroduce exactly the long-string injection this change
removes, silently and precisely when the composer is misbehaving. The cost is
that for those turns the judge cannot see *what* the last picture showed —
acceptable, because the anti-spam brake now rides on §1's counts, which are
computed from `metadata.image` presence and are unaffected by a missing
caption.

## 3. C — seedless verdict, generate-mode composer with captions

### Verdict schema

`pde_response_format` (`stream.rs:1610`) drops `image_prompt` from
`properties` and `required`, unconditionally. `PdeVerdict` drops the field;
`aspect_ratio` and `image_ref` stay. Judge prompts that still mention seeds
have nothing to write them into (strict schema), and a stray `image_prompt`
key from a non-strict provider deserializes away harmlessly.

### Composer becomes required for image capability

The effective image availability for a turn becomes
`image_executor_available && [tasks.chat_image_prompt_compose] exists`. The
gate is the **task section's presence** (a config-level fact), NOT a
`resolve_image_prompt_compose(..)` call: that resolver reaches
`self.resolve(COMPOSE_TASK, None)`, which advances the round-robin model
cursor as a side effect, so calling it merely to answer a capability question
would skew which model later image turns actually pick.

Without the composer task, `[图片能力] 本轮可发图=否` and `guard_action`
downgrades image verdicts exactly as it does today when the executor is
absent.

### `"raw"` is no longer a reserved variant

`resolve_image_prompt_compose` currently short-circuits to `None` when the
client's `prompt_variant` is `"raw"` (case-insensitive), meaning "skip the
composer, draw the seed as-is"; `check_variant_shape` correspondingly refuses
to boot on a config that defines a variant keyed `raw`. Both go away.

With no seed there is nothing to draw as-is, so the escape hatch is
incoherent. After this change `"raw"` is an ordinary variant name: it selects
a prompt only if a deployment actually configures one under that key, and
otherwise misses like any unknown key — falling back to the built-in prompt,
which is never an error. The boot refusal is deleted so that key becomes
configurable.

Removing the short-circuit also makes `resolve_image_prompt_compose` return
`None` for exactly one reason — the task section is absent — which is what
lets the capability gate above be stated so simply.

**No `prompt_variant` supplied** already resolves to the built-in prompt for
the variant shapes (`PromptSpec::select` returns `None` for `Indexed`/`Keyed`
without a variant), and to the configured string for a `Plain` `filter_prompt`
— a plain prompt is the deployment's single chosen prompt, not a variant miss.
That behaviour is unchanged and needs no code.

### Composer contract: EXPAND → GENERATE, and a second output field

`compose_user_payload` (`stream.rs:2294`) gains the partner's latest message —
the information the seed used to carry (the shared transcript deliberately
excludes the current turn) — and **loses the seed section entirely**:

```
[人物外观]\n{appearance}\n\n[最近场景]\n{recent_scene}\n\n[对方最新消息]\n{latest_user_msg}\n\n[风格]\n{style}\n\n[画幅]\n{aspect_ratio}
```

`resolve_image_turn_inputs` stops resolving a subject at all: its
`plan.image_prompt` arm goes with the judge's seed, and its
`req_image.image_prompt` arm goes with the request field (below). What remains
is style and aspect ratio.

`ImageReplyParams.image_prompt` is **removed from the request type**, and with
it from `openapi.json` and both API references. It was documented as the
subject for the forced-image path, but a client neither needs nor is permitted
to hand the engine an image prompt — the composer decides what the picture
shows. A client that keeps sending the field is unaffected: it deserializes
away like any unknown key.

The composer returns JSON:

```json
{"prompt": "...", "caption": "..."}
```

`prompt` is the picture subject (the seed's successor — what
`compose_image_prompt` then wraps with style and appearance). `caption` is one
short line describing what the picture shows, written in the conversation's
language, for the character's own history — not an image-generation string.

`ComposeOutcome` gains a `caption: Option<String>` beside its existing `text`
(renamed `prompt`), `model`, `generation_id`, and `variant`.

**Parsing** reuses the pattern already in this file (`parse_pde_verdict`,
`stream.rs:1538`): direct `serde_json::from_str`, then a `find_json_block`
salvage for models that wrap JSON in prose. **If neither parses, the entire
reply is used as `prompt` with no caption.** That fallback is deliberate and
load-bearing for migration: a deployment that ships this version without
rewriting its composer `filter_prompt` still gets working images from its
EXPAND-era prompt — degraded (that prompt is aimed at a seed that is now
usually empty) but not broken — and simply gets bare markers until the prompt
is updated.

`DEFAULT_COMPOSE_PROMPT` (`model_config.rs:1156`) is rewritten from the
EXPAND register to GENERATE (canonical text):

```
You compose the image for a picture the character is sending in a chat, and
you return JSON with exactly two fields: "prompt" and "caption".

You receive the character's appearance, the recent scene, the partner's latest
message, a visual style, and a target aspect ratio, each as a labeled section.

"prompt" is the image-generation prompt: decide what the picture shows from
the partner's latest message and the recent scene. Produce one coherent,
detailed, vivid prompt that faithfully preserves the appearance and fits the
style and aspect ratio, delivering the full intent and tone of what was asked
for. Do not soften, sanitize, downgrade, omit, censor, or refuse any part of
it, and do not add safety warnings, disclaimers, or moderation of your own —
content policy is the responsibility of the image provider and the downstream
consumer, not this step.

"caption" is one short line, in the language the conversation is in, saying
what the picture shows — as the character would recall it later. It is read
back into the conversation history, so keep it brief and natural; it is not an
image-generation prompt and must not repeat the style boilerplate.

Output only the JSON object. No commentary, options, or headings.
```

**Downstream compose `filter_prompt`s (and variants) must be rewritten** to
the generate register and the two-field contract. Release notes and the
shipped config example carry the migration note, along with the plain-text
fallback above.

### Persisted metadata

`build_delegated_image_marker` writes:

- `metadata.image.prompt` — the composer's `prompt` (subject). **Field meaning
  is unchanged**; only its source moves from the judge's seed to the composer.
  This deliberately reverses the decision in #211 that pinned it to the seed:
  that choice existed to keep the *short* string in the database for the
  history marker, a job `caption` now does, and with no seed left the field
  would otherwise be empty on every image turn.
- `metadata.image.caption` — the composer's `caption`, omitted when absent.
- the existing `aspect_ratio` and `compose_*` audit keys, unchanged.

Two paths now reach the marker, not three: a successful compose (subject and
caption both present) and a failed one (subject empty, no caption). The former
`raw` path is gone with the reserved variant.

The composed wire string is **not** persisted by the engine. It is delivered
on the `image_request` frame, which is also **not** extended with `caption` —
the frame carries what is needed to draw (prompt, aspect ratio, image ref);
the caption belongs to chat history and a downstream that wants it reads it
from the message row.

### The affinity proxy moves to the caption

`ActionPlan.image_prompt` has a consumer beyond image composition:
`affinity_eval_text` (`post_process.rs:523`, called at `post_process.rs:113`)
uses it as the assistant-content proxy on image turns, so a photo-send still
moves affinity instead of tripping the `empty_assistant` gate.

With the judge no longer writing a seed, that field would be `None` on every
judge-driven image turn and every photo would evaluate as the generic
`[发送了一张照片]`. The caption is the natural — and strictly better —
replacement: a short natural-language line, in the conversation's language,
describing what the picture showed, which is exactly what the first-person
affinity evaluator (#210) wants to read.

So:

- `ActionPlan.image_prompt` is **renamed** `image_caption`, redocumented as
  "what the picture showed, for post-process affinity", and `plan_for` loses
  its `image_prompt` parameter (nothing can supply a caption at decision time —
  the composer has not run yet).
- The stream sets `plan_bg.image_caption` after the composer resolves, on both
  image paths, at the point it already mutates the plan before spawning
  post-process (`stream.rs:3675`).
- `affinity_eval_text` reads `image_caption`. Its existing blank/absent
  behaviour is unchanged: it falls back to `[发送了一张照片]`, which is exactly
  §2's bare-marker rule expressed for the affinity path.

Note this makes the plan's field genuinely unused for composition —
`resolve_image_turn_inputs` reads `req_image.image_prompt` directly, and the
forced-image path was already passing that same value through the plan.

### Concurrency on `reply_text_image`

Today the composer runs only after the text stream's `done`
(`stream.rs:3618`), serially. The new trigger point is **after the input-filter
block and before `build_reply_request`** (`stream.rs:3494`).

That point is chosen for two reasons. The input filter is what turns
`user_msg.content` into the text the model actually sees, so triggering after
it means the composer and the chat model work from the same text and the
picture cannot drift from the reply. And the remaining overlap is already
ample: `build_reply_request` alone does a 20-row history fetch, a Voyage
embedding call, and memory/world recall before the chat stream even starts, so
one short composer call hides completely underneath without having to reach
back to the raw input.

The composer's inputs are captured as owned values at the trigger point
(`tokio::spawn` requires `'static + Send`; `AppState` and `CompanionPersona`
are both `Clone`):

```rust
struct ComposeJob {
    state: AppState,
    persona: CompanionPersona,
    recent_scene: String,          // pde_transcript
    latest_user_msg: String,       // the EFFECTIVE text (see below)
    seed_subject: Option<String>,  // req_image.image_prompt only
    style: StyleKey,
    aspect_ratio: Option<String>,
    variant: Option<String>,       // req_image.prompt_variant
    cfg: ResolvedImagePromptCompose,
}
```

`latest_user_msg` must be tracked locally through the input-filter block: the
rewrite is currently persisted and then dropped (`build_reply_request` re-reads
it from the database), so the block keeps a local `String` — initialised from
`user_msg.content`, replaced when `run_input_filter` returns a rewrite — rather
than paying a second read.

Spawn only when `plan.action_type == ReplyTextImage`. `ReplyImage` keeps its
single sequential call: it has no text task to overlap with and returns early
via `image_only_done`.

The `Option<JoinHandle<DelegatedImagePrompt>>` is held across the chat
stream's yields. At `stream.rs:3618` the `build_delegated_image_prompt(..).await`
call becomes `handle.await`; every line after it — marker merge, image frame,
final frame — is unchanged, so the wire frame order is byte-identical:

```
meta → delta* → done → image → final
        ↑                ↑
   chat streaming,    join here (usually already complete)
   composer running
```

Serial LLM hops on the turn drop from 3 to 2.

## 4. Error handling

- A and B are pure rendering; no new failure paths. A DB error still yields an
  empty transcript — and now zero counts, rendering as `已发图=0 张`, the same
  "no recent images" signal an empty transcript gives the judge today.
- The composer keeps its fail-open chain (per-model timeout → next model). With
  the seed concept deleted there is nothing to fall back to, so a fully-failed
  compose degrades to an empty subject: `compose_image_prompt(style, persona,
  "")` yields a persona-appearance portrait prompt, and no caption is persisted
  (bare marker, §2). Logged at warn. The degraded *outcome* changes from "the
  judge's softened seed" to "generic portrait"; the failure *rate* is unchanged
  (same chain). This is now the ONLY degraded image path — a turn either gets a
  composed picture or a portrait.
- A panicked or cancelled composer task surfaces at the join as
  `Err(JoinError)`. **Implemented differently from this section's original
  plan** (amended post-review, 2026-08-02 fix wave): rather than mapping
  straight to the portrait-fallback degradation above, the join's `Err(e)` arm
  logs a warn and re-runs `build_delegated_image_prompt` **sequentially**,
  right there — a second, synchronous attempt at a real composed picture
  before falling through to the empty-subject/portrait path (only if that
  second attempt also fails to produce one). This is strictly more robust than
  "maps to the same degradation": a `JoinError` is usually an orthogonal
  scheduler event (a panic, or an abort racing a not-yet-observed success), not
  evidence that the composer itself is broken, so unconditionally discarding
  its chance at a real picture would degrade turns that a plain retry could
  have saved — never a dropped frame either way. The cost is a second LLM call
  and a serial hop on this (rare) path. The reviewer that closed issue #212
  judged this behavior preferable to the original plan, so this document was
  amended to describe it rather than changing the code to match the original
  plan.
- If the text path returns early before the join (e.g. `build_reply_request`
  fails and the stream yields `Error` and returns), the handle is dropped.
  **Dropping a `JoinHandle` does not cancel the task**: it runs to completion
  and its result is discarded. The request is already in flight, so aborting
  saves no tokens; calling `handle.abort()` on those explicit error paths is
  tidiness, not correctness.

## 5. Testing

- Count computation: image-metadata rows counted, channel-marked rows
  excluded, current-turn excluded; `last_assistant_is_image` in both
  polarities.
- `[近期图片]` renders in both polarities and always appears when the PDE runs;
  the rendered unit says messages, and the count matches a fixture whose row
  count differs from its turn count.
- Caption rendering: both consumers use `caption` when present; both emit the
  bare marker when it is absent, for each of the three entry points (legacy row
  with only `prompt`, parsed reply without a caption, failed compose). A
  regression test asserts `prompt` is never rendered into either consumer.
- Schema: `image_prompt` absent from `pde_response_format`; verdict parse
  tolerates a stray `image_prompt` key from a non-strict provider.
- Composer parsing: direct JSON; JSON wrapped in prose; and a plain-text reply
  → whole reply becomes `prompt`, caption `None`.
- Image availability: composer task absent ⇒ 可发图=否 and image actions
  guarded to text; present ⇒ 是.
- `"raw"` is no longer reserved: a config defining a variant keyed `raw` boots
  and that variant is selectable; `prompt_variant = "raw"` with no such key
  configured resolves to the built-in prompt (a miss, not a skip and not an
  error); and `resolve_image_prompt_compose` returns `None` only when the task
  section is absent.
- Composer payload includes `[对方最新消息]` and no longer has a seed section.
- Metadata: `prompt` is the composer subject, `caption` present on the compose
  path and omitted on the failure path; no `composed_prompt` key is written;
  the `image_request` frame carries no caption. At least one DB-backed test
  must drive a real JSON composer reply through to a persisted row and assert
  `metadata.image.caption` — the unit tests cover the parser and the marker
  builder, but not the seam between them.
- Affinity proxy: `affinity_eval_text` returns the caption on an image turn
  when one exists, and `[发送了一张照片]` when it does not; `plan_bg.image_caption`
  is populated on both the `reply_image` and `reply_text_image` paths before
  post-process is spawned.
- Concurrency: `latest_user_msg` equals the input filter's rewrite when one
  fired and the raw content when none did; `reply_text_image` sqlx stream test
  asserts frame order `meta → delta* → done → image → final` is unchanged with
  the concurrent composer, and that a composer failure still yields an image
  frame carrying the portrait fallback.
- `reply_image` path unchanged.

## 6. Non-goals

- `ReasoningConfig.effort` (#213 already covers provider reasoning params via
  custom body params).
- Regenerate-on-empty for chat replies (report 38 §10) — separate concern,
  separate issue.
- Moving `aspect_ratio` / `image_ref` into the composer's JSON — rejected in §0.
- Renaming `INPUT_FILTER_CONTEXT_TURNS` or changing the window size.
- Persisting the composed wire prompt engine-side — rejected in §0.
- Downstream model selection and bench re-runs — operator-side.

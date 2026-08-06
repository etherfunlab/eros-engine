# eros-engine — iron rule ⑫: no bracket action blocks in chat replies

Adds one iron rule to the `chat_companion` system prompt forbidding square-bracket
action blocks in the reply, and naming the engine's own history-injection marker
`[你给对方发送了一张照片：…]` as something the model must not echo.

Background: the engine injects that marker into assistant history rows so the
model knows it previously sent an image (`handlers.rs:116`
`model_facing_assistant_text`). Models learn the format from their own context
and emit it as their reply. A deployment whose `output_regex` strips brackets
then strips the whole reply to nothing — a pseudo-ghost with
`fallback_reason="regex_strip"`.

The pollution source is prompt scaffolding the engine writes, not a defect in
any one model. Downstream deployments choose their own chat models and cannot
be told which ones to avoid, so the fix belongs in the engine's prompt.

---

## 0. Decisions (settled during brainstorm)

- **One iron rule, rendered unconditionally.** Not gated on persona, gender,
  or whether the deployment has images enabled. 8 of the 15 measured
  pseudo-ghosts followed a turn where the user never asked for a photo, so any
  condition keyed on user intent misses over half the cases.
- **The rule bans every `[...]` block, not just the photo marker.** Bracketed
  content in this product is almost always instruction-shaped stage direction,
  which reads as machinery and costs the persona its human feel. There is no
  legitimate-bracket case to carve out. A rule naming only the photo marker
  would be trivially routed around (`[递过手机]`, `[举起相机]`) and land in the
  same `\[[^\]]*\]` strip.
- **Round brackets（）are untouched.** Parenthetical action description is
  normal prose in this product and no production regex strips it.
- **The rule does not let the model decide whether to send a photo.** That
  decision belongs to the PDE; the image content belongs to the image-prompt
  composer. Wording that says "reply according to the user's intent about
  photos" would hand back exactly the authority this rule removes. The model is
  told only that photos are not its to send and that words are its whole job.
  Removing the marker from model output breaks no upstream path — see §1.
- **The rule names 「系统」 explicitly**, despite `PERSONA_GUARD`
  (`prompt.rs:60`) telling the model never to mention "a system". Precedent:
  `ANTI_REFUSAL_GUARD` (`prompt.rs:82`) already does this for the same class of
  problem — polluted text in context that self-reinforces — with a measured
  63% → 25% recurrence drop. Naming the polluted string and disowning it is the
  shape that works here. The fourth-wall risk is accepted.
- **`apply_output_regex`'s removed fail-safe stays removed.** An artifact-only
  reply stripping to empty is deliberate (2026-06-29): the empty row is
  persisted and downstream clients decide how to render it. ⑫ reduces how often
  a reply is artifact-only; it does not restore the fail-safe. The two are
  defence in depth, not alternatives.
- **The injected marker keeps its square brackets.** Changing its delimiters
  would cut the pollution off at the source, but production `output_regex`
  configs and downstream `model_config.toml` files are written against the
  bracket convention. That is a breaking change for less benefit than one
  prompt line. The broad `\[[^\]]*\]` pattern this refers to lives in a
  downstream deployment's private `model_config.toml`, not in this repo — the
  example shipped here (`examples/model_config.toml:159-164`) is the narrower,
  anchored `\s*\[你给对方发送了一张照片[：:][^\]]*\]\s*$`. A reader of the OSS
  repo should not go hunting for the broad one.
- **Rejected: making this a third guard constant** next to `PERSONA_GUARD` /
  `ANTI_REFUSAL_GUARD`. The guards render near the top of the prompt; the iron
  rules render last, immediately before `[output]`, which is the stronger
  position for a format constraint in a long context. The `违反即失效` framing
  also applies.
- **⑫ deliberately does not offer verbal accept/decline options.**
  `ANTI_REFUSAL_GUARD` (`prompt.rs:86-87`) already removes that authority —
  「对方要照片/图片时…不需要你用文字答应或拒绝」 — and `build_prompt`
  (`prompt.rs:404`) receives no signal that an image is coming on this turn,
  so a licensed 婉拒 could land beside an appended `image_request` on a
  `reply_text_image` turn, and a licensed 答应 would be an unfulfilled promise
  that then persists into `[recent_conversation]`. The non-committal options
  (撒娇、调侃) supply the anti-stiffness function without the authority.

## 1. The PDE image signal does not read the reply text

Fewer bracket markers in model output must not starve the image decision. It
cannot, on either PDE path:

- **Ordering.** The plan is fully resolved — rules, LLM judge, ghosting
  kill-switch, forced-image override — by `stream.rs:3200`. The text-reply arm
  that runs the chat model begins at `stream.rs:3491`, and its own comment
  records the direction of the dependency: "`resolved_image_gen` / `req_image`
  were resolved in the decision block above and are REUSED here." On the turn
  being decided, the reply text does not exist yet.
- **Rule path.** `pde::decide` (`pde.rs:33`) takes `DecisionInput { event,
  affinity, persona, signals }` (`types.rs:197`). Assistant reply text is not a
  field of that struct; `Event::UserMessage` carries the user's content only.
- **Judge path.** The two image facts in the judge's context come from
  `JudgeTranscriptAcc::push` (`stream.rs:2221`), which counts
  `metadata.image` — the persisted DB field written by the image executor —
  and never parses reply text:

  ```rust
  let is_image = metadata.and_then(|m| m.get("image")).is_some();
  ```

  `build_pde_ctx` (`stream.rs:1905`) then renders those counts and tells the
  judge to prefer them: 「以本行计数为准，对话记录里的图片标记仅供参考」.
  Capability comes from a separate `image_available` flag.

The judge's `[最近对话]` transcript does still contain assistant text, so any
bracket marker a model emitted is visible there. That is not a dependency the
rule breaks, it is the pollution loop the rule closes: the prompt already
demotes those markers to advisory, and the load-bearing counts are engine-side.
Fewer echoed markers make the transcript agree with the counts more often.

## 2. Terminology

Two distinct silent outcomes, not to be conflated:

- **Ghost** — a PDE decision. `ghost_decision = true` is stamped on the *user*
  row, no LLM call is made for the turn, and `record_ghost` increments
  `ghost_streak` / `total_ghosts` / `last_ghost_at`. SSE emits
  `meta(action_type=ghost, model=null)` → `done` → `final`.
- **Pseudo-ghost** — the LLM ran and its text resolved empty. Persisted as an
  ordinary assistant reply row with empty content, surfaced as
  `done(ghost_fallback=true)` with `metadata.fallback_reason` of
  `empty_completion` or `regex_strip`. Touches no ghost counters — the persona
  decided nothing.

This spec concerns pseudo-ghosts with `fallback_reason="regex_strip"` only.
See `docs/ghost-mechanics.md:126` and `stream.rs:508`.

## 3. Measured baseline

Production, 30 days to 2026-08-06, `engine.chat_messages` where
`role='assistant'` — 2486 reply rows, 192 with empty `content`. Split by the
four-class taxonomy in `etherfunlab/CLAUDE.md`:

| Class | Meaning | n |
|---|---|---|
| A | image-only turn (`metadata.image` present) — normal | 88 |
| **B** | **`output_regex` stripped to empty — pseudo-ghost** | **15** |
| C | `generation_id` present, zero output — continuation row covers it | 8 |
| D | upstream zero output — continuation row covers it | 81 |

Class B is 15 / 2486 = **0.60%** of replies. Only class B is in scope; A is
correct behaviour, and C/D are upstream faults the continuation path already
masks (`continues_from_message_id`).

All 15 class-B rows share three properties, each at 100%:

- the raw reply was bracket-only — a single `[...]` and nothing else;
- the bracket echoed the engine's own `你给对方发送了一张照片` marker;
- 7 of 15 followed a user message asking for a photo; the other 8 did not.

Spread across five model families, so this is not one model's defect:

| Model | replies | bracket-stripped | class B | class B % |
|---|---|---|---|---|
| `sao10k/l3.3-euryale-70b` | 318 | 27.7% | 8 | 2.52% |
| `nousresearch/hermes-4-70b` | 71 | 21.1% | 1 | 1.41% |
| `z-ai/glm-4.7-flash` | 356 | 2.2% | 2 | 0.56% |
| `x-ai/grok-4.20` | 593 | 22.9% | 2 | 0.34% |
| `gemma-4-uncensored@venice` | 728 | 7.1% | 2 | 0.27% |

`thedrummer/cydonia-24b-v4.1` contributes **zero** class-B rows; its 54 empty
rows are all class D. It is out of the production rotation and is not evidence
for this change.

## 4. The change (`prompt.rs`)

In the `format!` block that renders `[iron_rules — 违反即失效]`
(`prompt.rs:687`), insert one line after ⑪ (`prompt.rs:698`) and before the
blank line preceding `[output]`:

```
⑫ 回复里绝不出现方括号 [ ]：记录里的「[你给对方发送了一张照片：…]」是系统留的，不是你的话，别照抄，也别换成别的方括号动作块；每条回复都必须有正文，对方要照片时顺着话自然回应即可（撒娇、调侃都行），照片不用你发。
```

The string contains no `{` or `}`, so no `format!` escaping is required.

Five load-bearing clauses:

1. `回复里绝不出现方括号 [ ]` — the absolute format constraint.
2. `是系统留的，不是你的话` — disowns the marker, following
   `ANTI_REFUSAL_GUARD`'s shape.
3. `也别换成别的方括号动作块` — closes the paraphrase route to the same strip.
4. `每条回复都必须有正文` — a positive constraint, independently checkable, so
   a model that violates clause 1 can still be caught here. Weak models follow
   positive instructions more reliably than prohibitions.
5. `照片不用你发` — states the boundary without inviting a refusal.

The `（撒娇、调侃都行）` parenthetical exists so the model has a graceful text
exit and does not substitute a stiff "我发不了照片" for the marker — it does
not offer verbal accept/decline; see §0 for why.

Rule numbering is unaffected by ⑧'s conditional rendering: ⑧ is appended to
⑦'s line via `{gender_rule}`, and ⑨⑩⑪⑫ follow as literal text.

## 5. Test

One test in `prompt.rs`'s test module, shaped like
`build_prompt_renders_iron_rule_zero_before_one` (`prompt.rs:1397`):

- ⑫ renders in the built prompt;
- its offset is after ⑪ and before `[output]`;
- the five load-bearing substrings `方括号`, `是系统留的，不是你的话`,
  `也别换成别的方括号动作块`, `每条回复都必须有正文`, `照片不用你发` are
  present, so a later edit cannot silently drop a clause.

No existing test breaks. The iron-rules text appears only in `prompt.rs`; every
current assertion over it is structural (block ordering at `prompt.rs:1336`
and `prompt.rs:1348`, presence at `prompt.rs:1023`), and there is no golden
snapshot of the prompt anywhere in the repo.

## 6. Verification

Unit tests prove only that the rule renders. The behavioural claim is
verifiable in production data alone.

Primary metric — class-B rate:

```sql
select
  count(*) filter (
    where metadata->>'fallback_reason' = 'regex_strip'
      and not (metadata ? 'image')
  ) as klass_b,
  count(*) as replies
from engine.chat_messages
where role = 'assistant' and sent_at > now() - interval '30 days';
```

Baseline 15 / 2486 = 0.60%. Watch `sao10k/l3.3-euryale-70b` specifically
(2.52%, largest sample and most sensitive). This query was run against
production before deploy and returned exactly this baseline (`klass_b = 15`)
— the instrument and the §3 baseline are the same measurement, not two
independently derived numbers that happen to agree.

Secondary metric — bracket-strip rate (`filter_model = '<regex>'`), baseline
27.7% for euryale. This falling is the stronger signal: it means the model is
emitting fewer brackets, rather than merely happening to leave body text
alongside them.

Classes A, C, and D are out of scope and should not be read as regressions if
they move.

**Rollback trigger.** If the class-B rate or the bracket-strip rate *rises*
for any model after deploy, suspect that quoting the marker verbatim in the
rule text is reinforcing it rather than suppressing it — revert ⑫ and
re-baseline before attempting a differently-worded rule.

Expected effect is a reduction, not elimination. Models in this class follow
format constraints imperfectly, and the engine still strips whatever slips
through.

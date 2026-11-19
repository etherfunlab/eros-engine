# Toilet event, three together actions, four give categories, her state in the picture, genome write routes — Design

- **Date:** 2026-11-20
- **Status:** Approved
- **Type:** Engine change. One migration (indexes only). One new event kind
  drawn, two new event kinds placed, three new action types and four new
  give categories on the wire, one new section in the image composer's
  payload, two new service routes, one CI workflow.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-11-19-vehicles-actions-species-design.md` (machine
  personas, the picture of an accepted action),
  `2026-11-11-eight-short-events-and-hygienic-design.md` (`bathing_together`),
  `2026-11-08-strip-and-world-setting-design.md` (§2.1, the romance gate),
  `2026-10-13-give-categories-design.md` (valuables),
  `2026-10-26-event-message-image-design.md` (the composer's payload),
  `2026-10-27-caller-secret-gate-design.md` and the `/svc/v1` sub-trees.
- **Ships as:** v2.8.0.

## 1. Intent

Seven things, independent of each other, shipped as one release:

1. She can be on the toilet.
2. The user can ask to shower, bathe or take a sauna with her.
3. The user can hand her a car, a motorcycle, a boat or a horse.
4. A downstream server can create and update a persona genome over HTTP
   (issue #546), so it stops writing `engine.persona_genomes` directly.
5. Every foreign key in `engine.*` has an index.
6. A picture drawn while an event is live shows the event. Today it does not:
   the composer is never told what she is doing.
7. The CodeQL scan stops reporting test code.

## 2. Goals, non-goals, anti-goals

**Goals.** The wire additions below; a picture that agrees with her state
block; genome writes behind a secret; a clean code-scanning page.

**Non-goals.** A genome read or delete route; ownership, publishing or
versioning of genomes in the engine; dropping any index the database linter
calls unused; the linter's `web.*` and `payments.*` findings; changing the
standalone compose endpoint or the image-edit endpoint.

**Anti-goals.** Removing a capability to quiet a scanner; rewriting tests to
suit CodeQL; a second stored copy of what she is doing.

## 3. Approaches

**Together actions.** Three ways were on the table: a one-off gesture like
`kiss` (accept or refuse, nothing else); an accept that writes a shared
event; an accept that writes the event and shows a picture. The owner picked
the third: her state block and every later picture follow from the event,
and the accepted turn itself has something to show.

**Her state in the picture.** Three scopes: chat-turn pictures only, the
event's own picture only, or every picture the chat pipeline draws. The
owner picked every picture the chat pipeline draws; the standalone compose
endpoint and the image-edit endpoint stay as they are.

**CodeQL.** Dismiss the alerts one by one, rewrite the tests, or move to
advanced setup and leave test code out of the scan. The owner picked
advanced setup: it also covers tests written later.

**Genome routes.** BYOC's pattern (the JWT tree plus a caller secret) or a
`/svc/v1` sub-tree under its own secret. This spec takes `/svc/v1`: the
caller is a server, there is no end user on the request, and each `/svc/v1`
sub-tree already mounts under its own secret. *(Engine's call.)*

## 4. `using_toilet`

| | |
|---|---|
| wire value | `using_toilet` |
| category | `short` |
| weight / cooldown | 1.0 / 12 hours |
| lasts | 30 minutes |
| place | `Indoor` |
| setting | both |
| cue | （你正在上厕所。） |
| label | 上厕所中 |

Told, not offline, no picture, not in the emotional list, no intimacy gate,
no row in `follows`. A machine persona (`robot`, `android`) never draws it, as with the period
kinds. *(Engine's calls: every value in this section except the category.)*

## 5. Three together actions

| action `type` | the user | accepted, she is | event kind | lasts |
|---|---|---|---|---|
| `shower_together` | 想和你一起淋浴 | 和对方一起淋浴 | `showering_together` (new) | 30 min |
| `bath_together` | 想和你一起泡澡 | 和对方一起洗澡 | `bathing_together` (existing) | 30 min |
| `sauna_together` | 想和你一起蒸桑拿 | 和对方一起蒸桑拿 | `sauna_together` (new) | 1 h |

- No payload, like `kiss`. The DTO, the stored `metadata.action` and the
  OpenAPI enum gain the three values.
- **Who decides.** The judge, as for every action. Before the judge,
  `shower_together` and `bath_together` are ruled out when the turn's scopes
  are not romance-compatible, exactly as `strip` is; `sauna_together` has no
  such rule. *(Engine's call.)*
- **Refused.** Chemistry drops, as for every physical gesture.
- **Accepted.** In the turn's own transaction-or-equivalent write path, the
  engine places the event: a running incident ends, a background keeps going
  (the same preempting insert a `bathe_together` accept uses). The two new
  kinds are `SPECIAL` (placed, never drawn), category `short`, weight 0,
  `Neutral` place, fit both settings, not offline.
- **During a date.** Accepted inside a running date, a together action
  places no event: the date's `on_a_date` row is left alone and the date
  goes on. She answers and the picture is drawn as usual, with the
  action's kind passed to the composer as her state for that turn only.
- **The picture.** An accepted together action is a picture turn when the
  image executor is available, joining `photo`, `strip` and `ask_dancing`:
  the plan becomes `reply_text_image`; a refusal never shows one. The
  composer is told the event that was just placed (§7), so the picture shows
  it.
- **Tone.** `shower_together` and `bath_together` reply in the `emotional`
  tone when the deployment has it on; `sauna_together` does not. *(Engine's
  call.)*
- **Her own ask.** `bathe_together` (the life choice) and its cooldown do
  not read these actions, and the actions do not read hers. *(Engine's
  call.)*
- The two new kinds are never told, like `bathing_together`: they have a
  label for her state block and no cue. *(Engine's call.)*

## 6. Four give categories

`car`, `motorcycle`, `boat`, `horse` join `GivenItem` as valuables: nothing
lingers on her, no need moves, no habit reads them, no setting gate. The
measure word and noun the prompt uses:

| item | 量词 | 名词 |
|---|---|---|
| `car` | 辆 | 汽车 |
| `motorcycle` | 辆 | 摩托车 |
| `boat` | 艘 | 船 |
| `horse` | 匹 | 马 |

## 7. Her state in the picture

**Cause.** The composer's payload has seven sections: appearance, what she
has on, body state, recent scene, the user's latest message, style, aspect
ratio. None says what she is doing. An event reaches the picture only when
the last few lines of chat happen to mention it.

**Change.** A section `[此刻状态]` after `[身体状态]`, holding the labels
(`prompt::event_label`) of what is live at compose time: the background
first, then the foreground incident, joined by `；`. When nothing is live
the section is absent and the payload is byte-identical to today's, the way
`[同框人物]` behaves.

**Where.** Every picture the chat pipeline draws: a chat turn's picture
(judge-chosen, forced, or an accepted action's), an event message's own
picture, a proactive message's picture, a group turn's picture (the
speaker's state). For an event message's own picture the event is not
stored yet when the composer runs, so the kind being started is passed in
and counts as live. The same holds for the event an accepted together
action places in that turn.

**Not changed.** The standalone compose endpoint and the image-edit
endpoint send no such section.

**Audit.** The composer's input snapshot gains `current_state` (empty
string when absent).

**What she has on.** The engine overrides nothing: `[此刻穿着]` is sent
as it is today. The built-in composer prompt says that where her state and
what she has on disagree, the state wins. This holds for every kind, the
together kinds and her own `showering` and `bathing` included.

**Prompt.** The built-in composer prompt names the section and says the
picture must show it. The comment block in `examples/model_config.toml`
(and its `.d` copy) lists it. An operator with a custom composer prompt
should add a line for it; without one the model still sees the section.

## 8. Genome write routes (#546)

A new `/svc/v1` sub-tree, mounted only when `GENOME_WRITE_SECRET` is set,
admitted by the header `x-genome-write-secret` (constant-time compare, 403
otherwise), like the other sub-trees.

| | route | body | answers |
|---|---|---|---|
| create | `POST /svc/v1/genomes` | `{ "id"?: uuid, "card": <eros-character card> }` | `201 { "id" }`; `200 { "id" }` when that id already exists |
| update | `PUT /svc/v1/genomes/{id}` | `{ "card": <eros-character card> }` | `204`; `404` when no such genome |

- `card` is the eros-character document (`spec`, `spec_version` `"1"` or
  `"1.1"`, `data` with the four fields), validated by the one function BYOC
  uses (`parse_card_v1`), with the same 64 KiB limit. A rejected card is
  `400` with that function's fixed message; card text is never quoted back.
- **Retry-safe create.** The caller may supply `id`. If a genome with that
  id exists the route answers `200` with the id and writes nothing, so a
  retried create leaves one row. Without `id` the engine mints one.
- **Update** overwrites the four card columns in place.
- No user on the request, no ownership check: the caller does its own.
- Declared with `#[utoipa::path]` and `ToSchema` DTOs before the handlers.
- The `seed-personas` CLI is untouched.

*(Engine's calls: the path, the secret's name, the envelope, the status
codes.)*

## 9. Foreign-key indexes

One migration, `0086`, plain `CREATE INDEX IF NOT EXISTS` on:

| table | column |
|---|---|
| `companion_affinity` | `instance_id` |
| `companion_memories` | `instance_id` |
| `persona_story_memories` | `event_id` |
| `persona_story_memories` | `instance_id` |
| `world_memories` | `instance_id` |
| `world_post_comments` | `author_instance_id` |
| `world_posts` | `instance_id` |

`world_memories` and `persona_story_memories` have `(owner_uid,
instance_id)` today, which does not cover a delete from
`persona_instances`. Existing indexes stay as they are, the unused ones
included: several cover a foreign key, the rest serve features a
deployment may not have traffic on yet.

## 10. CodeQL

- A workflow `.github/workflows/codeql.yml` replaces default setup:
  languages `rust` and `actions`, the default query suite, on push and pull
  request to `main` and `dev`, and weekly.
- The Rust job sets `CODEQL_EXTRACTOR_RUST_OPTION_CARGO_CFG_OVERRIDES` to
  `-test`, so `#[cfg(test)]` modules are not extracted. The workspace has
  no integration-test directories, so no path filter is needed.
- Default setup is switched off in the repository's settings (one API
  call) immediately before the workflow's pull request is opened: an
  advanced-setup upload is rejected while default setup is on.
- Nineteen of the twenty open alerts are in test modules and close on the
  first scan of `main` without them. The twentieth, request forgery at the
  provider call, is BYOK's caller-supplied URL: the feature itself, guarded
  by `check_url` (https only, private addresses refused). It is dismissed
  by hand as a false positive with that sentence.

## 11. Order of work

One pull request at a time, each into `dev`:

1. §7, her state in the picture.
2. §4–§6, the toilet kind, the together actions, the give categories.
3. §8, the genome routes (`Fixes #546`).
4. §9, the index migration.
5. §10, the CodeQL workflow.
6. Docs: `life-events-needs`, `api-reference`, `model-config`, `deploying`
   (the new secret), and `docs/migrating/v2-8-0-*.md`.

Then promote and release v2.8.0, crates included. Web-side follow-ups are
filed as issues on the web repository: the composer prompt line, the new
wire values, moving genome writes to the routes.

## 12. Migrating notes to write

- The composer payload gains a section while an event is live.
- An accepted together action places an event and may show a picture.
- `GENOME_WRITE_SECRET` and the two routes.
- Migration `0086` adds indexes only; no order constraint with the image.

## 13. Left to the builder

- Where exactly in the turn the accepted action's event is written relative
  to the composer call, as long as the composer is told the kind.
- Index names.
- Whether `PUT` on an unchanged card writes or skips.

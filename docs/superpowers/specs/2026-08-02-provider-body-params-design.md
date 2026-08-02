# eros-engine — per-provider custom body parameters; remove provider prefs from `[defaults]`

`[providers.<name>]` gains **body rules**: deployer-defined JSON merged into
the chat/completions wire body, scoped per task. Motivating case: Venice's
`venice_parameters` extension block. The same mechanism is the designated home
for OpenRouter-specific body fields going forward — the built-in wire path is
to keep only OpenAI-compatible fields (`temperature` and friends), with
`reasoning` as the one grandfathered exception. As the first step of that
migration, `defaults.ignore_providers` and `defaults.provider_sort` are
**removed** (breaking, no forward compat): both are OpenRouter-specific
(`provider.ignore` / `provider.sort`, one wire object) and are now expressed
as an ordinary body rule on `[providers.openrouter]`.

Related: `2026-07-31-multi-llm-providers-design.md` (the `[providers]` block),
`2026-08-01-embedding-providers-design.md`.

---

## 0. Decisions (settled during brainstorm)

- **The reserved `openrouter` entry participates.** Any `[providers.<name>]`
  may declare body rules; there is no custom-only carve-out.
- **`[tasks.*].reasoning` survives as a forward-compat exception**, and is sent
  only on the built-in OpenRouter path (custom providers already strip it —
  reasoning shapes differ per vendor, so custom providers declare their own via
  body rules). When `[providers.openrouter]` rules also produce a `reasoning`
  key, **the providers block wins** — which falls out of the merge order for
  free.
- **chat/completions only** (streaming + non-streaming). `chat_vision`'s
  hand-rolled body and both embedding paths are out of scope.
- **Rules array, not a single param set.** Per-task granularity is required by
  the migration direction: `[tasks.*].reasoning` is per-task today, so its
  providers-block successor must be expressible per task.
- **Providers block wins on key conflicts, uniformly.** No per-field special
  cases; deployers may override `temperature` etc. Only the structural keys
  `model` / `messages` / `stream` are refused (at boot).
- **`ignore_providers` and `provider_sort` are removed together** and leftover
  keys refuse boot with a migration hint — the
  `[tasks.chat_image_generation]` removal precedent
  (`2026-08-01-remove-image-draw-endpoint-design.md`).
- **All validation at boot; zero new runtime failure surface.** Runtime is
  lookup + merge; no match means no merge.

---

## 1. Config surface

```toml
[providers.venice]
chat = "https://api.venice.ai/api/v1/chat/completions"

[[providers.venice.body]]
tasks  = ["chat_companion", "chat_output_filter"]   # omitted = every chat task this provider serves
params = { venice_parameters = { include_venice_system_prompt = false } }

[[providers.venice.body]]
tasks  = ["chat_companion"]
params = { reasoning = { max_tokens = 512 } }        # later rule wins on key conflict

[[providers.openrouter.body]]                        # reserved name participates
tasks  = ["chat_companion"]
params = { provider = { ignore = ["deepinfra"], sort = "price" }, transforms = ["middle-out"] }
```

`ProviderEntry` (`model_config.rs:541`) gains `body: Option<Vec<BodyRule>>`;
`BodyRule { tasks: Option<Vec<String>>, params: <non-empty table> }`. `params`
is opaque TOML→JSON passthrough — the engine never interprets it. Task match
is exact, case-sensitive string equality.

**Boot refusals** (in `validate_providers`, `main.rs:305`):

| condition |
|---|
| `params` empty, or containing `model` / `messages` / `stream` |
| `tasks = []` (empty array; omit the key to mean "all") |
| a custom provider declares `body` but no `chat` url (`openrouter` exempt — built-in url) |
| `defaults.ignore_providers` or `defaults.provider_sort` present — error text shows the `[providers.openrouter]` body-rule replacement |

**Boot warnings** (not fatal): a `tasks` entry that is not a known engine task
name (typo guard, against a small const list of engine task names introduced
by this PR), or names a task this mechanism does not cover (`chat_vision`,
`embedding`).

Dir-mode config merge needs nothing new — `[providers]` already merges as one
whole top-level key (`model_config.rs:1405`).

## 2. Data flow

- `ChatRequest` (`openrouter.rs:64`) gains `task: Option<String>` — config
  routing only, never serialized. Every pipeline call site sets it from its
  existing `*_TASK` const; `..Default::default()` paths stay `None` (= no
  rules apply). Sites: `handlers.rs:257` (chat), `post_process.rs:653`
  (affinity), `:864`, `:961` (insight), `dreaming.rs:201` (memory),
  `stream.rs:1368` (output filter), `:1652` (PDE), `:2204` (input filter),
  `:2317` (compose), `:3051` (product QA), `voice.rs:194`, `world.rs:290`,
  `world_town.rs:192`, `:305`, `story.rs:310`.
- Boot: `build_providers` (`model_config.rs:2556`) carries rules into
  `ProviderEndpoint`; the `openrouter` entry's rules travel on a separate
  client field (it is not in the custom-providers map).
- Merge point — `call_once` (`openrouter.rs:906`) and `execute_stream_as`
  (`:1023`), after `resolve_endpoint`: existing `for_endpoint` stripping runs
  first (custom providers still lose `session_id` / `metadata` / `reasoning`),
  then rules matching the request's task are merged into the serialized
  `WireRequest` as a top-level shallow merge, declaration order, later wins.
  When no rule matches, the wire bytes are identical to today.
- **Per-attempt resolution**: a fallback chain crossing providers (primary
  `@venice`, fallback OpenRouter) looks up rules per attempt, against the
  provider actually being called.

## 3. Removed

| Item | Site (pre-PR) |
|---|---|
| `DefaultConfig.ignore_providers`, `.provider_sort` | `model_config.rs:509-529` |
| `ModelConfig::ignore_provider_wire_slugs` | `model_config.rs:2585-2612` region |
| `ProviderPrefs`, `provider_prefs()` | `openrouter.rs:279`, `:642` |
| `WireRequest.provider` | field within `openrouter.rs:287-313` |
| `OpenRouterClient::with_ignore_providers`, `::with_provider_sort` (+ fields) | `openrouter.rs:513-561` region |
| vision-body prefs injection (`execute_vision` merges `provider_prefs()` into the built-in-endpoint vision body) | `openrouter.rs:791-797` |
| boot wiring of the two builders | `main.rs:357-364` |

Consequence of the vision row (found during planning): `chat_vision` calls
lose `provider.ignore`/`sort` entirely — body rules are chat-only and do not
replace them there. Accepted; revisit only if a vision routing need appears.

## 4. Tests

- Config: rule parsing (single / multiple / omitted `tasks`); each boot
  refusal; both warnings; dir-mode merge with rules.
- Wire (existing wiremock/serialization harness): merge applied for an
  enabled task; not applied for an unlisted task; later rule wins;
  `[providers.openrouter]` `reasoning` overrides `[tasks.*].reasoning`;
  custom-provider stripping runs before merge
  (`custom_endpoint_wire_is_strict_openai_subset` adapted — `provider` field
  gone); streaming path merges identically; cross-provider fallback resolves
  per attempt.

## 5. Docs

- `docs/model-config.md` — body-rules subsection under `[providers]`
  (shape, merge semantics, reasoning exception, structural-key refusals);
  `[defaults]` section drops the two keys and shows the migration snippet.
- `examples/model_config.toml` — commented body-rule example; drop the two
  defaults keys.
- OpenAPI untouched (config-side only). **Breaking config change** — release
  notes entry when the maintainer cuts the release.

## 6. Out of scope / non-goals

Vision and embedding bodies; the per-request audit passthroughs (`user`,
`metadata`, `session_id`); per-tier granularity; completing the
"built-in path is OpenAI-only" migration (this PR only lands the `reasoning`
exception and the provider-prefs removal); version/release timing.

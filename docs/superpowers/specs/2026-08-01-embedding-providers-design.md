# eros-engine — custom embedding providers + voyage read/write model split

Activates the long-reserved `[tasks.embedding]` block for real: the embedding
model becomes configurable, embedding calls can route to the built-in Voyage
client, the built-in OpenRouter embeddings endpoint, or any third-party
endpoint speaking the OpenRouter-compatible embeddings format — and, on the
voyage-4 series and above, the recall (read) and storage (write) paths can use
two different models that share one vector space.

Today `VoyageClient` hard-codes `voyage-3-lite`, 512 dims, and
`$VOYAGE_API_KEY` (`voyage.rs:10-12`); `[tasks.embedding]` is documented as
"Reserved — not consumed by current code". This design makes the config real
and clears that tech debt in the same move.

Builds directly on the multi-provider design
(`2026-07-31-multi-llm-providers-design.md`); everything not amended here —
slug grammar, `\@` escaping, name charset, key naming, bare-id rules for
model-keyed tables — carries over unchanged.

---

## 0. Decisions (settled during brainstorm)

- **`[providers]` values become tables** — `{ chat = "…" }`,
  `{ embeddings = "…" }`, or both. The 0.9.3 plain-string shape is **dropped
  with no compatibility layer**: that shape shipped in an unpublished
  release-in-progress and has no consumers. A string value now fails the load
  with a message showing the table form.
- **`dimensions` is removed from `[tasks.embedding]`.** It was a mistaken
  reservation, never consumed. The engine hard-codes **512** end to end: the
  three pgvector columns are `VECTOR(512) NOT NULL`
  (`0003_memory.sql`, `0035_world_memories.sql`, `0038_world_stories.sql`),
  the clients request 512 on the wire (one legacy fixed-dim exception,
  §5.2), and every response is length-checked.
  A leftover `dimensions = 512` line in an operator's config is inert
  (serde ignores unknown keys, exactly like any stale key today).
- **The `openrouter` reservation in `[providers]` is relaxed, and the
  `OPENROUTER_BASE_URL` env var is removed.** Declaring
  `[providers].openrouter` overrides the built-in endpoint URLs, per key
  (§4), and is now the only override mechanism. No boot guard for a
  still-set `OPENROUTER_BASE_URL`: the var never shipped in a release and
  has no consumers, so it is simply deleted and any set value is ignored
  like any unrelated env var.
- **`[providers]` entries carry custom headers; the env attribution vars
  are soft-deprecated.** Each entry accepts an optional `headers` table
  sent verbatim on every request to that provider's endpoints — for
  `openrouter`, this is where `HTTP-Referer` / `X-OpenRouter-Title` /
  `X-OpenRouter-Categories` now live. The three `OPENROUTER_APP_*` env
  vars become **inert: still-set values are silently ignored, not a boot
  error** — these vars did ship, but losing an attribution header costs
  analytics, not correctness, so a boot refusal would punish upgrades out
  of proportion. Deployments re-declare their headers under
  `[providers].openrouter`.
- **`[defaults].ignore_providers` entries must carry `@openrouter`.**
  Each entry is now written `<upstream-slug>@openrouter` (e.g.
  `"some-bad-provider-slug@openrouter"`); a bare entry, any other
  `@<provider>` suffix, or malformed `@` grammar refuses to boot with a
  message showing the new form. The suffix is stripped before the wire —
  `provider.ignore` still sends bare upstream slugs. This makes the
  knob's scope part of its syntax: it acts on OpenRouter routing only
  (custom providers and Voyage never receive `provider.ignore`), and the
  notation leaves room for per-provider ignore lists later. Breaking for
  existing configs (the field shipped pre-v0.6.1), loud not silent: the
  migration is appending `@openrouter` to each entry.
- **Bare chat slugs still mean OpenRouter** — the chat wire format is defined
  against OpenRouter, so no suffix ⇒ the built-in (or overridden) OpenRouter
  chat endpoint, exactly as in 0.9.3. `@openrouter` becomes a globally valid
  explicit spelling of the same thing.
- **Bare embedding slugs mean Voyage.** In `[tasks.embedding]`,
  `"voyage-4-lite"` ≡ `"voyage-4-lite@voyage"`. The default provider flips
  per task family because the incumbent embedding backend is Voyage, not
  OpenRouter.
- **The default model is `voyage-4-lite`** (post-review amendment,
  2026-08-01: Voyage no longer recommends `voyage-3-lite`). A deployment
  that still wants `voyage-3-lite` must pin it explicitly — and switching
  models over existing data changes the vector space, so pin or re-embed.
- **`@voyage` is embeddings-only.** A `@voyage` suffix on any non-embedding
  task slug refuses to boot. `voyage` stays undeclarable in `[providers]`
  (its native API is not the OpenRouter-compatible format this mechanism
  speaks).
- **`model_read` / `model_write` are a voyage-4-gated pair.** Mutually
  exclusive with `model`, must appear together, voyage-only, and the model id
  must parse as `voyage-<N>…` with N ≥ 4 — enforced at boot, because mixing
  vector spaces fails silently at query time, the worst possible failure
  mode.
- **Client structure: an `EmbeddingRouter` facade** holding a read backend
  and a write backend (the same object when a single `model` is configured),
  each backend either the native `VoyageClient` or a new OpenAI/OpenRouter-
  compatible `EmbedHttpClient`. The server-facing API
  (`embed_query` / `embed_document` / `embed_documents`) is unchanged.
- **Third-party embedding providers must speak the OpenRouter-compatible
  embeddings format** (`POST` body `{model, input, dimensions}` → response
  `data[].embedding`), per
  <https://openrouter.ai/docs/api_reference/embeddings>. The engine never
  translates model ids; whatever precedes `@` goes on the wire verbatim.
- **Embedding model specs are single fixed strings.** Round-robin and
  weighted forms would interleave incompatible vector spaces; `fallback` and
  `tiers` on `[tasks.embedding]` are rejected for the same reason. All
  refuse to boot (the `chat_voice` fixed-only precedent).

---

## 1. The `[providers]` block — table values

```toml
[providers]
venice = { chat = "https://api.venice.ai/api/v1/chat/completions" }
mixed  = { chat = "https://x/v1/chat/completions", embeddings = "https://x/v1/embeddings" }
local  = { embeddings = "http://127.0.0.1:8080/v1/embeddings" }

[providers.proxy]           # TOML section form works too
chat    = "https://proxy.internal/v1/chat/completions"
headers = { "X-Team" = "companion", "X-Env" = "prod" }
```

- Parsed as `HashMap<String, ProviderEntry>` where `ProviderEntry
  { chat: Option<String>, embeddings: Option<String>, headers:
  Option<HashMap<String, String>> }` with `deny_unknown_fields`. A
  plain-string value, an empty table, an unknown key, or an empty URL
  string refuses the load.
- `headers` are sent verbatim on every request to that entry's endpoints
  (both `chat` and `embeddings`). Header names and values must be valid
  HTTP header material at boot (refuse, don't warn-and-drop);
  `Authorization` and `Content-Type` are engine-owned and refuse the load
  if declared (case-insensitive) — a silently overridden `Authorization`
  is the worst kind of footgun.
- Both URLs are complete and posted verbatim — no path joining, unchanged
  from the multi-provider design.
- One name, one key: `<NAME_UPPERCASED>_API_KEY` covers both the `chat` and
  `embeddings` endpoints of that entry. Required only when the entry is
  actually referenced by some slug (unchanged).
- A provider referenced from a chat-shaped task must declare `chat`; one
  referenced from `[tasks.embedding]` must declare `embeddings`. A miss on
  either refuses to boot naming the slug, the entry, and the missing key.
- Merge behaviour under `MODEL_CONFIG_DIR` is unchanged: `[providers]` is
  one whole top-level key.

---

## 2. `[tasks.embedding]` — activated

```toml
# Single model — read and write use the same backend.
[tasks.embedding]
model = "voyage-4-lite"                       # ≡ "voyage-4-lite@voyage"
# model = "voyage-3-lite"                     # legacy pin (no longer recommended by Voyage)
# model = "openai/text-embedding-3-small@openrouter"
# model = "bge-m3@local"                      # third-party, OpenRouter-compatible wire

# OR: split read/write — voyage-4 series and above ONLY.
#[tasks.embedding]
#model_read  = "voyage-4-lite"   # recall path: embed_query, input_type "query"
#model_write = "voyage-4"        # storage path: embed_document(s), input_type "document"
```

Field semantics:

| field | type | rules |
|---|---|---|
| `model` | single fixed string | bare ⇒ `@voyage`; `@openrouter` / `@<custom>` route to the OpenRouter-compatible wire; mutually exclusive with the pair |
| `model_read` | `Option<String>` | pair-only, voyage-only, N ≥ 4; serves `embed_query` |
| `model_write` | `Option<String>` | pair-only, voyage-only, N ≥ 4; serves `embed_document` / `embed_documents` |

- `model_read` / `model_write` are plain `Option<String>` in `TaskConfig`,
  so array/table shapes are type errors at parse time; `model` stays the
  shared `ModelSpec` and boot validation enforces the `Fixed` shape.
- `model_read = model_write` is legal (redundant but harmless — equivalent
  to `model`).
- The read/write split exists because documents are embedded once but
  queries are embedded every turn: a deployment can write with a large
  voyage-4 model and read with a cheap one (or vice versa) inside the one
  shared voyage-4 vector space.
- `[tasks.embedding]` **absent** ⇒ native Voyage, `voyage-4-lite`
  (`output_dimension: 512` on the wire), `$VOYAGE_API_KEY` required.
- On other `[tasks.embedding]` fields: `temperature`, `max_tokens`, etc.
  remain ignored as today; `fallback` and `tiers` refuse to boot (§6).

### The voyage-4 gate

Applied to the bare id after stripping an optional `@voyage` suffix. The id
must begin `voyage-` followed by a numeric segment (digits and dots, ending
at the next `-` or end of string) that parses as a number ≥ 4:

- ✓ `voyage-4`, `voyage-4-lite`, `voyage-4.5-large`, `voyage-10`
- ✗ `voyage-3.5-lite` (N = 3.5), `voyage-code-3` (no leading numeric
  segment after `voyage-`), `bge-m3@local` (not voyage), any `@openrouter`
  or custom-provider slug

Rationale: only the voyage-4 series and above guarantee a shared vector
space across model sizes. A lower or non-numeric model in the pair would
silently write vectors the read model cannot compare — so the gate is a
boot refusal, not a docs footnote. Cost, accepted: if Voyage changes its
naming scheme, the gate needs a code change to admit the new names.

---

## 3. Slug semantics per context

| context | bare (no suffix) | `@openrouter` | `@voyage` | `@<custom>` |
|---|---|---|---|---|
| chat-shaped task `model`/`fallback` | built-in OpenRouter chat (unchanged) | same as bare | **boot error** | `[providers].<name>.chat` |
| `[tasks.embedding].model` | native Voyage | OpenRouter embeddings endpoint | same as bare | `[providers].<name>.embeddings` |
| `model_read` / `model_write` | native Voyage | **boot error** | same as bare | **boot error** |

`@openrouter` needs no `[providers]` entry and no extra key check
(`$OPENROUTER_API_KEY` is already unconditionally required for chat). The
slug grammar in `provider.rs` is untouched — `openrouter` and `voyage` are
resolution-level semantics applied after `split_model_slug`, not new syntax.

---

## 4. Built-in endpoint overrides via `[providers].openrouter`

```toml
[providers.openrouter]
embeddings = "http://my-proxy/v1/embeddings"
headers    = { "HTTP-Referer" = "https://eros.example", "X-OpenRouter-Title" = "Eros" }
```

- Each present key overrides that built-in URL; each absent key keeps the
  built-in default (`https://openrouter.ai/api/v1/chat/completions` /
  `https://openrouter.ai/api/v1/embeddings`). This partial-override rule is
  unique to `openrouter` — for ordinary entries a missing key is a boot
  error when referenced, because there is no built-in default to fall back
  to.
- The override changes the URL **only**. Traffic through it remains the
  full OpenRouter wire: `provider.ignore`, `provider_sort`, and per-task
  `reasoning` are all still sent — unlike custom providers, which keep
  receiving the strict OpenAI subset.
- Attribution headers now come from this entry's `headers` table and
  nowhere else. No entry, or an entry without `headers` ⇒ no attribution
  headers are sent. `AppAttribution` and its warn-and-drop env plumbing
  are removed with the env vars (§7) — the same tech-debt sweep as
  `voyage.rs`.
- The API key stays `$OPENROUTER_API_KEY` — the `<NAME>_API_KEY` naming
  convention degenerates to exactly the env var that already exists.
- `OPENROUTER_BASE_URL` is removed as an env var (§0) — the
  `with_openrouter_base_url` builder and the `main.rs` env read go with
  it; no boot guard (never shipped, no consumers).
- `voyage` remains undeclarable in `[providers]`.

### `[defaults].ignore_providers` — `@openrouter` required

```toml
[defaults]
ignore_providers = ["some-bad-provider-slug@openrouter"]
```

- Every entry must parse (same `split_model_slug` grammar) to a non-empty
  upstream slug plus provider `openrouter`; anything else refuses to boot.
- The wire is unchanged: `provider.ignore` carries the bare upstream
  slugs, on OpenRouter traffic only. The knob stays inert for custom
  providers and Voyage — which is exactly what the mandatory suffix now
  says out loud. `[defaults].provider_sort` is untouched (it has no
  per-entry syntax to scope).

---

## 5. Clients

### 5.1 `EmbeddingRouter` (new module `eros-engine-llm/src/embedding.rs`)

```rust
pub struct EmbeddingRouter {
    read:  EmbedBackend,   // embed_query
    write: EmbedBackend,   // embed_document, embed_documents
}

enum EmbedBackend {
    Voyage(VoyageClient),          // native Voyage wire, keeps input_type
    OpenAiCompat(EmbedHttpClient), // OpenRouter-compatible wire
}
```

- Public API mirrors today's `VoyageClient` surface: `embed_query`,
  `embed_document`, `embed_documents`. Call sites keep their shape.
- Built once at boot from `ModelConfig` + env; a single `model` yields the
  same backend on both sides.

### 5.2 `VoyageClient` — consumes the config (tech-debt cleanup)

- Model comes from `[tasks.embedding]` instead of the old hard-coded
  `voyage-3-lite`; the hard-coded default (`voyage-4-lite` after the §0
  amendment) remains only for the block-absent path.
- Sends `output_dimension: 512` on the wire **except** for `voyage-3-lite`,
  which is a fixed-512-dim legacy model (the parameter is unnecessary
  there, and the exception keeps a pinned legacy config byte-identical to
  its pre-config wire). The exception is a named constant with a comment;
  implementation verifies the exact parameter-support boundary against the
  Voyage API docs.
- `input_type` (`"query"` / `"document"`) behaviour is unchanged.
- Response embeddings are length-checked to 512 before returning
  (a clear `LlmError` instead of a downstream SQL error).

### 5.3 `EmbedHttpClient` (new) — OpenRouter-compatible wire

- `POST <declared embeddings URL>` verbatim;
  `Authorization: Bearer <key>`; plus the entry's declared `headers`, if
  any — nothing else (consistent with custom chat providers).
- Request body: `{ "model": "<bare id>", "input": ["…", …],
  "dimensions": 512 }`. `dimensions` is always sent — without it,
  common models default to other widths (e.g. `text-embedding-3-small` →
  1536) and would be unusable against the `VECTOR(512)` schema.
- Response: `data[].embedding`, ordered by `index` when present; count must
  equal the input count; every vector length-checked to 512.
- Error bodies pass through the existing `scrub_error_body` bounding
  (issue #188 precedent). Non-2xx maps to `LlmError::Status`.
- The wire has no `input_type` — the query/document optimisation is a
  Voyage-native nuance. Documented in the example config: routing embedding
  off Voyage forfeits it.

---

## 6. Boot validation

Extends `validate_providers`; the slug scan becomes task-aware (it already
walks every task's `model`/`fallback` including tier blocks — it now also
walks `model_read`/`model_write` and carries the task name).

| condition | outcome |
|---|---|
| `[providers]` value is a string, empty table, unknown key, or empty URL | refuse |
| entry named `voyage` | refuse (unchanged message) |
| `headers` declares `Authorization` or `Content-Type` (case-insensitive) | refuse |
| `headers` name/value is not valid HTTP header material | refuse |
| `ignore_providers` entry lacks `@openrouter`, names another provider, or has malformed `@` grammar | refuse |
| chat-shaped slug references an entry with no `chat` URL | refuse |
| `[tasks.embedding]` slug references an entry with no `embeddings` URL | refuse |
| referenced entry's `<NAME>_API_KEY` unset/empty | refuse (unchanged) |
| `@voyage` on any non-embedding task | refuse |
| `@openrouter` on chat-shaped slugs or `[tasks.embedding].model` | pass (built-in alias; no `[providers]` lookup) |
| `model` present together with `model_read`/`model_write` | refuse |
| exactly one of `model_read`/`model_write` present | refuse |
| `model_read`/`model_write` slug is non-voyage, or fails the N ≥ 4 gate | refuse |
| `[tasks.embedding].model` is round-robin or weighted | refuse |
| `[tasks.embedding]` has `fallback` or `tiers` | refuse |

All refusals are boot-time `Err(String)` in the existing loud-fail style:
name the config location, the offending value, and the fix.

---

## 7. Server wiring & environment

- `AppState.voyage: Arc<VoyageClient>` → `AppState.embed:
  Arc<EmbeddingRouter>`. Five call sites rename the field, nothing else:
  `handlers.rs` (`embed_query`), `post_process.rs` / `dreaming.rs`
  (`embed_document`), `world.rs` / `story.rs` (`embed_documents`).
- `VOYAGE_API_KEY` goes from unconditionally required to **required iff the
  resolved read or write backend is Voyage**. Block absent ⇒ Voyage default
  ⇒ still required, so existing deployments see no change; a deployment
  routing embedding entirely off Voyage no longer needs the var.
- `OPENROUTER_API_KEY`: unchanged (unconditionally required, chat needs it).
- `OPENROUTER_BASE_URL`: removed, no boot guard (never shipped, no
  consumers). Endpoint overrides live in `[providers].openrouter` only.
- `OPENROUTER_APP_REFERER` / `OPENROUTER_APP_TITLE` /
  `OPENROUTER_APP_CATEGORIES`: soft-deprecated — still-set values are
  silently ignored, never a boot error. Attribution headers are declared
  under `[providers].openrouter.headers` instead.
- Custom provider keys: `<NAME>_API_KEY`, required iff referenced —
  unchanged rule, now also triggered by embedding references.
- No new env vars. No DB migration. No OpenAPI change (embedding is
  internal). No audit-column change.

---

## 8. Testing

- **Validation matrix**: one unit test per row of §6, via
  `validate_providers_with` (injected env closure, no process-global
  mutation).
- **voyage-4 gate parser**: `voyage-4`, `voyage-4-lite`, `voyage-4.5-large`,
  `voyage-10` pass; `voyage-3.5-lite`, `voyage-code-3`, `voyage-`,
  non-voyage slugs fail.
- **`ProviderUrls` parsing**: table forms, string rejection, unknown-key
  rejection, empty-URL rejection, `MODEL_CONFIG_DIR` single-key merge
  unchanged.
- **`EmbedHttpClient`**: mock-server wire tests in the `openrouter.rs`
  `with_base_url` pattern — request body shape (`dimensions: 512`),
  declared `headers` present and nothing extra, index-ordered response
  parsing, count mismatch, dim mismatch, error-body scrubbing.
- **Headers**: declared headers reach both chat and embeddings requests;
  reserved-name refusal; invalid header-value refusal; `OPENROUTER_APP_*`
  env vars have no effect on the wire.
- **`ignore_providers`**: suffixed entries parse and strip to bare slugs
  on the wire; bare / wrong-provider / malformed entries refuse.
- **`VoyageClient`**: model-from-config, `output_dimension` present for
  voyage-4-class models, absent for `voyage-3-lite`, response length check.
- **`EmbeddingRouter`**: read/write dispatch (query → read backend,
  document(s) → write backend), single-model construction.
- Server sqlx tests: unaffected beyond the state field rename.
- Full local gate before PR: `cargo fmt` / `clippy` / `test` / openapi
  check (openapi is expected byte-identical).

---

## 9. Documentation

- `examples/model_config.toml`: rewrite the `[providers]` comment block to
  the table form (including the `openrouter` partial-override rule and the
  chat/embeddings key rules); replace the "Reserved — not consumed" note on
  `[tasks.embedding]` with the real contract — bare-means-voyage, the
  `@openrouter`/custom routes, `model_read`/`model_write` with the voyage-4
  rationale, the hard-coded 512, the removed `dimensions` line, and the
  input_type forfeiture note for non-Voyage routing.
- `.env.example`: `VOYAGE_API_KEY` comment becomes "required unless
  `[tasks.embedding]` routes read and write off Voyage"; the
  `OPENROUTER_BASE_URL` and `OPENROUTER_APP_*` lines are deleted.
- `examples/model_config.toml`: the `[providers]` block documents the
  `headers` key, including the `[providers].openrouter.headers` home for
  attribution headers.
- The `[defaults]` comment block: `ignore_providers` examples gain the
  `@openrouter` suffix and a line stating the knob acts on OpenRouter
  routing only.
- Migration note for the multi-provider spec's readers: `[providers]`
  string values are gone, `OPENROUTER_BASE_URL` moved into
  `[providers].openrouter`, `OPENROUTER_APP_*` attribution moved into its
  `headers` table, and `ignore_providers` entries need the `@openrouter`
  suffix (§0); a one-line fix per item.

---

## 10. Out of scope

- Non-512 dimensions (would require a pgvector migration and a full
  re-embed; nothing in this design moves toward it).
- Fallback chains, tiers, round-robin/weighted specs for embedding.
- Model-id translation between providers.
- Auditing embedding calls to the DB.
- Overriding the native Voyage base URL.
- Any hosted-service or deployment concern (OSS boundary).

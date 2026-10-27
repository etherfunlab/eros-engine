# Caller secret gate — design

**Status:** Approved
**Date:** 2026-10-27

## Intent

The engine's main HTTP surface (`/comp/*`, `/bff/v1/*`, `/world/*`,
`/persona/*`, and the rest of the Bearer-JWT tree) admits any request
carrying a valid Supabase JWT. End users hold such a JWT in their browser,
so on a deployment whose engine origin is reachable from the internet they
can bypass the deployment's own middleware — its billing, quota, and
body-shaping gates — and call the engine directly.

This feature lets a deployment require a second credential on that tree: a
caller secret agreed between the engine and its middleware (a web client's
server side). A request without it is refused before JWT validation. End
users never see the secret; only the middleware holds it.

## Goals

- A deployment can close the "end user calls the engine directly" hole with
  one env var on the engine and one header on its middleware.
- Zero-downtime secret rotation.
- Unset = exactly today's behavior. Existing deployments upgrade without
  action.

## Non-goals

- Per-caller identity, scopes, or rate limiting — this is one shared
  admission credential, not an API-key system.
- Changing `/svc/v1/*` (each sub-tree keeps its own secret), `/healthz`
  (platform health checks), or `/docs` + the OpenAPI document (public
  generated artifacts). (User decision.)
- The middleware side (header injection, secret storage) — that is each
  downstream's own deployment config.

## Anti-goals

- Any error path or log line that echoes a configured or received secret.
- Making the gate mandatory. The engine must keep working for deployments
  where it is not internet-reachable or where direct access is wanted.

## Design

Everything below follows the repo's existing caller-secret precedents
(`BYOK_CALLER_SECRET`, `BYOC_CALLER_SECRET`) unless marked otherwise.

### Configuration

- New env var **`CALLER_SECRET`**. Unset or blank ⇒ gate off, surface
  unchanged.
- The value is **one or more secrets, comma-separated** (user decision:
  multi-key for zero-downtime rotation). Entries are trimmed; empty entries
  (stray commas) are ignored; a comma can therefore not appear inside a
  secret. A non-blank value with no valid entries (e.g. `CALLER_SECRET=,`)
  **refuses to boot** rather than silently leaving the gate off — the
  operator clearly meant to turn it on (my call).
- Every entry must be at least **32 bytes** (`MIN_SVC_SECRET_LEN`), else the
  engine **refuses to boot** with an error naming `CALLER_SECRET` — same
  contract as the `/svc` secrets and the BYOK/BYOC secrets.
- The parsed config redacts the secrets in `Debug` output, like the other
  secret-bearing configs.
- Boot logs one info line when the gate is on, with the key *count* only.

### Request contract

- Header **`x-caller-secret`** must carry one of the configured secrets.
- Comparison is constant-time (`constant_time_eq`) against each configured
  entry.
- Missing or wrong ⇒ **`403`** with the repo's standard pre-handler error
  body, code `caller_forbidden`, message "caller secret missing or wrong".
  The body never distinguishes missing from wrong, nor says how many keys
  are configured.

### Scope (user decision)

- Gated: the entire Bearer-JWT tree — everything composed into the
  `require_auth` merge in `routes/mod.rs` (companion, companion_async,
  companion_stream, voice, bff, byoc, world_town, persona, persona_items,
  insight, image_edit, session_effects, session_open, session_events,
  life_choices, reaction, group).
- Not gated: `/healthz`, `/docs` + the served OpenAPI document, and the
  three `/svc/v1/*` sub-trees (their own secrets stay the only admission).
- The gate runs **before** JWT validation: a request without the caller
  secret is refused without touching the JWT validator.
- BYOK/BYOC requests sit inside the gated tree, so on a deployment with
  both configured they need both headers. That is correct: the BYOC caller
  is the same middleware.

### OpenAPI

- Declare an `apiKey` security scheme (`caller_secret`, header
  `x-caller-secret`) in components, with a description saying the whole
  authenticated tree requires it when the deployment sets `CALLER_SECRET`.
- Do **not** add it to every route's `security` list (my call): the gate is
  deployment-optional and deployment-wide; per-route annotations would
  churn the entire committed `openapi.json` snapshot for a conditional
  property. The top-level API description gains one sentence instead.

### Documentation

- `.env.example`: a commented `CALLER_SECRET=` block next to the BYOK/BYOC
  ones.
- `docs/deploying.md` (+ `.zh`): one entry in the env table/section, same
  shape as the BYOC entry, plus the rotation recipe (add new key on the
  engine → switch the middleware → remove the old key).
- The next release's migrating guide: one paragraph — opt-in, what to set
  on both sides, nothing to do if you don't want it.

## Testing expectations

- Gate off: tree behaves as today (one explicit test).
- Gate on: no header ⇒ 403; wrong value ⇒ 403; correct value ⇒ request
  proceeds to JWT auth (401 without a bearer, 200 with a valid one).
- Rotation: two configured keys, each admits.
- `/healthz` and a `/svc` sub-tree (with its own secret) stay reachable
  with the gate on and no caller header.
- Boot refusal: a single short key; a list with one short entry.
- 403 body carries `caller_forbidden` and echoes no secret.

## Left to the builder

- Exact middleware wiring (layer ordering on the merged router) and where
  the parsed config lives in `ServerConfig`.
- Test placement (alongside the existing admission tests).
- Whether to reuse `parse_svc_secret` or write a list-aware sibling.

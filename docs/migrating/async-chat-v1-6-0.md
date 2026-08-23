# Migrating to the v1.6.0 async chat path

**Applies to:** clients of `eros-engine` upgrading to `v1.6.0`.
**Design:** [`docs/superpowers/specs/2026-08-22-user-insights-and-api-v2-design.md`](../superpowers/specs/2026-08-22-user-insights-and-api-v2-design.md) §4.6

**One path is removed.** `POST /v2/comp/chat/{session_id}/message/async` — the
original spelling of the enqueue-only chat turn shipped in `v1.5.0` — is gone.
It was renamed to `POST /v2/comp/session/{session_id}/message/async` under the
v2 path convention (`session` is the entity the id belongs to; `chat` is not an
entity), and the old path is no longer registered: an authenticated request to it now
returns **404** (an unauthenticated one still gets `require_auth`'s **401**,
as on every `comp` path).

**If you do nothing, every async send fails.** The engine does not fall back;
a 404 on this path with a valid bearer is the whole symptom.

**Fix:** replace the `chat` segment with `session` in the URL. Nothing else
changes — request body (`StreamSendRequest`), auth, idempotency on
`client_msg_id`, and every response status and body are identical.

```diff
-POST /v2/comp/chat/{session_id}/message/async
+POST /v2/comp/session/{session_id}/message/async
```

Only this one path moves. `POST /comp/chat/start`,
`GET /comp/chat/{session_id}/history`, `POST /comp/chat/{session_id}/message/stream`
and the other v1 `/comp/chat/*` routes are unchanged — the rename applies to the
`/v2/` tree only.

**Rollout: the two paths do not overlap across releases.** `v1.5.x` serves
only the `chat` spelling and `v1.6.0` serves only the `session` spelling, so
the client path change and the engine bump land together — deploy the engine,
then the client, and expect async sends to 404 in the window between. No store
migration ships with this change.

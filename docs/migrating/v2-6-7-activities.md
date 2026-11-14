# Migrating to v2.6.7: activities, a date as a sub-session

**Applies to:**
- deployments (one migration, `0085`);
- clients that send chat turns, list sessions, or read history and effects;
- anything that reads `engine.chat_sessions.channel` or `engine.companion_affinity_events.event_type` directly: exports, analytics, SQL consumers;
- deployments that tune model tasks or `AFFINITY_*` knobs (both optional).

Every HTTP change is additive except four new `409` codes on the send routes: `activity_in_progress`, `activity_ended`, `activity_settling` and `choice_pending`. A client sees the first three only around a date that is open, and `choice_pending` only on a `date` action, which it can refuse before any date exists. Migration `0085` must run before the new image serves.
**Design:** [activities](../activities.md), [the send routes](../api-reference.md#post-compchatsession_idmessagestream) and [the end route](../api-reference.md#post-v2compsessionsession_idactivitiesactivity_idend).

## What users will notice

- **They can ask her out.** A `date` action names a place, a plan and a length (`short`, 1 hour, or `mid`, 4 hours). A `mid` date needs bond at tier 2; neither happens while she is offline. She accepts or refuses; a refusal costs nothing.
- **An accepted date is a conversation of its own.** It runs in a sub-session for its length, or until the user ends it. Meanwhile the persona's 1:1 text sessions are paused: sends are refused, `/open` returns `null`, no life event is drawn and she asks no life choice. Voice and group sessions are not paused.
- **The date is judged once, at the end.** Its turns skip the per-turn judgement and the rule nudges, so they move nothing one by one, except that a refused `give`, `gift` or physical action inside a date still drops its line one tier on the parent's affinity. When it is settled, one verdict moves each line by at most one tier, and the date becomes one episode with a `date` journal row and its milestones, all `real`.
- A client that never sends the `date` action changes nothing for its users: no date opens, so nothing pauses.

## Migration `0085` and deploy order

`0085`:
- widens `chat_sessions_channel_check` to admit `'activity'`, and `companion_affinity_events_event_type_check` to admit `'activity'`. Both checks are swapped in `NOT VALID`: existing rows satisfy them already, so neither table is scanned under the lock;
- creates `engine.activities` with its foreign keys and indexes, row-level security on and no policy, `anon` and `authenticated` revoked where those roles exist.

It rewrites no row. The two `ALTER TABLE`s take an `ACCESS EXCLUSIVE` lock under a 3-second `lock_timeout`, so on a busy database the migration can fail before anything changes; run it again.

**Run `migrate` before the new image serves.** The new image reads `engine.activities` on every send, `/open`, event draw and effects call. Against a database without `0085`, all of them fail.

## Readers of the two columns

- **`engine.chat_sessions.channel = 'activity'`** is a date's sub-session. It carries the parent's `user_id` and `instance_id`; its parent is `engine.activities.session_id`, joined on `sub_session_id`. A consumer that counts sessions per user, or treats every session that is not `voice` or `group` as a 1:1 chat, should exclude these or fold them into their parent. Their messages are ordinary `engine.chat_messages` rows.
- **`engine.companion_affinity_events.event_type = 'activity'`** is one row per settled date that had at least one substantive turn, on the parent's affinity, with no `user_message_id`. A date with no substantive turn settles with no event; a date whose judge call failed still writes one, with zero movement. It is not a turn: a consumer that counts judged turns or charts affinity per message should leave it out or show it on its own. Its `deltas` are raw scores times `context.weight`; `context` also carries `activity_id`, `turns`, `reason` and, when the cap held a line, `capped`.
- The turns inside a date, and the invitation she accepted, are `message` events with `context.eval_skip_reason = "activity"`.
- `engine.persona_events.kind` gains `on_a_date`, and `engine.relationship_journal.kind` gains `date`. A consumer that only accepts known values must add them.

The `/svc/v1/*` export routes do not change.

## Clients

1. **Send the invitation.** A `date` action on a turn in a 1:1 text session: `{"type": "date", "duration": "short", "place": "…", "content": "…"}`, `place` 1–40 characters, `content` 1–120, no line breaks. The turn's `action_response` frame says whether she accepted; on `accept` the stream's `final` frame carries `activity` (`id`, `sub_session_id`, `duration`, `place`, `content`, `status`, `started_at`, `ends_at`). The invitation turn never carries a picture.
2. **Find the date after the fact.** Async senders, and any client on a reload or a second device, read the `activity` receipt on the invitation's user row in either history route and in the start bundle, or `activity` on `GET /v2/comp/session/{session_id}/effects`, present while the date is `ongoing`, or `ended` and not settled yet. Effects also show the `on_a_date` incident as `event` while it lasts.
3. **Send date turns to `sub_session_id`** through the same send routes, and read the date's history under that id. Read the relationship at the parent's id: a sub-session has no affinity of its own. The per-turn delta route accepts either id.
4. **End the date.** When the user ends it, and when `ends_at` passes, call `POST /v2/comp/session/{session_id}/activities/{activity_id}/end` with the parent's id. It returns the settled date, the relationship after it and the deltas the date moved; a repeat returns the same result. Without the call, the next send or `/open` in one of the persona's text sessions settles the date, and waits for the judge call before its own work.
5. **Handle the new refusals.** They carry the usual pre-stream body `{code, message, user_message}`:

   | Code | Where | What to do |
   |---|---|---|
   | `409 activity_in_progress` | a send to a 1:1 text session of the persona during a date; a `date` action during a date | take the user to the date |
   | `409 activity_ended` | a send or an image edit in a date's sub-session after the date | go back to the main chat; call the end route for the result |
   | `409 activity_settling` | a send or the end route while another request settles the date | retry shortly |
   | `409 choice_pending` | a `date` action while she waits for an answer to a life choice | answer the choice first |
   | `400 byoc_unsupported` | a `date` action on a BYOC instance | do not offer dates there |
   | `404 activity_not_found` | the end route, for an activity that is not a date of that session | check the ids: `session_id` is the parent's |

6. **Catalog the new values.** `on_a_date` is an event kind the engine places itself; naming it in an `/events` `kinds` filter is a `422`. Session lists (`GET /comp/chat/{user_id}/sessions`) leave sub-sessions out.

### A client that does not adapt

A client that never sends `date` sees no change of its own. When a persona instance is shared with a client that does, the unadapted one sees `409 activity_in_progress` on its sends while a date runs and `greeting: null` from `/open`; it can treat the code as a busy state and send again after the date's `ends_at`, which `GET …/effects` carries.

## Deployments

- **Model config (optional).** `[tasks.activity_judge]` sets the date judge's model chain. Without it the judge runs on `[tasks.affinity_evaluation]`'s chain and parameters. `max_tokens` is the configured or inherited value raised to at least 1200. A date with at least one substantive turn makes one judge call with that budget; a date with none makes no call. A `filter_prompt` in the block refuses to boot. The shipped `examples/model_config.toml` and `examples/model_config.d/affinity.toml` carry the block.
- **Env (optional).** `AFFINITY_ACTIVITY_TURN_SHARE` (default `0.25`), `AFFINITY_ACTIVITY_TURN_CAP_SHORT` (`12`) and `AFFINITY_ACTIVITY_TURN_CAP_MID` (`32`) set how much a date's verdict weighs ([affinity-model.md](../affinity-model.md#dates)).
- **Your PDE judge prompt** needs no change: the invitation reaches the judge as an ordinary `[用户动作]` line.

## Rollback

An image older than this one, against a database where dates have been opened:
- lists `activity` sessions in `GET /comp/chat/{user_id}/sessions`, with `channel: "activity"`, and refuses sends to them with `409 wrong_channel`;
- pauses nothing during a date and never settles one, so a date under way at rollback leaves no verdict and nothing in memory;
- skips `on_a_date` rows on read, as it skips any event kind it does not know;
- never returns an `activity` affinity event from the per-turn delta route, which reads `message`, `gift`, `proactive` and `ghost` only.

Deploy the older image without running its `migrate`: its migrator does not know `0085` and refuses a database that has it. The widened checks and `engine.activities` can stay; everything the older image writes satisfies them. Upgrading again settles a date left unsettled at the next send or `/open` in one of the persona's text sessions.

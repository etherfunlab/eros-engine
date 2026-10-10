# Migrating to the v2.1.2 tip action

**Applies to:** clients of `eros-engine` upgrading to `v2.1.2`.
**Design:** [`docs/superpowers/specs/2026-10-11-tip-as-action-design.md`](../superpowers/specs/2026-10-11-tip-as-action-design.md)

**A tip is now a user action, judged like every other.** The request field
`tips_amount_usd` is removed from `POST /comp/chat/{session_id}/message/stream`
and `POST /v2/comp/session/{session_id}/message/async`. Send the tip as an
action instead:

```diff
 {
   "content": "",
   "client_msg_id": "01J3333333333333333333333A",
-  "tips_amount_usd": 9.99
+  "action": {"type": "tip", "amount_usd": 9.99}
 }
```

`amount_usd` is required on `tip`, finite, and in `(0, 1_000_000_000]`.
A tip may now carry `image_url` and `image`, forced or not, like every action.

**If you do nothing, tips silently stop.** The request body does not deny
unknown fields: a stale `tips_amount_usd` is ignored. A turn that also has
text runs as a plain message; an empty one is rejected with `422`
(`content must not be empty`).

**The persona may refuse a tip.** The PDE judge decides before the reply, and
the turn emits the `action_response` frame (`accept` | `refuse`) before the
first `meta` frame, replayed on a repeated `client_msg_id`. A refused tip
costs the relationship nothing. What happens to the money is yours to decide:
the engine reports the decision and settles nothing.

**History.** Tip rows are `role = "user"` and carry
`action: {"type": "tip", "amount_usd": …, "response": …}` on both history
routes. `gift_user` is gone from `role`, and the BFF history's
`tips_amount_usd` field is removed. A tip row now gets a `read_at` receipt
like any text `user` row.

**Store.** Migration `0066` rewrites every existing `gift_user` row into this
shape (`response: "accept"`, the `(打赏 $X)` placeholder cleared, the user's
own text kept), removes `gift_user` from the role CHECK and drops the
`metadata->>'tips_amount_usd'` index. Anything that reads
`engine.chat_messages` directly must read `metadata.action` with
`type = 'tip'` instead of `role = 'gift_user'` or `metadata.tips_amount_usd`.

**Trust.** The engine records `amount_usd` and never verifies payment, as
before. A caller that forwards actions from its own clients must strip a
client-supplied `tip`; only the payment authority may set one.

**Rollout: the engine and the caller switch together.** The previous engine
rejects `type: "tip"`, and this one ignores `tips_amount_usd`. The migration
is forward-only: the previous image cannot write a tip against the new CHECK,
so rolling back means the previous image, the caller's previous version, and
an `ALTER` re-adding `gift_user` to `chat_messages_role_check`. An async tip
queued before the deploy and driven after it runs as a plain turn.

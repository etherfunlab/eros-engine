# An unread count on the group DTO (#539)

## Intent

A client that lists groups wants an unread badge on each row and one on the
entry to the list. Today it can only count by fetching every group's history
and paging back to the first read row, the per-group fetch `last_message` was
added to remove.

## Goals

- `GroupDto` gains `unread_count`: an integer, always present, `0` when
  nothing is unread or the group is empty.
- It counts the group session's rows that a `POST /comp/chat/{session_id}/read`
  call made now would stamp (not `role = 'user'`, `read_at IS NULL`) and that a
  client shows, under the same visibility rule as `last_message` (`role` is
  `user` or `assistant`, not `channel = 'product_qa'`, text after trimming or
  an image). Together: visible `assistant` rows with no `read_at`.
- Every route that returns `GroupDto` carries it: `POST /v2/comp/group`,
  `GET /v2/comp/groups`, `GET`/`PATCH /v2/comp/group/{group_id}`, and the
  member add/remove routes, since all build it through `group_dto`.

## Non-goals

- 1:1 sessions: no count on their routes.
- No notification; no change to the read route.
- No cap on the count; a badge's `99+` is the client's.

## Approaches

1. **A count subquery in `GROUP_SQL` (picked).** The count rides the one query
   that already reads `last_message`, with the visibility rule pulled into one
   SQL fragment both use, so the badge and the preview never disagree on what
   a client shows.
2. **A separate count query per group in `with_members`.** One more round trip
   per group on every list load, for no gain.
3. **A partial index on unread rows plus either of the above.** A migration
   for a list a user holds a handful of groups in; it can be added later
   without changing behavior.

## Decided

- No migration. The subquery runs on `idx_chat_messages_session`
  (`session_id, sent_at`); a partial index on unread rows is the lever if it is
  ever slow.
- The visibility predicate is one constant in `crates/eros-engine-store/src/group.rs`,
  used by the `last_message` lateral and by the count.
- Docs (`group-chat.md`, `api-reference.md`, their `.zh` pages) go in a
  separate PR after the code PR merges.

## Left to the builder

Names of the constant and the column alias; the integer type (`i64` from
`count(*)`).

## Tests

- A group with assistant rows, some read: the count is the unread visible ones;
  a user row, an empty-content row, a product-answer row and a `system_error`
  row never count.
- `POST …/read` on the group brings it to `0`; a later assistant row brings it
  to `1`.
- A new group and a group with only read rows: `0`.
- Both `GET` routes return the same count for the same group.

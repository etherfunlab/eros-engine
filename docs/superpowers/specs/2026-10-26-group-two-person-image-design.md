# Group chat: a picture of two — Design

- **Date:** 2026-10-26
- **Status:** Approved
- **Type:** Engine change. No migration. Additive API changes: one optional
  field on the group director's verdict entry; one optional object on the
  `image_request` frame and on `GET …/image-request`. The 1:1 path is
  byte-identical. Ships in v2.3.1.
- **Owner:** enriquephl (sole dev)
- **Builds on:** `2026-10-12-group-chat-design.md` (*the group spec*: the
  director, cues, the per-speaker compose) and the delegated image path
  (`image_request`, `[tasks.chat_image_prompt_compose]`).

## 1. Why

When a group speaker sends a picture, the composer gets that speaker alone:
her appearance, her clothing, the scene. The director cannot say "she is in
the picture with him", the frame carries one `image_ref` (her face), and a
client can only hand the image model one face as reference. A group is the
one place two personas are in the same room; its pictures should be able to
show two. The cap is two: the speaker and one companion.

## 2. The director names a companion

The verdict entry gains `with`: the handle (`m1`, `m2`, `m3`) of the member
who is in the picture with the speaker, or `null`. It is required in the
strict schema like the other optional fields (`"type": ["string", "null"]`,
enum of the seated handles plus `null`), so a strict model always answers
it. The engine-owned user message (`director_context`) tells the director
what it means, in one line after `[图片能力]`, only on a turn where a picture
is available (a text-only turn's bytes are unchanged):

```
[同框] 发图的发言人可用 with 指定一位同框成员（m1/m2/m3），独自一人或不发图时为 null
```

The shipped `examples/model_config.toml` director prompt's output shape
gains `"with": null`.

`normalize` turns it into `Cue.with: Option<usize>` (a seat index) and drops
it silently, never erroring, when:

- the handle is not a seated member;
- it is the speaker's own seat;
- the entry's action, after the image-availability downgrade, carries no
  picture (`reply_text`).

The rule fallback (no director) never sets it. The director audit row keeps
the verdict as the model wrote it (the raw `with` included), and the
`inputs` snapshot is unchanged.

## 3. The composer sees both

When a cue carries a companion, the compose payload gains one section after
`[此刻穿着]`:

```
[同框人物]
{name}：{appearance or （无）}；{tip_personality or （无）}
这张图里有两个人：{speaker name} 和 {name}。
```

The composer's audit `inputs` gain `companion: {name, appearance,
personality}`. Without a companion the payload and the audit are
byte-identical to today's. The built-in composer system prompt
(`DEFAULT_COMPOSE_PROMPT`) gains one sentence: an optional `[同框人物]`
section names a second person who is in the picture with the character, and
the subject then describes both. That sentence is sent on the 1:1 path too
(the prompt is one constant); what stays byte-identical there is the payload
and the frames. The final wire prompt still layers style, the
speaker's appearance and the composed subject: the companion's look reaches
the subject through the composer, not through a second appearance block, so
a deployment without a composer configured gets the portrait fallback of the
speaker alone, as now.

## 4. The frame and the restore route carry the companion

`image_request` (the SSE frame, on image-only and text+image group turns)
and `GET /comp/chat/{session_id}/messages/{message_id}/image-request` gain
`companion`, present only when there is one:

```json
{"type": "image_request", "message_id": "…", "composed_prompt": "…",
 "image_ref": "face", "aspect_ratio": "3:4",
 "companion": {"member_session_id": "…", "instance_id": "…"}}
```

A client fetches the companion's face reference by `instance_id` the way it
fetches the speaker's, and draws with both. The persisted image marker on
the assistant row stores the same object so the restore route can serve it;
rows written before this carry none. A replay never re-emits
`image_request` (the group spec), so there is nothing to replay.

`companion` rides the frame and the marker only when the composer produced
the subject (`compose_model` is set). When the composer chain is exhausted
the wire prompt is the speaker's solo portrait, and a client must not be
handed two faces for it; the composer audit `inputs` still record the
companion that was asked for.

## 5. Out of scope

- Three or more people in one picture.
- The 1:1 path: no `with`, no `companion`, no schema change there.
- Persona-to-persona relationship text in the compose payload beyond the
  names and personalities: the engine keeps no such fact outside the
  optional world system.
- Changing the group's memory writes or who speaks.

## 6. Testing

- Core: `normalize` keeps `with` for a seated other member on an image
  action; drops it for a stranger handle, the speaker's own handle, a text
  action, and an image action downgraded by `image_available = false`.
- Server: the director schema lists `with` with the seated handles and
  `null`; the compose payload with a companion carries the section in the
  stated place and is byte-identical without; the frame and the marker carry
  `companion` on a group image-only turn and a text+image turn, and the
  restore route returns it; a 1:1 turn's frame and marker carry no
  `companion` key.

## 7. Documentation

`docs/group-chat.md` and `.zh.md`: the director's verdict entry (`with`),
the compose section, the `companion` object on the frame and the restore
route, and the drop rules. `docs/api-reference.md` and `.zh.md`: the
`image_request` frame and the restore payload gain the optional
`companion`. The v2.3.1 release notes.

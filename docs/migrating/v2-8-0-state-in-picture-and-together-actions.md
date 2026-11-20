# Migrating to v2.8.0: her state in the picture, three together actions, a toilet event, four give items

**Applies to:**
- deployments with a custom `filter_prompt` on `[tasks.chat_image_prompt_compose]`;
- clients that keep their own catalog of event kinds, action types or give items, or that match one of them exhaustively;
- callers of `POST /v2/comp/session/{session_id}/events` that list kinds in `kinds`;
- anything that reads `engine.chat_images_events.inputs`;
- crate users who call `ChatRepo::set_action_response` or match on `EventKind`, `UserAction` or `GivenItem`;
- deployments (one migration, `0086`, indexes only).

Every HTTP change is additive: new values in enums a client may match exhaustively, and a picture on an accepted together action. Migration `0086` has no ordering constraint with the new image.
**Design:** [`2026-11-20-together-actions-state-in-picture-and-genome-routes-design.md`](../superpowers/specs/2026-11-20-together-actions-state-in-picture-and-genome-routes-design.md)

The rules are in [the composer](../model-config.md#taskschat_image_prompt_compose--image-prompt-composer-required-for-image-turns), [together actions](../life-events-needs.md#showering-bathing-and-the-sauna-together), [using the toilet](../life-events-needs.md#using-the-toilet) and [the send route](../api-reference.md#post-compchatsession_idmessagestream). The genome routes that ship in the same release have [a guide of their own](v2-8-0-genome-write-routes.md).

## What users will notice

- **A picture shows what she is doing.** While an event is live, every picture the chat pipeline draws shows it: a chat turn's, an event message's own, and a group turn's, which shows its speaker's state. A picture drawn on the train shows the train. Where what she is doing and what she has on disagree, the state wins.
- **She can be on the toilet.** `using_toilet` is a `short` incident of 30 minutes, told like any other, with no picture. A robot or android persona never draws it.
- **The user can ask to shower, bathe or take a sauna with her** (`shower_together`, `bath_together`, `sauna_together`). The judge decides.
  - A shower or a bath is refused before the judge when the pair is not romance-compatible, as `strip` is. A sauna has no such rule.
  - A refusal drops chemistry one tier and places nothing.
  - An accepted one puts her in an event: `showering_together` (30 minutes), `bathing_together` (30 minutes) or `sauna_together` (1 hour). The incident under way ends; a background such as a trip keeps going. Inside a date nothing is placed and the date goes on.
  - An accepted one comes with her picture when the turn carries `image` and the deployment has the composer, as an accepted `photo` does. The picture shows the event.
  - Her reply to a shower or a bath is in the `emotional` tone when the deployment has [`tones.emotional`](../model-config.md#tones--chat-tones-chat-task-only) above 0; a sauna's is not.
- **Four more things to hand her**: `car`, `motorcycle`, `boat` and `horse`. They are valuables, like `jewelry`: nothing lingers on her and no need moves.

## Deployments

### The composer prompt

The composer's payload gains a section while an event is live:

```
[身体状态]
（无）

[此刻状态]
短途旅游；在坐火车

[最近场景]
…
```

`[此刻状态]` lists the labels of the background and then the incident under way, joined by `；`. It sits after `[身体状态]` and before `[最近场景]`. With nothing live it is absent and the payload is exactly what it was.

- **The built-in prompt** names the section, says the picture must show what it says, and says that the state wins where it disagrees with `[此刻穿着]`. Nothing to do.
- **A custom `filter_prompt`** should carry a line for it. Without one the model still reads the section, but nothing tells it what the section is for or that it outranks `[此刻穿着]`. A line such as this does it: "When a [此刻状态] section is present, the picture must show what it says; where it disagrees with what the character is wearing, the state wins and the character is dressed as that state has them."
- The engine does not rewrite `[此刻穿着]` for any kind.
- The standalone compose endpoint and the image-edit endpoint send no such section.
- `engine.chat_images_events.inputs` gains `current_state`: the section's text, `""` when it was absent, and always `""` on the standalone endpoint. A reader that selects the keys of `inputs` by name keeps working.

No new setting.

### Migration `0086`

`0086_fk_indexes` creates nine indexes with `CREATE INDEX IF NOT EXISTS` and does nothing else. It adds no column and rewrites no row. Each covers a foreign key that no index led with, so that deleting a persona instance, a story event or a chat session finds its dependents by index instead of scanning the table:

| table | column |
|---|---|
| `engine.companion_affinity` | `instance_id` |
| `engine.companion_memories` | `instance_id` |
| `engine.companion_memories` | `session_id` |
| `engine.chat_turn_queue` | `session_id` |
| `engine.persona_story_memories` | `event_id` |
| `engine.persona_story_memories` | `instance_id` |
| `engine.world_memories` | `instance_id` |
| `engine.world_post_comments` | `author_instance_id` |
| `engine.world_posts` | `instance_id` |

No ordering constraint: the previous image runs against a database that has the indexes, and the new image against one that does not. Run `migrate` as for any release ([deploying](../deploying.md#operational-notes)). The build is a plain `CREATE INDEX`, which blocks writes to its table while it runs; on a deployment where `engine.companion_memories` or `engine.chat_turn_queue` is large, run it outside peak traffic. Existing indexes stay as they are.

### Rollback

Roll back the image; the indexes can stay. On the `v2.7.3` image:
- an event of a new kind written by the new image (`using_toilet`, `showering_together`, `sauna_together`) reads as no event, as any event kind the build does not know;
- an action of a new type on a stored row reads as no action;
- a request that sends a new action type or give item answers `422`.

Deploy the older image without running its `migrate`: its migrator does not know `0086` and refuses a database that has it.

## Clients

1. **Add the three action types** if your client offers them. None carries a field. The answer arrives as for any action: the `action_response` frame, then `response` on the stored row in both history routes.
2. **Expect a picture on an accepted together action.** With `image` in the request and the composer configured, the frames run `action_response → meta(reply_text_image) → delta* → done → image_request → final`, as for an accepted `photo`, `strip` or `ask_dancing`. A refusal, and a deployment without the composer, are text only. A client that already draws the `image_request` of an accepted `photo` needs no new code.
3. **Add the new event kinds to your catalog** before or with the upgrade:

   | kind | label | how it arrives |
   |---|---|---|
   | `using_toilet` | 上厕所中 | drawn: an engine on the new image draws it for any caller that does not list `kinds` |
   | `showering_together` | 和对方一起淋浴 | placed by an accepted `shower_together`; never drawn |
   | `sauna_together` | 和对方一起蒸桑拿 | placed by an accepted `sauna_together`; never drawn |

   A client that drops kinds it does not know shows nothing for them. The two placed kinds appear as `event` on `GET /v2/comp/session/{session_id}/effects` while they last, and `changes_at` counts their end. Naming either in a `kinds` filter is a `422`. `bathing_together` is now also placed by an accepted `bath_together`, not only by her own ask.
4. **Check your `kinds` filter.** A caller that allow-lists adds `using_toilet` to draw it. A caller that disallows draws it unless it names it there.
5. **Add the four give items** if your client offers them: `car` (辆), `motorcycle` (辆), `boat` (艘), `horse` (匹).
6. **Match enums leniently.** `action.type`, a give's `item` and an event's `kind` gain three, four and three values.

## Crate users

- `ChatRepo::set_action_response` takes a fifth parameter, `place: Option<&eros_engine_store::events::NewEvent<'_>>`: the event an accepted together action puts her in. It is written in the same transaction as the stored answer, once per `event_key`, and never while a date of the instance is on. Pass `None` for the old behaviour.
- `EventKind` gains `UsingToilet`, `ShoweringTogether` and `SaunaTogether`. `EventKind::ALL` is now `[EventKind; 70]` and `EventKind::SPECIAL` `[EventKind; 7]`: a signature or a constant that spells the length changes, and an exhaustive `match` adds the arms.
- `UserAction` gains `ShowerTogether`, `BathTogether` and `SaunaTogether`, and the method `together_kind()`, which names the event an accepted one places. `GivenItem` gains `Car`, `Motorcycle`, `Boat` and `Horse`.
- `eros_engine_core::pde::together_ruled_out` is the pre-judge rule for a shower or a bath, beside `strip_ruled_out`.

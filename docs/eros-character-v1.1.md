# Eros character card — v1.1

An Eros character card is a JSON document that describes one character. It is
the plaintext a downstream seals and sends with each request against a
bring-your-own-character (BYOC) instance; see [byoc.md](byoc.md) for the
sealing and the routes.

v1.1 has exactly the fields of a persona genome, so a persona seed file
([`examples/personas/*.toml`](../examples/personas/)) converts to a card field
for field: the top-level keys go into `data`, the `[art_metadata]` table into
`data.art_metadata`.

## Document

```json
{
  "spec": "eros-character",
  "spec_version": "1.1",
  "data": {
    "name": "Aria",
    "system_prompt": "You are Aria, a 27-year-old rare-books librarian …",
    "tip_personality": "warm-but-reserved",
    "art_metadata": {
      "gender": "female",
      "age": 27,
      "timezone": "America/Chicago",
      "birthday": "11-03",
      "appearance": "late-20s woman, auburn hair often loosely tied back, green eyes"
    }
  }
}
```

| Field | Type | Required | Meaning |
|---|---|---|---|
| `spec` | string | yes | Always `"eros-character"`. |
| `spec_version` | string | yes | `"1.1"`. A `"1"` card is read the same way. |
| `data.name` | string | yes | Non-blank. The character's name wherever the engine names her. |
| `data.system_prompt` | string | yes | Non-blank. The character definition, placed in the reply prompt as written. |
| `data.tip_personality` | string \| null | no | How she closes a tip turn. Absent, null or blank: no closing line. |
| `data.art_metadata` | object | no | Character settings, below. Absent means `{}`. |

Unknown keys at the top level or in `data` are rejected. The whole document is
at most 64 KiB of UTF-8.

## `art_metadata`

An open object. The engine reads the keys below and ignores any other key. A
value of the wrong shape reads as absent.

| Key | Type | Meaning |
|---|---|---|
| `gender` | string | `male` or `female`; any other word is rendered as written. |
| `age` | integer, or a string holding one | Her age. |
| `species` | string | `human`, `alien`, `beastkin`, `elf`, `deity`, or any other word. Absent means human. |
| `planet` | string | Free text. `earth` or absent renders no planet. |
| `hometown` | string | Free text. |
| `timezone` | string | IANA zone of her clock. Absent: the request's `user_timezone`, else `Asia/Singapore`. |
| `birthday` | string | `"MM-DD"` on her clock. |
| `mbti` | string | MBTI type. |
| `backstory` | string | Free text. |
| `speech_style` | string | Free text. |
| `quirks` | array of strings | Habits and tics. |
| `topics` | array of strings | What she likes to talk about. |
| `appearance` | string | Visual description seeded into image prompts. |
| `possessions` | array of strings | Things she owns, merged with the instance's item rows. |
| `wardrobe` | array of strings | Clothes she can wear, merged with the instance's item rows. |
| `smoking`, `alcoholic`, `drug`, `coffee`, `gluttony` | boolean | Whether she smokes, drinks, uses weed, drinks coffee, loves good food: how she takes a `give` of each, how fast her craving for what she's hooked on builds, and what it names. `smoking`, `alcoholic` and `drug` set to `true` also let life events draw her smoking, drinking and smoking weed. See [life-events-needs.md](life-events-needs.md#persona-settings). |
| `health` | string | `weak`, `normal` or `strong`. Read by life events. |
| `prefer_trip` | boolean | Read by life events. |
| `prefer_events` | array of strings | Read by life events. |
| `homesick` | boolean | Read by life events. |
| `is_virgin` | boolean | Read by life events. |
| `sex_addict` | boolean | Read by life events: only `true` draws `having_sex` and `masturbating`. |
| `hygienic` | boolean | Whether she loves being clean. Read by life events, and by when she asks the user to take a bath together. |
| `employed` | string | `free`, `part_time` or `full_time`: how much of her day is work. Absent or any other value reads as `part_time`. Read by life events. |
| `outdoorsy` | string | `outdoor`, `neutral` or `indoor`: whether she leans outdoors or stays in. Absent or any other value reads as `neutral`. Read by life events. |
| `setting` | string | `modern` or `fantasy`: the world she lives in, which decides the life events that fit her. Absent or any other value reads as `modern`. Read by life events. |

The last ten are accepted and have no effect on a BYOC instance, since
BYOC does not serve life events yet.

## Versions

- **1.1** adds the `gluttony` habit key and the `setting`, `hygienic`,
  `employed` and `outdoorsy` keys. Nothing else about the document changes.
- **1** is the first version, documented in [eros-character-v1.md](eros-character-v1.md).
  The engine still accepts it, and reads these keys on a `"1"` card too, since
  `art_metadata` is an open object.

The engine reads `spec` and `spec_version` first and rejects a pair it does
not know. A later version of this format raises `spec_version`; a card in
another format carries that format's own `spec` value.

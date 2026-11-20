# Migrating to v2.8.0: creating and updating a persona genome over HTTP

**Applies to:**
- a downstream server that writes `engine.persona_genomes` directly;
- deployments that want the routes (one new setting, `GENOME_WRITE_SECRET`).

No migration, and nothing changes until `GENOME_WRITE_SECRET` is set: the routes are not mounted, and their paths answer as any unknown path does.
**Design:** [`2026-11-20-together-actions-state-in-picture-and-genome-routes-design.md`](../superpowers/specs/2026-11-20-together-actions-state-in-picture-and-genome-routes-design.md)

A downstream server that authors personas can now create and overwrite a genome through two routes under `/svc/v1/genomes`, and stop writing the table itself. The contract is in [the API reference](../api-reference.md#post-svcv1genomes); the card they take is [eros-character](../eros-character-v1.1.md).

## What users will notice

Nothing. The routes serve the downstream server that authors personas, not end users.

## Deployments

1. **Pick a secret** of at least 32 bytes. A shorter one refuses to boot; a blank value counts as unset. Keep it on the downstream's server.
2. **Set `GENOME_WRITE_SECRET`** on the engine and restart. The downstream sends it as `x-genome-write-secret`. Without the setting the sub-tree is not mounted: its paths answer exactly as any unknown path does, never `403`, never `200`.
3. **Keep it apart from the other `/svc/v1/*` secrets.** Each sub-tree has its own, so a holder of an export secret cannot write a genome and the other way round ([deploying](../deploying.md#operational-notes)).

The routes check no ownership: no user is on the request, and the secret is the only gate. Whoever holds it can overwrite any genome, so it belongs on a server and never in a client.

## The routes

| | route | body | answers |
|---|---|---|---|
| create | `POST /svc/v1/genomes` | `{"id"?: uuid, "card": {…}}` | `201 {"id"}`; `200 {"id"}` when that id already exists |
| update | `PUT /svc/v1/genomes/{id}` | `{"card": {…}}` | `204`; `404` when no genome has the id |

Both answer `403` for a missing or wrong header, checked before the body or the path, and `400` for a body or a card the engine refuses. The path `id` of an update must be a UUID, else `400`.

`card` is the whole eros-character document: `spec` `"eros-character"`, `spec_version` `"1"` or `"1.1"`, and `data` with `name`, `system_prompt`, `tip_personality` and `art_metadata`. It is checked by the function that opens a BYOC card, with the same 64 KiB limit on the serialised document:
- `name` and `system_prompt` are non-blank strings;
- `tip_personality` is a string or `null`;
- `art_metadata` is an object, or `null` for `{}`;
- an unknown field at the top level, in `data` or in the request body is refused;
- a NUL character in any string or key of the card is refused, since the database cannot store one.

The `400` message is fixed and names the rule that failed. Card text is never quoted back.

A genome has no other column to set: the routes write the four card columns, and a create takes the `id` the caller names or mints one.

### Moving off direct writes

1. **Create with an `id` you choose** and retry freely. A `200` means a genome with that id already exists and nothing was written: it keeps its first columns even when the retried card differs. Without `id` the engine mints one and a retry makes another genome.
2. **Change a genome with `PUT`.** It replaces the four columns in place and keeps the `id` and `created_at`. A `404` means the id is unknown; create it first.
3. **Do your own checks first**: who may author or edit a persona, whether a genome is published, any versioning. The engine holds none of that.
4. **Reads and deletes are not here.** There is no route to read, list or delete a genome; the `seed-personas` subcommand is unchanged.

## Rollback

Unset `GENOME_WRITE_SECRET`, or roll back the image: the routes disappear and answer as unknown paths. Genomes already written stay, and are ordinary rows of `engine.persona_genomes`.

# Migration guides

One guide per engine release that requires downstream work — a removed endpoint,
a changed response shape, a dropped column, or a deployment step that must
happen in a particular order. If an upgrade is drop-in, it gets no file here.

**This directory starts at `v1.3.1`.** Breaking changes in earlier releases are
not backfilled — for those, read `docs/` and the release notes.

Guides are named `<subsystem>-v<engine release>.md`, with dots written as
hyphens: `affinity-v1-3-1.md`.

When the subsystem carries its own model version that moves independently of
the engine release, put that between the two — `affinity-4-0-v1-3-1.md` reads
"the Affinity 4.0 surface, as it changes in `v1.3.1`". This matters exactly
when the two numbers can disagree: the same model version can be reshaped by
several releases, and a release can ship without touching the model at all, so
the name has to say which pair it means.

Do not invent a model code name for a subsystem that has none. If the change is
to a subsystem with a single version — its own — `<subsystem>-v<release>` is
the whole name.

**English only, by design.** These files are written to be handed to a coding
agent, and an LLM-translated copy of a technical document drifts from the
original without anyone noticing which version is authoritative. Machine
translation is one step away for a human reader who wants it; a stale second
copy in the repository is not. The same convention governs
`docs/superpowers/specs/`.

User-facing product documentation is different and stays bilingual — see the
`.zh.md` pairs in `docs/`.

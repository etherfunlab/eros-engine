# Migrating off `[tasks.*].reasoning` (v1.9.2)

**Applies to:** any deployment whose `model_config.toml` (or any file under
`MODEL_CONFIG_DIR`) carries a `reasoning = { … }` key inside a `[tasks.<name>]`
block. The engine refuses to boot on that key from this release on.
**Reference:** [`docs/model-config.md`](../model-config.md), section
"`[[providers.<name>.body]]` — custom body parameters".

**No database change, no deployment ordering.** This is a config edit made
before the upgrade is deployed; the new build then boots against it as usual.

---

## 1. What changed

Reasoning control used to have two entry points that wrote the same wire
field: the per-task `reasoning` object and a `[[providers.<name>.body]]` rule
carrying `params.reasoning`. The rule already won whenever both were set. The
per-task key is gone; the body rule is the only entry point.

The key is still parsed, purely so the boot gate can name it:

```
[tasks.chat_companion].reasoning was removed: reasoning control is an ordinary
body param on the provider entry —
[[providers.openrouter.body]]
tasks  = ["chat_companion"]
params = { reasoning = { enabled = false } }
(omit `tasks` to apply it to every task on that provider). Delete the
[tasks.*] key.
```

Every shape of the key is refused — a table, an empty table, an empty string,
a commented-out line that was uncommented by mistake. Blank is not lenient
here because the key is never read.

## 2. The edit

For each `[tasks.<name>]` block that has a `reasoning` key, move the value to
a body rule on the provider that serves that task and delete the key.

Before:

```toml
[tasks.chat_companion]
model     = "x-ai/grok-4"
reasoning = { enabled = false }

[tasks.pde_decision]
model     = "google/gemini-3.1-flash-lite"
reasoning = { enabled = false }
```

After:

```toml
[tasks.chat_companion]
model = "x-ai/grok-4"

[tasks.pde_decision]
model = "google/gemini-3.1-flash-lite"

[[providers.openrouter.body]]
tasks  = ["chat_companion", "pde_decision"]
params = { reasoning = { enabled = false } }
```

- `params.reasoning` is OpenRouter's `reasoning` object verbatim, so
  `{ exclude = true }`, `{ effort = "low" }` and any other field it accepts
  carry over unchanged.
- Tiers never had their own `reasoning`; they inherit whatever rule matches
  their task, exactly as before.
- A task served by a custom provider (`model = "m@venice"`) never received
  the per-task key at all — custom endpoints got the strict OpenAI subset. If
  such a provider wants a reasoning field, declare it on **that** entry's
  `[[providers.<name>.body]]`; a rule on `openrouter` does not reach it.
- Under `MODEL_CONFIG_DIR`, `[providers]` merges as one top-level key: put
  the rule in the file that already holds `[providers]`, not next to the
  task block it used to live in.

## 3. Verify

Boot the new build against the edited config. The gate runs with the other
config checks, before any network call, so a leftover key fails at startup
with the message above — a container that stays up past config load has
passed it.

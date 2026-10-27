# Migrating to v2.5.1: affinity-gated profile recall

**Applies to:** every deployment. Nothing on the HTTP surface, in the schema
or in the configuration changes; what reaches the chat prompt does, on the
first turn after the upgrade.
**Design:** [`2026-10-27-affinity-gated-profile-recall-design.md`](../superpowers/specs/2026-10-27-affinity-gated-profile-recall-design.md)

`memory_scope` is now a ceiling, not the whole answer: within it, the
relationship's time-decayed bond and chemistry tiers decide how much of the
user's profile a persona sees. Extraction, the fact ledger, dreaming and
every other write path are unchanged — only injection narrows. The rules are
in [Memory layers → Affinity gate](../memory-layers.md#affinity-gate) and
[Insights → Chat prompt](../insights.md#chat-prompt).

## What users will notice

- **New and cold relationships see less.** At tier 1 on both lines a
  persona recalls nothing from the cross-persona profile layer and one
  relationship memory per turn (text; voice the same), where it used to get
  up to ten grouped profile memories and three relationship memories. Both
  grow back with the higher tier, reaching today's values from tier 3.
- **`full` / `insights_only` callers:** the eight intimate 基础画像 fields
  appear one by one as their own affinity line climbs (`kinks` from
  chemistry 4, `mental_health` and `finance_status` from bond 4, and so on).
  The neutral fields are untouched.
- **Domain-tagged memories** (`sexuality`, `romance`, `family`, `finance`,
  `health` …) wait for the same tiers as their matching field, even inside
  the profile-layer K.
- **Voice** now reads the pair's affinity time-decayed, like text: on a pair
  idle for a while the relationship line can name a lower tier than before.
  Bootstrap snapshots already frozen in `chat_sessions.metadata` — including
  ones written by earlier releases — are filtered on every turn; nothing to
  migrate.
- **Image-edit text half** reads decayed affinity too.

## What to check

- **Your `memory_extraction` prompt's `domain` labels.** The engine routes
  on `sexuality`, `kink`, `romance`, `family`, `finance`, `health` and
  `mental_health`, matched trimmed and case-insensitively — the vocabulary
  of the in-repo example prompt. A prompt that emits other labels, or no
  `domain`, still works: those rows follow the tier ramp alone.
- **There is no switch.** The thresholds are code constants; a deployment
  that relied on every persona seeing every profile memory from the first
  turn gets the gated behavior with no way to opt out short of a code change.

## Rollback

Swap back to the `v2.5.0` image. Nothing is written differently, so there is
nothing to undo.

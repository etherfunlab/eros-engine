# Migrating to v2.9.0: dancing and massage together, and the passion attribute

**Applies to:**
- clients that send `ask_for_dance` (it is removed: send `dance_together`);
- clients that offer `french_kiss`, `tease`, `lick` or `touch` to a pair that is not romance-compatible;
- clients that keep their own catalog of action types or event kinds, or match one of them exhaustively;
- crate users who match on `UserAction` or `EventKind`, rely on the length of `EventKind::SPECIAL` or `EventKind::ENDS_ON_CHEMISTRY_DROP`, or call `pde::strip_ruled_out`.

No migration, no new request field and no new frame. The action-type and event-kind enums change.

**Design:** [`2026-11-28-dance-massage-and-passion-actions-design.md`](../superpowers/specs/2026-11-28-dance-massage-and-passion-actions-design.md)

The other note for this release is [the erotic tone](v2-9-0-erotic-tone.md).

## What users will notice

- **An incompatible pair's kiss, tease, lick or touch is refused.** `french_kiss`, `tease`, `lick` and `touch` carry the passion attribute, as does the new `ask_massage`. From a pair whose turn `affinity_scope` has no chemistry axis, each is refused before the judge and drops chemistry a tier, where there is a tier to drop. There is no bypass. A compatible pair's reach the judge as before.
- **An accepted one can read alluring.** With `tones.erotic` set, her reply to an accepted gesture with the attribute is a fixed moment of the `erotic` tone ([the erotic tone](v2-9-0-erotic-tone.md)). Refused, or a roll lost, it is what it was.
- **Dancing together is one action.** `ask_for_dance` is gone; `dance_together` is the invitation to dance together, and `dance_naked_together` its naturism twin.
- **Massage, three ways.** The user can ask her for a massage (`ask_massage`), give her one (`massage_together`), or undress and massage each other (`massage_naked_together`).

## Deployments

Nothing to run. Deploy the image as for any release, **after** the client has stopped sending `ask_for_dance` (see Clients).

- **A custom judge prompt.** The judge's `[用户动作]` line carries the new markers: `对方想和你一起裸舞`, `对方想让你帮忙按摩`, `对方想给你按摩`, `对方想和你脱了衣服互相按摩`. A gesture refused by the attribute's gate ends the line `，这次你不会答应`.
- **A custom composer prompt.** The two naked kinds put her in nothing, as every naturism kind does; their labels are `和对方一起裸舞` and `和对方脱了衣服互相按摩`. `massaging_together` reads `让对方给自己按摩`.

### Rollback

Redeploy the previous image. On it, a stored row whose action is one of the four new types reads as no action, and a request that sends one answers `422`; an event of one of the three new kinds reads as no event. Rows that stored `ask_for_dance` read as `ask_for_dance` again.

## Clients

1. **Stop sending `ask_for_dance` before you upgrade.** It answers `422` on v2.9.0. Send `dance_together`, which places `dancing_together` and comes with her picture when accepted. A history row that stored `ask_for_dance` reads back as `dance_together`, with its `response`.
2. **Gate the four gestures for an incompatible pair**, or expect them to be refused: the answer arrives as any refusal does (`action_response` frame `refuse`, `response` on the stored row).
3. **Add the new action types** if your client offers them. None carries a field.

   | action | places | label | group | lasts |
   |---|---|---|---|---|
   | `dance_naked_together` | `naked_dancing_together` | 和对方一起裸舞 | naturism | 1 h |
   | `massage_together` | `massaging_together` | 让对方给自己按摩 | general | 30 min |
   | `massage_naked_together` | `naked_massaging_together` | 和对方脱了衣服互相按摩 | naturism | 30 min |
   | `ask_massage` | nothing | — | gesture (passion) | — |

   Each together action is decided by the judge, places its kind on accept, ends the incident under way, and comes with her picture when the turn carries `image`. A refusal drops chemistry.
4. **Add the three event kinds** to your catalog. They are placed by the accepted actions and never drawn: naming one in a `kinds` filter is a `422`.
5. **Match enums leniently.** `action.type` loses one value and gains four; an event's `kind` gains three.

## Crate users

- `UserAction` loses `AskForDance` and gains `AskMassage`, `DanceNakedTogether`, `MassageTogether` and `MassageNakedTogether`; `DanceTogether` deserialises `"ask_for_dance"` as an alias. `UserAction::is_passion()` is new.
- `EventKind` gains `NakedDancingTogether`, `MassagingTogether` and `NakedMassagingTogether`. `EventKind::SPECIAL` is now `[EventKind; 40]` and `EventKind::ENDS_ON_CHEMISTRY_DROP` `[EventKind; 11]`: a signature or a constant that spells either length changes, and an exhaustive `match` adds the arms.
- `pde::strip_ruled_out` is replaced by `pde::romance_ruled_out(event)`, true for a `strip` or a gesture with the passion attribute from a pair that is not romance-compatible.

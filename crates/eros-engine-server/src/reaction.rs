// SPDX-License-Identifier: AGPL-3.0-only
//! Which emoji a user may react with (spec 2026-10-05-message-reactions-
//! design.md §4): exactly one standard emoji per the `emojis` data set,
//! stored fully qualified, optionally narrowed by `CHAT_REACTION_EMOJI`.

/// The emoji a string names, fully qualified (`❤` resolves to `❤️`); `None`
/// unless the whole string is exactly one emoji.
pub(crate) fn resolve(s: &str) -> Option<&'static emojis::Emoji> {
    emojis::get(s)
}

/// An emoji with its skin tone set to the default; the emoji itself when it
/// has no skin tones.
pub(crate) fn base(e: &'static emojis::Emoji) -> &'static emojis::Emoji {
    e.with_skin_tone(emojis::SkinTone::Default).unwrap_or(e)
}

/// A deployment's allowed range: base emoji in `CHAT_REACTION_EMOJI` order,
/// deduplicated. A request matches when its base is listed, so every skin
/// tone of a listed emoji is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReactionAllowlist {
    bases: Vec<&'static str>,
}

impl ReactionAllowlist {
    /// Parse the env value. Unset, blank, or only separators ⇒ `Ok(None)`:
    /// every standard emoji is allowed. An entry that is not exactly one
    /// emoji ⇒ `Err` naming it, which refuses boot.
    pub(crate) fn parse(raw: Option<&str>) -> Result<Option<Self>, String> {
        let mut bases: Vec<&'static str> = Vec::new();
        for entry in raw
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let e = resolve(entry).ok_or_else(|| {
                format!("CHAT_REACTION_EMOJI: {entry:?} is not a single standard emoji")
            })?;
            let b = base(e).as_str();
            if !bases.contains(&b) {
                bases.push(b);
            }
        }
        Ok((!bases.is_empty()).then_some(Self { bases }))
    }

    pub(crate) fn allows(&self, e: &'static emojis::Emoji) -> bool {
        self.bases.contains(&base(e).as_str())
    }

    pub(crate) fn listing(&self) -> &[&'static str] {
        &self.bases
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReactionError {
    NotAnEmoji,
    NotAllowed,
}

impl std::fmt::Display for ReactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ReactionError::NotAnEmoji => "emoji must be exactly one standard emoji",
            ReactionError::NotAllowed => "emoji is not allowed on this deployment",
        })
    }
}

/// Validate a request's emoji against the allowed range. `Ok` carries the
/// fully qualified form, which is what gets stored and returned.
pub(crate) fn validate(
    raw: &str,
    allow: Option<&ReactionAllowlist>,
) -> Result<&'static str, ReactionError> {
    let e = resolve(raw).ok_or(ReactionError::NotAnEmoji)?;
    match allow {
        Some(a) if !a.allows(e) => Err(ReactionError::NotAllowed),
        _ => Ok(e.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_accepts_exactly_one_emoji() {
        assert_eq!(resolve("👍").map(|e| e.as_str()), Some("👍"));
        assert!(resolve("👍👍").is_none());
        assert!(resolve("ok").is_none());
        assert!(resolve("👍 ok").is_none());
        assert!(resolve("").is_none());
        assert!(resolve("👨‍👩‍👧").is_some(), "ZWJ sequence");
    }

    #[test]
    fn skin_tones_resolve_and_reduce_to_their_base() {
        let toned = resolve("👍🏽").expect("a skin-tone variant is an emoji");
        assert!(toned.skin_tone().is_some());
        assert_eq!(base(toned).as_str(), "👍");
        let plain = resolve("🔥").unwrap();
        assert_eq!(base(plain).as_str(), "🔥", "no skin tones: itself");
    }

    #[test]
    fn validate_normalizes_unqualified_input() {
        assert_eq!(validate("❤", None).unwrap(), "❤️");
        assert_eq!(validate("❤️", None).unwrap(), "❤️");
    }

    #[test]
    fn validate_rejects_non_emoji() {
        assert!(matches!(
            validate("ab", None),
            Err(ReactionError::NotAnEmoji)
        ));
        assert!(matches!(
            validate("👍👍", None),
            Err(ReactionError::NotAnEmoji)
        ));
    }

    #[test]
    fn allowlist_unset_or_blank_allows_all() {
        assert_eq!(ReactionAllowlist::parse(None).unwrap(), None);
        assert_eq!(ReactionAllowlist::parse(Some("  ")).unwrap(), None);
        assert_eq!(ReactionAllowlist::parse(Some(" , ,")).unwrap(), None);
    }

    #[test]
    fn allowlist_refuses_an_unresolvable_entry_and_names_it() {
        let err = ReactionAllowlist::parse(Some("👍, nope")).unwrap_err();
        assert!(
            err.contains("CHAT_REACTION_EMOJI") && err.contains("nope"),
            "{err}"
        );
    }

    #[test]
    fn allowlist_matches_on_base() {
        let a = ReactionAllowlist::parse(Some("👍🏽, ❤, 👍"))
            .unwrap()
            .unwrap();
        assert_eq!(a.listing(), &["👍", "❤️"], "bases, deduplicated, env order");
        assert!(a.allows(resolve("👍🏿").unwrap()));
        assert!(a.allows(resolve("👍").unwrap()));
        assert!(a.allows(resolve("❤️").unwrap()));
        assert!(!a.allows(resolve("🔥").unwrap()));
        assert_eq!(
            validate("👍🏻", Some(&a)).unwrap(),
            "👍🏻",
            "the toned form is what is stored"
        );
        assert!(matches!(
            validate("🔥", Some(&a)),
            Err(ReactionError::NotAllowed)
        ));
    }

    #[test]
    fn mixed_tone_handshake_resolves() {
        // Multi-person skin tones are emoji too. If the data set groups them
        // under 🤝, an allowlist naming 🤝 admits them; record what it does.
        let e = resolve("🫱🏻‍🫲🏿").expect("a mixed-tone handshake is an emoji");
        let a = ReactionAllowlist::parse(Some("🤝")).unwrap().unwrap();
        assert!(
            a.allows(e),
            "base of {} is {}",
            e.as_str(),
            base(e).as_str()
        );
    }
}

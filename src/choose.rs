//! Deciding what `oxutrm <target>` does about the sessions a host offers.
//!
//! [`decide`] is the switcher spec's §3.1 table as one pure function: no
//! I/O, so every row is a test with nothing to fake. There is no case left
//! that asks on the line: with any session running, a bare connect lands in
//! a lobby, and the selector asks there (switcher spec §1.1).

use oxutrm_proto::{Choice, Name, OfferEntry};

/// The shortest id prefix `--attach` accepts, in characters.
///
/// A session id is 32 hex characters; nothing shorter than this is a prefix
/// anyone would type by accident, and it keeps `--attach a` from matching
/// half the sessions on a busy host.
const MIN_ATTACH_PREFIX: usize = 4;

/// `MIN_ATTACH_PREFIX`, spelled out for the refusal message: a digit
/// interpolated into that sentence reads worse than the word.
///
/// Its own constant rather than a comment's promise to remember.
/// `the_minimum_prefix_matches_its_spelled_out_word` is the guard.
const MIN_ATTACH_PREFIX_WORD: &str = "four";

/// What a connect resolves to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The choice is made; send it and move on.
    Chosen(Choice),
    /// Nothing to send. The reason is for the user, printed as-is before raw
    /// mode and before this process exits.
    Refused(String),
}

/// The table, in the order the spec gives it: `--new` first, then
/// `--attach`, then the no-flags cases. `name` is `--name`, which `connect`
/// accepts only with `--new`.
pub(crate) fn decide(
    offered: &[OfferEntry],
    attach: Option<&str>,
    new: bool,
    name: Option<Name>,
) -> Decision {
    if new {
        return Decision::Chosen(Choice::New { name });
    }
    if let Some(x) = attach {
        return decide_attach(offered, x);
    }
    if offered.is_empty() {
        Decision::Chosen(Choice::New { name: None })
    } else {
        // Even exactly one: nothing is resumed silently (spec §1.1).
        Decision::Chosen(Choice::Lobby)
    }
}

/// `--attach <x>`: the one session named `x`, or the one whose id `x`
/// prefixes. A name always has a character outside `[0-9a-f]` (spec §2.3),
/// so `x` cannot be both, and no precedence is needed.
fn decide_attach(offered: &[OfferEntry], x: &str) -> Decision {
    if let Some(named) = offered
        .iter()
        .find(|e| e.name.as_ref().is_some_and(|n| n.as_str() == x))
    {
        return attach_to(named);
    }
    // Characters, not bytes: `--attach ää` is two characters.
    if x.chars().count() < MIN_ATTACH_PREFIX {
        return Decision::Refused(format!(
            "no session is named {x:?}, and an id needs at least \
             {MIN_ATTACH_PREFIX_WORD} characters. {}",
            describe_live(offered)
        ));
    }
    let matches: Vec<&OfferEntry> = offered.iter().filter(|e| e.id.starts_with(x)).collect();
    match matches.as_slice() {
        [] => Decision::Refused(format!(
            "no session is named {x:?} or has an id starting with it. {}",
            describe_live(offered)
        )),
        [one] => attach_to(one),
        many => Decision::Refused(format!(
            "{x:?} begins more than one session's id: {}. Give more characters.",
            many.iter()
                .map(|e| e.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Attach to `e`, unless it cannot be attached.
fn attach_to(e: &OfferEntry) -> Decision {
    if e.detachable {
        Decision::Chosen(Choice::Attach { id: e.id })
    } else {
        Decision::Refused(format!(
            "session {} is not detachable: it tunnels its data through the ssh \
             connection that created it, so it cannot outlive it and cannot be \
             reattached. Start a new session.",
            label(e)
        ))
    }
}

/// A session as a refusal names it: its name and id, or its id.
fn label(e: &OfferEntry) -> String {
    match &e.name {
        Some(n) => format!("{n} ({})", e.id),
        None => e.id.to_string(),
    }
}

/// What IS live, for a refusal to point at.
fn describe_live(offered: &[OfferEntry]) -> String {
    if offered.is_empty() {
        "There are no live sessions on this host.".to_owned()
    } else {
        format!(
            "Live sessions: {}.",
            offered.iter().map(label).collect::<Vec<_>>().join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::TermSize;

    const BUILD: &str = "3ff1218f5e0c4b7d9a1c2e3f40516273";
    const LOGS: &str = "3ff1a0c95b7d4c2e8f6a1b0c9d8e7f60";
    const OTHER: &str = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60";

    fn offer(id: &str, name: Option<&str>, detachable: bool) -> OfferEntry {
        OfferEntry {
            id: id.parse().unwrap(),
            name: name.map(|n| Name::parse(n).unwrap()),
            shell: "/bin/bash".to_owned(),
            created_unix: 1_791_450_840,
            size: TermSize {
                cols: 120,
                rows: 40,
            },
            detachable,
        }
    }

    fn attach(id: &str) -> Decision {
        Decision::Chosen(Choice::Attach {
            id: id.parse().unwrap(),
        })
    }

    fn refused(d: Decision) -> String {
        match d {
            Decision::Refused(why) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn nothing_offered_means_a_new_session() {
        assert_eq!(
            decide(&[], None, false, None),
            Decision::Chosen(Choice::New { name: None })
        );
    }

    #[test]
    fn one_session_or_more_lands_in_the_lobby() {
        let one = [offer(BUILD, Some("build"), true)];
        assert_eq!(
            decide(&one, None, false, None),
            Decision::Chosen(Choice::Lobby)
        );
        let three = [
            offer(BUILD, Some("build"), true),
            offer(LOGS, Some("logs"), true),
            offer(OTHER, None, false),
        ];
        assert_eq!(
            decide(&three, None, false, None),
            Decision::Chosen(Choice::Lobby)
        );
        // Even when the only session cannot be attached: the lobby lists it
        // dimmed and offers a new one.
        let tunnelled = [offer(OTHER, None, false)];
        assert_eq!(
            decide(&tunnelled, None, false, None),
            Decision::Chosen(Choice::Lobby)
        );
    }

    #[test]
    fn new_wins_over_everything_and_carries_its_name() {
        let offered = [offer(BUILD, Some("build"), true)];
        let name = Name::parse("logs").unwrap();
        assert_eq!(
            decide(&offered, None, true, Some(name.clone())),
            Decision::Chosen(Choice::New { name: Some(name) })
        );
    }

    #[test]
    fn attach_takes_an_exact_name() {
        let offered = [
            offer(BUILD, Some("build"), true),
            offer(OTHER, Some("logs"), true),
        ];
        assert_eq!(decide(&offered, Some("logs"), false, None), attach(OTHER));
        // A name is exact: a prefix of one is not it.
        assert!(refused(decide(&offered, Some("buil"), false, None)).contains("buil"));
    }

    #[test]
    fn attach_takes_an_id_prefix_in_lowercase_only() {
        let offered = [offer(BUILD, Some("build"), true), offer(OTHER, None, true)];
        assert_eq!(decide(&offered, Some("a3f9"), false, None), attach(OTHER));
        assert_eq!(decide(&offered, Some("a3f9c0"), false, None), attach(OTHER));
        assert_eq!(decide(&offered, Some(BUILD), false, None), attach(BUILD));
        // An id is lowercase hex. Uppercase is a name's, never an id's: were
        // it matched as a prefix too, `--attach CAFE` could mean two sessions.
        let why = refused(decide(&offered, Some("A3F9C0"), false, None));
        assert!(why.contains("A3F9C0"), "{why}");
    }

    #[test]
    fn an_uppercase_name_and_a_lowercase_id_prefix_never_both_match() {
        // `CAFE` is a legal name (it has a character outside [0-9a-f]), and
        // `cafe...` is a legal id. `--attach CAFE` is the name, unambiguously,
        // because the id does not start with `CAFE`.
        const CAFE_ID: &str = "cafe18f5e0c4b7d9a1c2e3f405162730";
        let offered = [offer(BUILD, Some("CAFE"), true), offer(CAFE_ID, None, true)];
        assert_eq!(decide(&offered, Some("CAFE"), false, None), attach(BUILD));
        assert_eq!(decide(&offered, Some("cafe"), false, None), attach(CAFE_ID));
    }

    #[test]
    fn an_ambiguous_prefix_is_refused_listing_the_candidates() {
        let offered = [offer(BUILD, None, true), offer(LOGS, None, true)];
        let why = refused(decide(&offered, Some("3ff1"), false, None));
        assert!(why.contains(BUILD) && why.contains(LOGS), "{why}");
        // One more character settles it.
        assert_eq!(decide(&offered, Some("3ff12"), false, None), attach(BUILD));
    }

    #[test]
    fn a_name_or_prefix_that_matches_nothing_says_what_is_live() {
        let offered = [offer(BUILD, Some("build"), true)];
        let why = refused(decide(&offered, Some("9999"), false, None));
        assert!(why.contains("9999"), "{why}");
        assert!(why.contains("build") && why.contains(BUILD), "{why}");
        let why = refused(decide(&[], Some("build"), false, None));
        assert!(why.contains("no live sessions"), "{why}");
    }

    #[test]
    fn a_session_that_cannot_be_attached_is_refused_with_the_reason() {
        let offered = [offer(OTHER, Some("tunnel"), false)];
        for x in ["tunnel", "a3f9"] {
            let why = refused(decide(&offered, Some(x), false, None));
            assert!(why.contains("ssh"), "{x}: {why}");
        }
    }

    #[test]
    fn a_short_string_that_is_no_name_is_refused_as_too_short() {
        let offered = [offer(BUILD, Some("build"), true)];
        let why = refused(decide(&offered, Some("3f"), false, None));
        assert!(why.contains("four"), "{why}");
        // Characters, not bytes: two characters, four bytes.
        let why = refused(decide(&offered, Some("ää"), false, None));
        assert!(why.contains("four"), "{why}");
    }

    #[test]
    fn a_short_name_is_still_a_name() {
        let offered = [offer(BUILD, Some("x"), true)];
        assert_eq!(decide(&offered, Some("x"), false, None), attach(BUILD));
    }

    /// `MIN_ATTACH_PREFIX_WORD` is prose, not derived from
    /// `MIN_ATTACH_PREFIX`. Change one without the other and this fails.
    #[test]
    fn the_minimum_prefix_matches_its_spelled_out_word() {
        assert_eq!(
            MIN_ATTACH_PREFIX, 4,
            "MIN_ATTACH_PREFIX_WORD says \"four\" -- change both together"
        );
    }
}

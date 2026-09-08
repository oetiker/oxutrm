//! Deciding what a bare `oxutrm <target>` should do about a live session, and
//! asking when the decision is not the client's to make alone.
//!
//! [`decide`] is the whole spec's table as one pure function: no I/O, so
//! every row is a test with nothing to fake. [`pick`] is the one path that
//! needs a human, and it is line I/O so it can run before raw mode -- like
//! everything else that may need the terminal (`ssh`'s own prompts, a
//! `Decision::Refused` message) does.

use std::io::{BufRead, Write};

use oxutrm_proto::{Choice, SessionSummary};

/// The shortest prefix `--attach` accepts, in characters.
///
/// A session id is 32 hex characters; nothing shorter than this is a prefix
/// anyone would type by accident, and it keeps `--attach a` from matching
/// half the sessions on a busy host.
const MIN_ATTACH_PREFIX: usize = 4;

/// `MIN_ATTACH_PREFIX`, spelled out for `decide_attach`'s refusal message: a
/// digit interpolated into that sentence reads worse than the word.
///
/// This is its own constant rather than a comment's promise to remember,
/// because a promise like that has already drifted false on this branch once.
/// `the_minimum_prefix_matches_its_spelled_out_word` is the guard: change
/// `MIN_ATTACH_PREFIX` without updating this, and that test fails rather than
/// the message quietly lying about the rule.
const MIN_ATTACH_PREFIX_WORD: &str = "four";

/// What a bare connect (or `--attach` / `--new`) resolves to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The choice is made; send it and move on.
    Chosen(Choice),
    /// More than one session is live and nothing else settled it: leave the
    /// terminal alone and ask.
    Ask,
    /// Nothing to send. The reason is for the user, printed as-is before raw
    /// mode and before this process exits.
    Refused(String),
}

/// The spec's table, in the order it gives it: `--new` first, then
/// `--attach`, then the no-flags cases.
pub(crate) fn decide(offered: &[SessionSummary], attach: Option<&str>, new: bool) -> Decision {
    if new {
        return Decision::Chosen(Choice::New);
    }

    if let Some(prefix) = attach {
        return decide_attach(offered, prefix);
    }

    match offered {
        [] => Decision::Chosen(Choice::New),
        [one] if one.detachable => Decision::Chosen(Choice::Attach {
            id: one.session_id.clone(),
        }),
        // A rung-4 session dies with its ssh, so resuming it is not an
        // option -- but that is not a reason to refuse the connect.
        [_one] => Decision::Chosen(Choice::New),
        _ => Decision::Ask,
    }
}

/// `--attach <prefix>`: exactly one candidate, or a refusal that says why.
fn decide_attach(offered: &[SessionSummary], prefix: &str) -> Decision {
    // Characters, not bytes: a session id is ASCII hex, so this only ever
    // affects garbage input, but `prefix.len()` would count `--attach ää` as
    // four when the user typed two characters, not what "at least four
    // characters" means.
    if prefix.chars().count() < MIN_ATTACH_PREFIX {
        return Decision::Refused(format!(
            "--attach needs at least {MIN_ATTACH_PREFIX_WORD} characters of a session id, got {prefix:?}"
        ));
    }

    let matches: Vec<&SessionSummary> = offered
        .iter()
        .filter(|s| s.session_id.starts_with(prefix))
        .collect();

    match matches.as_slice() {
        [] => Decision::Refused(format!(
            "no live session starts with {prefix:?}. {}",
            describe_live(offered)
        )),
        [one] if one.detachable => Decision::Chosen(Choice::Attach {
            id: one.session_id.clone(),
        }),
        [one] => Decision::Refused(not_detachable_reason(&one.session_id)),
        many => Decision::Refused(format!(
            "{prefix:?} matches more than one live session: {}. Give more characters.",
            many.iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Why a specific session cannot be attached to.
///
/// Shared between `decide_attach`'s refusal and `pick`'s own rejection of the
/// same case (a session offered as not-detachable, picked by number anyway),
/// so the two ways of reaching the same fact cannot say different things
/// about it.
fn not_detachable_reason(session_id: &str) -> String {
    format!(
        "session {session_id} is not detachable: it tunnels its data through \
         the ssh connection that created it, so it cannot outlive it and \
         cannot be reattached. Start a new session."
    )
}

/// What IS live, for a refusal to point at.
fn describe_live(offered: &[SessionSummary]) -> String {
    if offered.is_empty() {
        "There are no live sessions on this host.".to_owned()
    } else {
        format!(
            "Live sessions: {}.",
            offered
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Ask, on an ordinary terminal, before raw mode: a numbered list built from
/// the same fields `oxutrm_host::attach::format_session_list` uses (that
/// function lives on the host side and works from `SessionMeta`; this one
/// works from the client's own [`SessionSummary`], which is what is actually
/// in hand here), then `[1-N] resume, n new session, q quit`.
///
/// Loops on anything it does not understand. `Ok(None)` for `q` and for EOF
/// -- a client whose stdin is closed must exit rather than hang, and must not
/// pick something on the user's behalf.
pub(crate) fn pick(
    offered: &[SessionSummary],
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> anyhow::Result<Option<Choice>> {
    writeln!(out, "sessions live on this host:")?;
    for (i, s) in offered.iter().enumerate() {
        writeln!(
            out,
            "{:>2}) {}  {:>3}x{:<3}  attach {}  {}  {}",
            i + 1,
            s.session_id,
            s.size.cols,
            s.size.rows,
            s.attach_id,
            s.shell,
            if s.detachable {
                "detachable"
            } else {
                "NOT detachable (dies with its ssh)"
            },
        )?;
    }
    writeln!(out, "[1-{}] resume, n new session, q quit", offered.len())?;

    loop {
        write!(out, "> ")?;
        out.flush()?;

        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            // EOF, not a hang and not a panic.
            return Ok(None);
        }

        match line.trim() {
            "q" => return Ok(None),
            "n" => return Ok(Some(Choice::New)),
            other => match other.parse::<usize>() {
                Ok(n) if n >= 1 && n <= offered.len() => {
                    let chosen = &offered[n - 1];
                    if !chosen.detachable {
                        // The list already labelled this one "NOT
                        // detachable"; sending the choice anyway would just
                        // hand the host a request it has to refuse on its
                        // own. Same wording `decide_attach` refuses with for
                        // the identical fact, and the same re-ask as any
                        // other input that did not work out.
                        writeln!(out, "{}", not_detachable_reason(&chosen.session_id))?;
                        continue;
                    }
                    return Ok(Some(Choice::Attach {
                        id: chosen.session_id.clone(),
                    }));
                }
                _ => {
                    writeln!(out, "not understood: {other:?}. Try again.")?;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::TermSize;

    fn summary(id: &str, detachable: bool) -> SessionSummary {
        SessionSummary {
            session_id: id.to_owned(),
            created_unix: 1_757_200_000,
            shell: "/bin/sh".to_owned(),
            size: TermSize { cols: 80, rows: 24 },
            detachable,
            attach_id: 1,
        }
    }

    #[test]
    fn nothing_offered_means_a_new_session() {
        assert_eq!(decide(&[], None, false), Decision::Chosen(Choice::New));
    }

    #[test]
    fn exactly_one_detachable_session_is_resumed_without_asking() {
        // The case that sent a user to a new shell while their old one sat
        // parked. Asking here would be worse than deciding: there is only
        // one answer.
        let offered = [summary(&"a".repeat(32), true)];
        assert_eq!(
            decide(&offered, None, false),
            Decision::Chosen(Choice::Attach { id: "a".repeat(32) })
        );
    }

    #[test]
    fn exactly_one_session_that_cannot_be_attached_starts_a_new_one() {
        // A rung-4 session dies with its ssh. Resuming it is not possible, so
        // the useful thing is a new session, not an error.
        let offered = [summary(&"a".repeat(32), false)];
        assert_eq!(decide(&offered, None, false), Decision::Chosen(Choice::New));
    }

    #[test]
    fn several_sessions_ask() {
        let offered = [
            summary(&"a".repeat(32), true),
            summary(&"b".repeat(32), true),
        ];
        assert_eq!(decide(&offered, None, false), Decision::Ask);
    }

    #[test]
    fn new_wins_over_everything() {
        let offered = [summary(&"a".repeat(32), true)];
        assert_eq!(decide(&offered, None, true), Decision::Chosen(Choice::New));
    }

    #[test]
    fn a_prefix_selects_one_session() {
        let offered = [
            summary(&"abcd".repeat(8), true),
            summary(&"ef01".repeat(8), true),
        ];
        assert_eq!(
            decide(&offered, Some("abcd"), false),
            Decision::Chosen(Choice::Attach {
                id: "abcd".repeat(8)
            })
        );
    }

    #[test]
    fn an_ambiguous_prefix_is_refused_by_name() {
        let offered = [summary("abcd1111", true), summary("abcd2222", true)];
        match decide(&offered, Some("abcd"), false) {
            Decision::Refused(why) => {
                assert!(
                    why.contains("abcd1111"),
                    "the message must list the candidates: {why}"
                );
                assert!(
                    why.contains("abcd2222"),
                    "the message must list the candidates: {why}"
                );
            }
            other => panic!("an ambiguous prefix must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_prefix_that_matches_nothing_says_what_is_live() {
        match decide(&[summary("abcd1111", true)], Some("9999"), false) {
            Decision::Refused(why) => {
                assert!(
                    why.contains("9999"),
                    "the message must name what was asked for: {why}"
                );
                assert!(why.contains("abcd1111"), "and what is actually live: {why}");
            }
            other => panic!("a missing id must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_named_session_that_cannot_be_attached_is_refused_with_the_reason() {
        match decide(&[summary("abcd1111", false)], Some("abcd1111"), false) {
            Decision::Refused(why) => assert!(
                why.contains("ssh"),
                "the reason must say it dies with its ssh, not merely 'no': {why}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_prefix_shorter_than_four_characters_is_refused() {
        match decide(&[summary("abcd1111", true)], Some("ab"), false) {
            Decision::Refused(why) => assert!(why.contains("four"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// `MIN_ATTACH_PREFIX_WORD` is prose, not derived from
    /// `MIN_ATTACH_PREFIX` -- nothing in the language keeps them in step.
    /// This is the guard: change the number without updating the word (or the
    /// reverse), and this fails rather than the refusal quietly lying about
    /// the rule it enforces.
    #[test]
    fn the_minimum_prefix_matches_its_spelled_out_word() {
        assert_eq!(
            MIN_ATTACH_PREFIX, 4,
            "MIN_ATTACH_PREFIX_WORD says \"four\" -- change both together"
        );
    }

    /// A prefix is measured in characters, not bytes: `--attach ää` is two
    /// characters and four bytes, and "at least four characters" has to mean
    /// what it says even though session ids are always ASCII hex in practice.
    #[test]
    fn a_two_character_multibyte_prefix_is_still_refused_as_too_short() {
        match decide(&[summary("abcd1111", true)], Some("ää"), false) {
            Decision::Refused(why) => assert!(why.contains("four"), "{why}"),
            other => panic!(
                "\"ää\" is two characters (four bytes); a byte count would \
                 wrongly accept it, got {other:?}"
            ),
        }
    }

    #[test]
    fn the_picker_takes_a_number() {
        let offered = [
            summary(&"a".repeat(32), true),
            summary(&"b".repeat(32), true),
        ];
        let mut input = std::io::Cursor::new(b"2\n".to_vec());
        let mut out = Vec::new();
        let chosen = pick(&offered, &mut input, &mut out).expect("the picker runs");
        assert_eq!(chosen, Some(Choice::Attach { id: "b".repeat(32) }));
        let shown = String::from_utf8(out).expect("the prompt is text");
        assert!(
            shown.contains(&"a".repeat(32)),
            "the list must show the ids: {shown}"
        );
    }

    #[test]
    fn the_picker_takes_n_for_new_and_q_for_quit() {
        let offered = [summary(&"a".repeat(32), true)];
        let mut new_input = std::io::Cursor::new(b"n\n".to_vec());
        let mut out = Vec::new();
        assert_eq!(
            pick(&offered, &mut new_input, &mut out).expect("the picker runs"),
            Some(Choice::New)
        );
        let mut quit_input = std::io::Cursor::new(b"q\n".to_vec());
        let mut out = Vec::new();
        assert_eq!(
            pick(&offered, &mut quit_input, &mut out).expect("the picker runs"),
            None
        );
    }

    #[test]
    fn the_picker_re_asks_after_nonsense_and_quits_at_eof() {
        let offered = [summary(&"a".repeat(32), true)];
        let mut input = std::io::Cursor::new(b"banana\n7\n1\n".to_vec());
        let mut out = Vec::new();
        assert_eq!(
            pick(&offered, &mut input, &mut out).expect("the picker runs"),
            Some(Choice::Attach { id: "a".repeat(32) })
        );
        // A picker that silently discarded "banana" and "7" without telling
        // the user anything would still reach the same final answer, so the
        // return value alone does not prove it re-asked rather than ignored.
        let shown = String::from_utf8(out).expect("the prompt is text");
        assert!(
            shown.contains("not understood"),
            "the picker must say something about the input it rejected: {shown}"
        );
        // EOF is a quit, not a hang and not a panic: a client whose stdin is
        // closed must exit, and it must not pick something on the user's
        // behalf.
        let mut empty = std::io::Cursor::new(Vec::new());
        let mut out = Vec::new();
        assert_eq!(
            pick(&offered, &mut empty, &mut out).expect("the picker runs"),
            None
        );
    }

    /// A session the list already marked "NOT detachable" must stay refused
    /// even when picked by number, with the same reason `decide_attach` gives
    /// for the identical fact -- not silently forwarded to the host to fail
    /// on its own.
    #[test]
    fn the_picker_refuses_a_session_that_cannot_be_attached_and_re_asks() {
        let offered = [
            summary(&"a".repeat(32), false),
            summary(&"b".repeat(32), true),
        ];
        let mut input = std::io::Cursor::new(b"1\n2\n".to_vec());
        let mut out = Vec::new();
        assert_eq!(
            pick(&offered, &mut input, &mut out).expect("the picker runs"),
            Some(Choice::Attach { id: "b".repeat(32) })
        );
        let shown = String::from_utf8(out).expect("the prompt is text");
        assert!(
            shown.contains("not detachable") && shown.contains("ssh"),
            "the picker must explain the refusal, not merely skip to the next \
             answer: {shown}"
        );
    }
}

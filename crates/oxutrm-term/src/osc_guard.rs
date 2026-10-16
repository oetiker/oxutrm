//! A length cap on OSC strings, applied before the bytes reach the parser.
//!
//! `alacritty_terminal` builds `vte` with its `std` feature, and with that
//! feature `vte` collects an OSC body in a `Vec` with **no limit at all**: the
//! 1 KiB cap only exists in the `no_std` build. A child that prints `ESC ]`
//! and then never a terminator grows the host's memory one byte per byte it
//! writes, until the OOM killer picks a victim. A `cat` of a corrupt file is
//! enough; no malice is needed.
//!
//! The other string states are not affected: DCS passthrough hands each byte
//! to `Perform::put`, which `alacritty_terminal` drops, and SOS/PM/APC bodies
//! are discarded by the parser itself. CSI parameters and intermediates live
//! in fixed arrays, and the synchronized-update buffer stops at 2 MiB.

/// The longest OSC body passed on, in bytes.
pub(crate) const MAX_OSC_LEN: usize = 512 * 1024;

/// Sits between the PTY and the parser and holds every OSC body until it is
/// terminated.
///
/// Holding, rather than forwarding the body and aborting the parser when it
/// grows too long, is what makes an oversized OSC have no effect at all:
/// every way out of vte's OSC state dispatches what it has collected, so an
/// abort mid-body would still set a truncated title or store a truncated
/// clipboard copy. Here the parser is told to abort while its own body is
/// still empty, and an empty OSC dispatches to nothing.
///
/// The guard tracks only as much of vte's state machine as decides where an
/// OSC starts and ends, and it must agree with vte on exactly that:
///
/// - Only `ESC ]` opens one. vte stays in its escape state across a repeated
///   ESC, a C0 control, DEL and any byte from 0x80 up; anything else ends it.
///   The 8-bit introducer 0x9D opens nothing: vte executes it as a C1 control.
/// - BEL, CAN, SUB and ESC end one. ESC in any state starts a new escape.
/// - 0x9C (the 8-bit ST) does **not** end one. vte 0.15 stores it in the body.
pub(crate) struct OscGuard {
    state: State,
    /// The body of the current OSC, not yet passed on.
    held: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Not in an escape sequence the guard cares about.
    Ground,
    /// After an ESC: a `]` now opens an OSC.
    Escape,
    /// In an OSC body, holding it.
    Osc,
    /// In an OSC body that went over the cap: drop it up to its terminator.
    Discard,
}

const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const ESC: u8 = 0x1b;

/// What a buffer as large as the cap is shrunk back to after use, so one long
/// OSC does not pin half a megabyte for the rest of the session.
const KEEP_CAPACITY: usize = 4 * 1024;

fn ends_osc(b: &u8) -> bool {
    matches!(*b, BEL | CAN | SUB | ESC)
}

impl OscGuard {
    pub(crate) fn new() -> OscGuard {
        OscGuard {
            state: State::Ground,
            held: Vec::new(),
        }
    }

    /// Pass `input` on to `sink`, minus any OSC that grew past
    /// [`MAX_OSC_LEN`]. `sink` may be called several times; what it receives,
    /// concatenated, is the stream the parser is to see.
    pub(crate) fn feed(&mut self, input: &[u8], mut sink: impl FnMut(&[u8])) {
        // Start of the bytes passed on unchanged and not yet handed over.
        let mut start = 0;
        let mut i = 0;
        while i < input.len() {
            match self.state {
                State::Ground => match input[i..].iter().position(|&b| b == ESC) {
                    Some(at) => {
                        i += at + 1;
                        self.state = State::Escape;
                    }
                    None => i = input.len(),
                },
                State::Escape => {
                    let b = input[i];
                    i += 1;
                    match b {
                        b']' => {
                            // Hand over everything up to and including the
                            // introducer, so the parser's own body stays empty.
                            sink(&input[start..i]);
                            start = i;
                            self.state = State::Osc;
                        }
                        0x00..=0x17 | 0x19 | 0x1b..=0x1f | 0x7f..=0xff => {}
                        _ => self.state = State::Ground,
                    }
                }
                State::Osc | State::Discard => {
                    let rest = &input[i..];
                    let end = rest.iter().position(ends_osc);
                    let body = &rest[..end.unwrap_or(rest.len())];
                    if self.state == State::Osc {
                        if self.held.len() + body.len() > MAX_OSC_LEN {
                            self.release();
                            // Aborts the parser's OSC, which holds nothing.
                            sink(&[CAN]);
                            self.state = State::Discard;
                        } else {
                            self.held.extend_from_slice(body);
                        }
                    }
                    i += body.len();
                    start = i;
                    let Some(&b) = input.get(i) else { break };
                    if self.state == State::Osc {
                        sink(&self.held);
                        self.release();
                    } else if b == BEL {
                        // The parser is no longer in an OSC, where BEL is a
                        // terminator; passed on now, it would ring the bell.
                        start += 1;
                    }
                    // The terminator itself is passed on with what follows:
                    // ESC opens the next sequence, and vte executes CAN and
                    // SUB after an OSC just as it does anywhere else.
                    self.state = if b == ESC {
                        State::Escape
                    } else {
                        State::Ground
                    };
                    i += 1;
                }
            }
        }
        if start < input.len() {
            sink(&input[start..]);
        }
    }

    fn release(&mut self) {
        self.held.clear();
        self.held.shrink_to(KEEP_CAPACITY);
    }

    /// How many bytes of an unterminated OSC body are being held.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.held.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alacritty_terminal::Term;
    use alacritty_terminal::index::{Column, Line, Point};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;
    use oxutrm_proto::TermSize;

    use crate::grid::GridSize;
    use crate::listener::EventSink;

    /// `assert_eq!` on half a megabyte prints half a megabyte.
    fn same(got: &[u8], want: &[u8], what: &str) {
        let diff = got.iter().zip(want).position(|(a, b)| a != b);
        assert!(
            got == want,
            "{what}: got {} bytes, want {}, first difference at {diff:?}",
            got.len(),
            want.len()
        );
    }

    fn run(guard: &mut OscGuard, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        guard.feed(input, |b| out.extend_from_slice(b));
        out
    }

    /// `input` fed in one piece, and split at every possible point, must give
    /// `want` each time.
    fn assert_all_splits(input: &[u8], want: &[u8]) {
        same(&run(&mut OscGuard::new(), input), want, "in one read");
        for at in 0..=input.len() {
            let mut g = OscGuard::new();
            let mut out = run(&mut g, &input[..at]);
            out.extend(run(&mut g, &input[at..]));
            same(&out, want, &format!("split at {at}"));
        }
    }

    fn osc(body_len: usize) -> Vec<u8> {
        let mut v = b"\x1b]2;".to_vec();
        v.resize(v.len() + body_len - 2, b'x');
        v
    }

    #[test]
    fn ordinary_output_passes_through_unchanged() {
        let input = b"plain \x1b[1;31mred\x1b[0m \x1b(B \x1b]2;title\x07 \
              \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ \x1bP1$r\x1b\\ \xc3\xa9 \x9c end";
        assert_all_splits(input, input);
    }

    #[test]
    fn an_osc_split_across_reads_arrives_whole() {
        for term in [&b"\x07"[..], b"\x1b\\", b"\x18", b"\x1a"] {
            let mut input = osc(300);
            input.extend_from_slice(term);
            input.extend_from_slice(b"after");
            assert_all_splits(&input, &input);
        }
    }

    #[test]
    fn an_osc_exactly_at_the_cap_passes() {
        let mut input = osc(MAX_OSC_LEN);
        input.extend_from_slice(b"\x07after");
        let mut g = OscGuard::new();
        same(&run(&mut g, &input), &input, "at the cap");
        assert_eq!(g.held(), 0);
    }

    #[test]
    fn an_osc_one_byte_over_the_cap_is_dropped_whole() {
        let mut input = osc(MAX_OSC_LEN + 1);
        input.extend_from_slice(b"\x07after");
        let mut want = b"\x1b]".to_vec();
        want.push(CAN);
        want.extend_from_slice(b"after");
        let mut g = OscGuard::new();
        same(&run(&mut g, &input), &want, "over the cap");
        assert_eq!(g.held(), 0);
    }

    #[test]
    fn an_over_cap_osc_ended_by_st_keeps_the_st() {
        let mut input = osc(MAX_OSC_LEN + 10);
        input.extend_from_slice(b"\x1b\\after");
        let mut want = b"\x1b]".to_vec();
        want.push(CAN);
        want.extend_from_slice(b"\x1b\\after");
        // Split in the middle of the ST as well, which is where it hurts.
        for at in [3, MAX_OSC_LEN, MAX_OSC_LEN + 12, MAX_OSC_LEN + 13] {
            let mut g = OscGuard::new();
            let mut out = run(&mut g, &input[..at]);
            out.extend(run(&mut g, &input[at..]));
            same(&out, &want, &format!("split at {at}"));
        }
    }

    #[test]
    fn an_over_cap_osc_ended_by_can_or_sub_keeps_it() {
        // After an OSC, vte executes CAN and SUB, and SUB prints a
        // substitute; that much must still happen.
        for term in [0x18u8, 0x1a] {
            let mut input = osc(MAX_OSC_LEN + 1);
            input.push(term);
            input.extend_from_slice(b"after");
            let mut want = b"\x1b]".to_vec();
            want.push(CAN);
            want.push(term);
            want.extend_from_slice(b"after");
            same(&run(&mut OscGuard::new(), &input), &want, "over the cap");
        }
    }

    #[test]
    fn the_8_bit_st_does_not_end_an_osc_because_vte_ignores_it() {
        // vte 0.15 stores 0x9C in the OSC body. Mirroring that is what keeps
        // the guard and the parser agreeing on where a string ends.
        let mut input = osc(MAX_OSC_LEN - 1);
        input.push(0x9c);
        input.extend_from_slice(b"xx\x07after");
        let mut want = b"\x1b]".to_vec();
        want.push(CAN);
        want.extend_from_slice(b"after");
        same(&run(&mut OscGuard::new(), &input), &want, "over the cap");
    }

    #[test]
    fn only_esc_bracket_opens_an_osc() {
        let body = vec![b'x'; MAX_OSC_LEN + 1];
        // ESC ( ] designates a charset; the ] is no introducer.
        let mut input = b"\x1b(]".to_vec();
        input.extend_from_slice(&body);
        same(&run(&mut OscGuard::new(), &input), &input, "ESC ( ]");
        // vte stays in the escape state across a repeated ESC and across a
        // C0 control, so both of these do open an OSC.
        for prefix in [&b"\x1b\x1b]"[..], b"\x1b\x0a]"] {
            let mut input = prefix.to_vec();
            input.extend_from_slice(&body);
            let mut want = prefix.to_vec();
            want.push(CAN);
            same(
                &run(&mut OscGuard::new(), &input),
                &want,
                &format!("{prefix:?}"),
            );
        }
    }

    #[test]
    fn what_is_held_never_exceeds_the_cap() {
        let mut g = OscGuard::new();
        let mut out = run(&mut g, b"\x1b]52;c;");
        let chunk = vec![b'A'; 64 * 1024];
        for _ in 0..64 {
            out.extend(run(&mut g, &chunk));
            assert!(g.held() <= MAX_OSC_LEN, "holding {}", g.held());
            assert!(out.len() <= 16, "passed on {} bytes", out.len());
        }
    }

    #[test]
    fn text_after_an_over_cap_osc_renders_and_the_osc_has_no_effect() {
        let dims = GridSize::new(TermSize { cols: 20, rows: 3 }, 10).expect("size");
        let events = EventSink::new();
        let mut term = Term::new(Config::default(), &dims, events.clone());
        let mut parser: Processor = Processor::new();
        let mut g = OscGuard::new();

        let mut input = osc(MAX_OSC_LEN + 1);
        input.extend_from_slice(b"\x07after");
        for chunk in input.chunks(4096) {
            g.feed(chunk, |b| parser.advance(&mut term, b));
        }

        let row: String = (0..5)
            .map(|c| {
                let point = Point::new(Line(0), Column(c));
                crate::grid::cell_at(&term, point).expect("cell").c
            })
            .collect();
        assert_eq!(row, "after");
        let signals = events.drain();
        assert_eq!(signals.title, None, "the oversized title must not be set");
        assert_eq!(signals.bells, 0, "the BEL ended the OSC; it is no bell");
    }
}

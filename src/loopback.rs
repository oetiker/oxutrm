//! Both halves of oxutrm in one process, with no network in between.
//!
//! ```text
//!   shell on a PTY
//!     -> HostTerm (alacritty_terminal)
//!     -> ScreenState snapshot
//!     -> Sender<ScreenState> -> Frame -> BYTES -> Frame -> Receiver<ScreenState>
//!     -> Renderer
//!     -> the terminal the user is sitting in front of
//! ```
//!
//! and the reverse for input.
//!
//! # The round trip is real, and that is the entire point
//!
//! Every screen and every keystroke is encoded to a [`Frame`], **serialised to
//! bytes**, decoded again, and applied through a [`Receiver`]. Handing the
//! `ScreenState` straight to the renderer would be shorter and would prove
//! nothing: the value of this milestone is that the sync engine is exercised
//! against a live terminal rather than against test data. The indirection that
//! looks pointless here is the whole architecture.
//!
//! # It cannot fall behind
//!
//! There is no queue anywhere in this file. If the child produces output
//! faster than the tick, the newer state simply replaces the older one in the
//! sender's ring, and the next frame is current by construction. A runaway
//! `yes` therefore costs one frame per tick, not a backlog — and memory stays
//! flat, which [`Loopback::frames_sent`] and the tests make checkable.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context as _, Result};

use oxutrm_client::Renderer;
use oxutrm_proto::{Frame, ScreenState, TermSize, TerminalCaps};
use oxutrm_sync::{InputState, Receiver, Sender};
use oxutrm_term::HostTerm;

/// How often the loop offers a frame.
///
/// There is no QUIC here and therefore no round-trip time to adapt to, so the
/// interval is fixed. 8 ms is under one frame at 120 Hz: fast enough that
/// typing feels direct, slow enough that a screenful of output coalesces into
/// one frame instead of a hundred.
pub const TICK: Duration = Duration::from_millis(8);

/// What one turn of the loop did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Tick {
    /// A screen frame went round the loop.
    pub screen_frame: bool,
    /// An input frame went round the loop.
    pub input_frame: bool,
    /// Bytes the screen frame occupied on the wire. Zero when there was none.
    pub screen_bytes: usize,
    /// The client repainted.
    pub rendered: bool,
    /// The child is gone, with this exit code.
    pub exited: Option<i32>,
}

/// Both halves, joined.
pub struct Loopback {
    host: HostTerm,

    /// Host to client.
    screen_tx: Sender<ScreenState>,
    screen_rx: Receiver<ScreenState>,

    /// Client to host.
    input_tx: Sender<InputState>,
    input_rx: Receiver<InputState>,

    renderer: Renderer,
    size: TermSize,

    frames_sent: u64,
    bytes_sent: u64,
}

impl Loopback {
    pub fn new(
        shell: &str,
        args: &[String],
        env: &[(String, String)],
        size: TermSize,
        scrollback: usize,
        caps: TerminalCaps,
    ) -> Result<Loopback> {
        let host = HostTerm::spawn(shell, args, env, size, scrollback)
            .context("starting the shell on a pty")?;

        // Both ends start from the same blank screen, exactly as a fresh
        // attach would: the client has seen nothing, so the first frame is a
        // full state.
        let blank = ScreenState::blank(size.rows, size.cols)?;
        let empty = InputState {
            seq: 1,
            pending: Vec::new(),
            size,
        };

        Ok(Loopback {
            host,
            screen_tx: Sender::new(blank.clone()),
            screen_rx: Receiver::new(blank),
            input_tx: Sender::new(empty.clone()),
            input_rx: Receiver::new(empty),
            renderer: Renderer::new(size, caps),
            size,
            frames_sent: 0,
            bytes_sent: 0,
        })
    }

    /// One turn: feed `input` in, drain the PTY, move one frame each way,
    /// repaint.
    pub fn tick<W: Write>(&mut self, input: &[u8], out: &mut W) -> Result<Tick> {
        let mut tick = Tick::default();

        // ---- client -> host ------------------------------------------------
        if !input.is_empty() {
            let next = self.input_tx.current().append(input, self.size);
            self.input_tx.update(next);
        }
        self.input_tx.on_ack(self.input_rx.ack());
        if let Some(frame) = self.input_tx.make_frame(self.input_rx.ack())? {
            let round_tripped = Frame::decode(&frame.encode()?)?;
            if self.input_rx.on_frame(&round_tripped)? {
                tick.input_frame = true;

                // Only ever write straight after applying a frame. The
                // receiver's `pending` still holds these bytes until the next
                // diff trims them, so writing on a tick that applied nothing
                // would send the same keystrokes to the shell twice.
                let pending = self.input_rx.state().pending.clone();
                if !pending.is_empty() {
                    self.host
                        .write_input(&pending)
                        .context("writing to the pty")?;
                    // The host consumed all of it. The client drops the
                    // consumed prefix, and the next diff carries the count.
                    let trimmed = self.input_tx.current().consume(pending.len());
                    self.input_tx.update(trimmed);
                }
            }
        }

        // ---- the terminal --------------------------------------------------
        let changed = self.host.poll().context("draining the pty")?;
        if changed {
            // The sequence number here is a placeholder: `update` mints the
            // real one, which is what keeps numbering in exactly one place.
            let snapshot = self.host.snapshot(1);
            self.screen_tx.update(snapshot);
        }

        // ---- host -> client ------------------------------------------------
        self.screen_tx.on_ack(self.screen_rx.ack());
        if let Some(frame) = self.screen_tx.make_frame(self.screen_rx.ack())? {
            // Encode, then decode. Not an assertion of faith in postcard: it
            // is what makes this a transport round trip rather than a
            // pointer copy.
            let bytes = frame.encode().context("encoding a screen frame")?;
            let round_tripped = Frame::decode(&bytes).context("decoding a screen frame")?;

            tick.screen_bytes = bytes.len();
            self.frames_sent += 1;
            self.bytes_sent += bytes.len() as u64;

            if self.screen_rx.on_frame(&round_tripped)? {
                tick.screen_frame = true;
                self.renderer
                    .render(out, self.screen_rx.state())
                    .context("painting the terminal")?;
                out.flush().context("flushing the terminal")?;
                tick.rendered = true;
            }
        }

        tick.exited = self.host.child_exited();
        Ok(tick)
    }

    /// The window changed size.
    ///
    /// The emulator reflows, the renderer forgets what is painted, and the
    /// next diff carries the new geometry. Nothing here repaints directly:
    /// the resize travels the same path as everything else.
    pub fn resize(&mut self, size: TermSize) -> Result<()> {
        if size == self.size {
            return Ok(());
        }
        self.host.resize(size).context("resizing the pty")?;
        self.renderer.resize(size);
        self.size = size;
        Ok(())
    }
}

/// Windows into the loop, for the tests only.
///
/// The binary itself needs none of these: it feeds `tick` and paints what
/// comes back. They exist so the tests can compare the two ends and watch the
/// frame count, which is how "it cannot fall behind" is checked rather than
/// asserted.
#[cfg(test)]
impl Loopback {
    /// The screen the host believes in.
    pub fn host_screen(&self) -> ScreenState {
        self.host.snapshot(self.screen_tx.current().seq)
    }

    /// The screen the client has replicated, which is what was painted.
    pub fn client_screen(&self) -> &ScreenState {
        self.screen_rx.state()
    }

    /// How many frames have crossed the loop, and how many bytes they took.
    ///
    /// The `yes` test watches these: output arriving faster than the tick must
    /// not turn into more frames, because states are replaced rather than
    /// queued.
    pub fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }

    pub fn size(&self) -> TermSize {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use alacritty_terminal::Term;
    use alacritty_terminal::grid::Dimensions as _;
    use alacritty_terminal::index::{Column, Line, Point};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    fn size() -> TermSize {
        TermSize { cols: 40, rows: 10 }
    }

    fn caps() -> TerminalCaps {
        TerminalCaps {
            truecolor: true,
            colors: 16_777_216,
            bracketed_paste: true,
            mouse_sgr: true,
            osc52: true,
            term_name: "xterm-256color".to_owned(),
        }
    }

    fn loopback(script: &str) -> Loopback {
        Loopback::new(
            "/bin/sh",
            &["-c".to_owned(), script.to_owned()],
            &[],
            size(),
            200,
            caps(),
        )
        .expect("spawn")
    }

    /// Turn the loop until `f` is satisfied, or give up. There is a real
    /// process on the other end, so nothing may assume it has already run.
    fn drive(
        lb: &mut Loopback,
        out: &mut Vec<u8>,
        budget: Duration,
        f: impl Fn(&Loopback) -> bool,
    ) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            lb.tick(&[], out).expect("tick");
            if f(lb) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    fn text_of(s: &ScreenState) -> String {
        (0..s.rows)
            .map(|r| {
                s.row(r)
                    .iter()
                    .map(|c| {
                        if c.text.is_empty() {
                            " "
                        } else {
                            c.text.as_str()
                        }
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_owned()
    }

    /// Replay the bytes the renderer emitted through a real emulator, and read
    /// back what a terminal would be showing.
    ///
    /// This is the end-to-end assertion: not "the client's state matches the
    /// host's" — which the sync tests already prove — but "the ANSI we
    /// actually wrote to the terminal paints the screen the host meant".
    fn replay(ansi: &[u8], size: TermSize) -> Vec<String> {
        struct Dims(TermSize);
        impl alacritty_terminal::grid::Dimensions for Dims {
            fn total_lines(&self) -> usize {
                self.0.rows as usize
            }
            fn screen_lines(&self) -> usize {
                self.0.rows as usize
            }
            fn columns(&self) -> usize {
                self.0.cols as usize
            }
        }

        let dims = Dims(size);
        let mut term = Term::new(
            Config::default(),
            &dims,
            alacritty_terminal::event::VoidListener,
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, ansi);

        (0..term.screen_lines())
            .map(|r| {
                (0..term.columns())
                    .map(|c| term.grid()[Point::new(Line(r as i32), Column(c))].c)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn what_the_renderer_paints_is_what_the_host_meant() {
        let mut lb = loopback("printf 'alpha\\r\\nbeta\\r\\ngamma'; sleep 5");
        let mut out = Vec::new();
        assert!(
            drive(&mut lb, &mut out, Duration::from_secs(10), |lb| {
                text_of(lb.client_screen()).contains("gamma")
            }),
            "the text never arrived; client screen was {:?}",
            text_of(lb.client_screen())
        );

        // Replay the ANSI through an emulator and compare with the authority.
        let painted = replay(&out, lb.size());
        let host = lb.host_screen();
        for row in 0..host.rows {
            let want = host
                .row(row)
                .iter()
                .map(|c| {
                    if c.text.is_empty() {
                        " "
                    } else {
                        c.text.as_str()
                    }
                })
                .collect::<String>()
                .trim_end()
                .to_owned();
            assert_eq!(painted[row as usize], want, "row {row} differs");
        }
        assert_eq!(painted[0], "alpha");
        assert_eq!(painted[1], "beta");
        assert_eq!(painted[2], "gamma");
    }

    #[test]
    fn the_client_state_is_reached_only_through_encoded_frames() {
        // If anything ever short-circuits the round trip, the client would
        // still match - so the check is that frames were actually produced
        // and had a size on the wire.
        let mut lb = loopback("printf hello; sleep 5");
        let mut out = Vec::new();
        drive(&mut lb, &mut out, Duration::from_secs(10), |lb| {
            text_of(lb.client_screen()).contains("hello")
        });

        assert!(lb.frames_sent() >= 1, "no frame ever crossed the loop");
        assert!(
            lb.bytes_sent() > 0,
            "frames must be serialised, not passed by pointer"
        );
        assert_eq!(text_of(lb.client_screen()), text_of(&lb.host_screen()));
        assert_eq!(lb.client_screen().validate(), Ok(()));
    }

    #[test]
    fn a_flood_costs_one_frame_per_tick_rather_than_one_per_screen() {
        // The property that makes this design worth its indirection: states
        // are REPLACED, never queued.
        //
        // This used to assert `frames_sent() <= ticks`, which `tick` makes
        // true by construction - it increments at most one of each per turn -
        // so it could not go red for any change to any code. The claim it was
        // reaching for is about VOLUME: each of these lines is a separate
        // write that moves the screen, and thousands of them must collapse
        // into a handful of frames. A loop that carried screens one at a time
        // would need a frame per line, so counting lines against frames is
        // what makes "replaced, never queued" checkable rather than merely
        // asserted. Shrink `oxutrm_term`'s READ_BUDGET to a few bytes and this
        // fails, which the old form did not.
        const LINES: u64 = 4000;
        let mut lb = loopback(&format!(
            "i=0; while [ $i -lt {LINES} ]; do echo flood$i; i=$((i+1)); done; \
             printf 'FLOOD-OVER\\r\\n'; sleep 30"
        ));
        let mut out = Vec::new();

        assert!(
            drive(&mut lb, &mut out, Duration::from_secs(60), |lb| {
                text_of(lb.client_screen()).contains("FLOOD-OVER")
            }),
            "the flood never finished; the loop fell behind. client screen was {:?}",
            text_of(lb.client_screen())
        );

        let frames = lb.frames_sent();
        assert!(frames >= 1, "no frame ever crossed the loop");
        assert!(
            frames * 8 < LINES,
            "{frames} frames to carry {LINES} lines of output: the loop is \
             delivering screens one at a time rather than replacing them"
        );
        assert!(
            text_of(lb.client_screen()).contains("FLOOD-OVER"),
            "the screen should be current, not stuck behind a backlog"
        );
        assert_eq!(lb.client_screen().validate(), Ok(()));
        assert_eq!(lb.client_screen().rows, size().rows);
    }

    #[test]
    fn a_runaway_writer_leaves_the_screen_current_rather_than_stale() {
        // "Cannot fall behind" is not just about frame count: after the flood
        // stops, the very next frame must carry the CURRENT screen, not the
        // next one in a queue.
        let mut lb = loopback(
            "i=0; while [ $i -lt 4000 ]; do echo flood$i; i=$((i+1)); done; printf DONE-MARKER; sleep 5",
        );
        let mut out = Vec::new();
        assert!(
            drive(&mut lb, &mut out, Duration::from_secs(20), |lb| {
                text_of(lb.client_screen()).contains("DONE-MARKER")
            }),
            "the final marker never appeared; the loop fell behind"
        );
    }

    #[test]
    fn keystrokes_reach_the_shell_and_the_answer_comes_back() {
        let mut lb = loopback("read line; printf 'you said %s' \"$line\"");
        let mut out = Vec::new();

        lb.tick(b"ping\n", &mut out).expect("tick");
        assert!(
            drive(&mut lb, &mut out, Duration::from_secs(10), |lb| {
                text_of(lb.client_screen()).contains("you said ping")
            }),
            "got {:?}",
            text_of(lb.client_screen())
        );
    }

    #[test]
    fn input_is_never_written_to_the_shell_twice() {
        // The receiver's `pending` still holds the bytes until the next diff
        // trims them, so a loop that wrote on every tick would send the same
        // keystrokes again and again. `cat` echoes what it is given, so a
        // duplicate would show up as a second copy.
        let mut lb = loopback("read a; read b; printf 'first=%s second=%s' \"$a\" \"$b\"");
        let mut out = Vec::new();

        lb.tick(b"one\n", &mut out).expect("tick");
        // Several ticks with no new input at all.
        for _ in 0..30 {
            lb.tick(&[], &mut out).expect("tick");
            std::thread::sleep(Duration::from_millis(2));
        }
        lb.tick(b"two\n", &mut out).expect("tick");

        assert!(
            drive(&mut lb, &mut out, Duration::from_secs(10), |lb| {
                text_of(lb.client_screen()).contains("second=two")
            }),
            "got {:?}",
            text_of(lb.client_screen())
        );
        let text = text_of(lb.client_screen());
        assert!(
            text.contains("first=one second=two"),
            "input was duplicated or lost: {text:?}"
        );
    }

    #[test]
    fn nothing_is_sent_when_nothing_happened() {
        // Staying quiet costs no bandwidth: with the peer up to date,
        // make_frame returns None and the loop does nothing at all.
        let mut lb = loopback("sleep 5");
        let mut out = Vec::new();
        drive(&mut lb, &mut out, Duration::from_millis(300), |_| false);

        let before = lb.frames_sent();
        for _ in 0..20 {
            let t = lb.tick(&[], &mut out).expect("tick");
            assert!(!t.screen_frame, "a quiet terminal produced a frame");
        }
        assert_eq!(lb.frames_sent(), before);
    }

    #[test]
    fn a_resize_travels_through_the_diff_like_everything_else() {
        let mut lb = loopback("sleep 5");
        let mut out = Vec::new();
        drive(&mut lb, &mut out, Duration::from_millis(200), |_| false);

        let bigger = TermSize {
            cols: 100,
            rows: 30,
        };
        lb.resize(bigger).expect("resize");
        assert!(
            drive(&mut lb, &mut out, Duration::from_secs(10), |lb| {
                lb.client_screen().cols == 100 && lb.client_screen().rows == 30
            }),
            "the resize never reached the client: {:?}",
            (lb.client_screen().cols, lb.client_screen().rows)
        );
        assert_eq!(lb.client_screen().validate(), Ok(()));
        assert_eq!(lb.size(), bigger);

        // And back down again.
        lb.resize(size()).expect("resize");
        assert!(drive(&mut lb, &mut out, Duration::from_secs(10), |lb| {
            lb.client_screen().cols == 40
        }));
    }

    #[test]
    fn resizing_to_the_same_size_is_a_no_op() {
        let mut lb = loopback("sleep 5");
        lb.resize(size()).expect("resize");
        assert_eq!(lb.size(), size());
    }

    #[test]
    fn the_loop_notices_when_the_child_exits() {
        let mut lb = loopback("exit 7");
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let t = lb.tick(&[], &mut out).expect("tick");
            if let Some(code) = t.exited {
                assert_eq!(code, 7);
                break;
            }
            assert!(Instant::now() < deadline, "the child never exited");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_client_screen_is_valid_after_every_single_tick() {
        // The invariant everything downstream assumes. If a frame could ever
        // leave the client in an invalid state, the renderer would be
        // painting from something no one can describe.
        let mut lb = loopback("printf 'a\\r\\nb\\r\\nc'; printf '\\033[2J'; printf 'z'; sleep 5");
        let mut out = Vec::new();
        for _ in 0..200 {
            lb.tick(&[], &mut out).expect("tick");
            assert_eq!(lb.client_screen().validate(), Ok(()));
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

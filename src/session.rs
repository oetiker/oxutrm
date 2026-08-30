//! The two loops that make a remote terminal.
//!
//! ```text
//!   host:    PTY -> HostTerm -> Sender<ScreenState> -> QUIC
//!            QUIC -> Receiver<InputState> -> HostTerm::write_input -> PTY
//!
//!   client:  keys -> Sender<InputState> -> QUIC
//!            QUIC -> Receiver<ScreenState> -> Renderer -> the real terminal
//! ```
//!
//! They are the same shape, which is the point: one replicated value in each
//! direction, diffed against what the peer last acknowledged, paced by the
//! link's own round-trip estimate.
//!
//! # Three rules, all of them about not falling behind
//!
//! **States coalesce; frames never queue.** If output outruns the link, the
//! sender's ring simply holds newer states and the next frame is current by
//! construction. A runaway `yes` costs one frame per pacing interval, not a
//! backlog.
//!
//! **A send failure is not a disconnection.** Neither loop propagates a send
//! error. A dropped frame costs one interval, because the next diff is
//! computed against the same acknowledged base and carries everything the lost
//! one would have. This is the same discipline as the receive side, where a
//! rejected frame leaves the state and the ack exactly as they were.
//!
//! **Pacing comes from `quinn`, not from us.** `clamp(rtt / 2, 8ms, 100ms)`,
//! read from the live connection, with an immediate send when the link has
//! been idle.
//!
//! # Capabilities travel one way
//!
//! The client calls `detect_caps` and down-converts colours it cannot show at
//! render time. The host calls `negotiate_term()`, which takes **no
//! arguments**: `TERM` is derived solely from what the emulator supports.
//! A shell that has been running for a week cannot have its `TERM` changed
//! under it because a different client reattached, and down-converting on the
//! host would permanently degrade the state for every future client.

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use oxutrm_client::{Notice, Renderer, layout_notice, status_line, terminal_size_of};
use oxutrm_proto::{Frame, PathDescription, ScreenState, TermSize, TerminalCaps};
use oxutrm_sync::{InputState, Receiver, Sender, SyncState as _};
use oxutrm_term::HostTerm;

use crate::link::{Link, SendOutcome};
use crate::linkstate::{Command, LinkState, Phase};

/// How long a loop waits for something to happen before looking again.
///
/// Short enough that a keystroke is never sitting in a buffer, long enough
/// that an idle session costs nothing.
const IDLE_POLL: Duration = Duration::from_millis(4);

/// How long [`ClientSession::drain`] will keep taking frames off a closed
/// link before handing the user their prompt back.
///
/// The work it bounds is local — decode, apply, paint — so this is never
/// reached in practice. It exists so that a reader task which somehow outlives
/// its connection cannot hold a person's terminal hostage.
const FINAL_DRAIN: Duration = Duration::from_secs(2);

/// How often the numbers inside a notice already on the screen are allowed to
/// change.
///
/// The counters in the `Silent` box move on nearly every lap: the client keeps
/// retransmitting during an outage, so `sent_packets` climbs at the pacing
/// rate, which is as often as 125 times a second. That is both expensive --
/// each change is two `Paragraph` renders, a clone of the whole cell grid, a
/// diff and a flush -- and useless, because a number churning that fast cannot
/// be read, in a box whose entire job is to be read.
///
/// It bounds the REFRESH only. A change of phase is what the box exists to
/// announce and is never held back; see [`ClientSession::notice_at`].
const NOTICE_REFRESH: Duration = Duration::from_secs(1);

/// How long the host keeps building frames for a client it has not heard from.
///
/// **This is the guarantee quinn used to provide and no longer does.** Until
/// phase 2 the host asked `close_reason()`, which answered once the transport's
/// 30 s idle timeout had fired. `max_idle_timeout` is `None` now, so
/// `close_reason()` stays `None` for ever on a silent peer and the question has
/// to be answered from a clock of our own.
///
/// Thirty seconds, so the behaviour is unchanged by construction: it is
/// exactly what quinn enforced before -- with one difference. Quinn's idle
/// timer was reset by *any* transport activity, including the 10 s
/// keep-alive, so a client whose quinn stack kept answering keep-alives while
/// its application loop was wedged used to stay attached for ever. `last_heard`
/// moves only on an application frame, so that peer now detaches at 30 s
/// instead -- a stricter and more honest reading of "still there". Six times
/// `HEARTBEAT_IDLE`, so an attached client that is merely quiet is nowhere
/// near it -- it heartbeats at 0.2 Hz and every heartbeat is a frame.
///
/// Detaching closes nothing. It stops snapshotting and stops offering frames;
/// the pty is still drained and the emulator still fed, because the screen being
/// current on reattach is the whole reason a detached session keeps emulating.
/// A peer that comes back is heard on its first frame and `screen_stale` forces
/// the snapshot.
pub const DETACH_AFTER: Duration = Duration::from_secs(30);

/// What one turn did. Returned so tests can watch the loop rather than infer
/// it from the screen.
#[derive(Clone, Debug, Default)]
pub struct Turn {
    pub sent: Option<SendOutcome>,
    pub applied: usize,
    /// Frames that arrived but could not be applied: the two ends disagree
    /// about the base.
    ///
    /// This should be **zero**, and a steady stream of it is a defect however
    /// healthy the screen looks. That warning was originally written as "a
    /// deadlock rather than a slow link", and when the flood test finally made
    /// it fire, it was neither: the session converged and every test passed,
    /// while half of every frame the host sent was thrown away and the client
    /// painted a screen it was holding a newer copy of. Convergence was doing
    /// the job of hiding it — the sender re-diffs from the same ack, so the
    /// content always arrives eventually, one round trip later than it should.
    /// See contract rules R4 and R5.
    ///
    /// So: not necessarily a deadlock. Necessarily wasted work, and the waste
    /// is invisible to any assertion about the screen.
    pub rejected: usize,
    pub exited: Option<i32>,
    /// No peer was listening this turn, so the send-side work was skipped.
    ///
    /// Reported rather than inferred: "no frame was sent" is also what a
    /// paced turn looks like, and the two are not the same thing.
    pub detached: bool,
}

/// The remote half: owns the PTY and the authoritative screen.
pub struct HostSession {
    term: HostTerm,
    screen_tx: oxutrm_sync::Sender<ScreenState>,
    input_rx: Receiver<InputState>,
    link: Link,
    size: TermSize,
    last_send: Option<Instant>,
    /// How much of the receiver's pending input has already gone to the PTY.
    /// See [`HostSession::drain_input`].
    written: usize,
    /// The emulator moved while nobody was attached, so the snapshot the
    /// sender holds is older than the screen. Forces one snapshot on the
    /// turn a peer comes back, whether or not the pty moved on that turn.
    screen_stale: bool,
    /// The last time anything arrived from the client. The host's own liveness
    /// clock, because `close_reason()` stopped being one when the transport's
    /// idle timeout went. See [`DETACH_AFTER`].
    last_heard: Instant,
}

impl HostSession {
    /// Start a shell and serve it over `link`.
    ///
    /// `TERM` and `COLORTERM` come from [`oxutrm_term::negotiate_term`], which
    /// takes no arguments on purpose.
    pub fn spawn(
        shell: &str,
        size: TermSize,
        scrollback: usize,
        link: Link,
    ) -> Result<HostSession> {
        let (term_name, colorterm) = oxutrm_term::negotiate_term();
        let mut env = vec![("TERM".to_owned(), term_name)];
        if let Some(ct) = colorterm {
            env.push(("COLORTERM".to_owned(), ct));
        }

        let term = HostTerm::spawn(shell, &[], &env, size, scrollback)
            .context("starting the shell on a pty")?;
        let blank = ScreenState::blank(size.rows, size.cols)?;
        let empty = InputState {
            seq: 1,
            pending: Vec::new(),
            size,
        };

        Ok(HostSession {
            term,
            screen_tx: oxutrm_sync::Sender::new(blank),
            input_rx: Receiver::new(empty),
            link,
            size,
            last_send: None,
            written: 0,
            screen_stale: false,
            // An attach has just completed and R5 obliges the client to send
            // immediately, so "now" is true rather than optimistic.
            last_heard: Instant::now(),
        })
    }

    /// One turn: apply whatever arrived, drain the PTY, offer a frame.
    pub fn turn(&mut self) -> Result<Turn> {
        self.turn_at(Instant::now(), None)
    }

    /// [`HostSession::turn`], plus a frame the caller has already taken off
    /// the source.
    ///
    /// `run`'s select has to *receive* a frame to know one arrived, so it
    /// arrives holding one; `try_recv` below would never see it and the
    /// keystrokes in it would be silently dropped.
    pub fn turn_with(&mut self, first: Option<Frame>) -> Result<Turn> {
        self.turn_at(Instant::now(), first)
    }

    /// [`HostSession::turn_with`], with the clock injected.
    ///
    /// The clock is a parameter for the same reason it is one throughout
    /// `LinkState` and `ClientSession::note_heard`: [`DETACH_AFTER`] is thirty
    /// seconds, and a threshold that can only be tested by sleeping thirty
    /// seconds is a threshold nobody tests.
    pub fn turn_at(&mut self, now: Instant, mut first: Option<Frame>) -> Result<Turn> {
        let mut turn = Turn::default();

        // ---- inbound: the client's keystrokes ------------------------------
        // (the size the client wants rides on the same diff, and is applied
        // below once the frames have been taken in)
        while let Some(frame) = first.take().or_else(|| self.link.source.try_recv()) {
            // Any frame at all is evidence of a peer, including one `on_frame`
            // rejects: a stale sequence number says the client is behind, not
            // that it is gone.
            self.last_heard = now;
            // A rejected frame is not a disconnection: the state and the ack
            // are both untouched, and the peer's next diff will apply.
            match self.input_rx.on_frame(&frame) {
                Ok(true) => {
                    turn.applied += 1;
                    self.drain_input()?;
                }
                Ok(false) => {}
                // Not a disconnection, but not nothing either: a BaseMismatch
                // is the peer diffing from a base we do not hold, and silence
                // here once hid a deadlock for a whole day.
                Err(e) => {
                    turn.rejected += 1;
                    eprintln!("oxutrm: host dropped an unapplicable input frame: {e}");
                }
            }
        }

        // The client's requested size arrives on the input diff. This has to
        // live in `turn` rather than in `run`, or a caller driving the loop
        // itself - which is every test, and will be M3's reattach path -
        // silently never resizes.
        let wanted = self.input_rx.state().size;
        if wanted != self.size && wanted.cols > 0 && wanted.rows > 0 {
            self.resize(wanted)?;
        }

        // ---- is anyone listening? -------------------------------------------
        // A detached session must keep DRAINING the pty below - a child whose
        // output nobody reads fills the buffer and blocks forever - and must
        // keep feeding the emulator, because the whole point of a detachable
        // session is that the screen is current when you come back. But
        // everything after that exists only to build a frame for a peer, and
        // there is no peer.
        //
        // Measured: a detached session whose child was writing five lines a
        // second burned 17-20% of a core doing exactly that, for a screen
        // nobody would ever see. Quiet ones cost 1.2%, which is why this hid.
        //
        // Two questions, and since phase 2 they have different answers.
        // `close_reason` still catches a peer that closed properly or a
        // transport error -- both are immediate and certain. What it no longer
        // catches is silence: `max_idle_timeout` is `None`, so quinn will hold
        // a connection to a peer that vanished for ever, and this used to read
        // "turns off only once quinn has given the connection up".
        //
        // So the recency window is what answers it now. Generous on purpose:
        // during a blip the connection is open and we WANT the work to
        // continue, so the session resumes instantly when the peer comes back.
        // `DETACH_AFTER` is six times the client's heartbeat interval.
        let closed = self.link.sink.connection().close_reason().is_some();
        let quiet_too_long = now.duration_since(self.last_heard) >= DETACH_AFTER;
        let attached = !closed && !quiet_too_long;
        turn.detached = !attached;

        // ---- the terminal --------------------------------------------------
        let moved = self.term.poll().context("draining the pty")?;
        if attached {
            if moved || self.screen_stale {
                // The sequence number is a placeholder; `update` mints the real
                // one, keeping numbering in exactly one place.
                let snapshot = self.term.snapshot(1);
                self.screen_tx.update(snapshot);
                self.screen_stale = false;
            }
        } else if moved {
            self.screen_stale = true;
        }

        // ---- outbound: the screen ------------------------------------------
        if attached {
            turn.sent = self.offer_frame();
        }
        turn.exited = self.term.child_exited();
        Ok(turn)
    }

    /// Write newly acknowledged input to the PTY, exactly once.
    ///
    /// The receiver's `pending` holds bytes until the client's next diff trims
    /// them, so a loop that wrote all of `pending` on every turn would send
    /// the same keystrokes to the shell repeatedly. `written` tracks how much
    /// of the current `pending` has already gone out, and the client's
    /// `consumed` count is what shrinks it back.
    fn drain_input(&mut self) -> Result<()> {
        let pending = self.input_rx.state().pending.clone();
        // A diff that consumed from the front makes `pending` shorter; the
        // offset has to shrink with it or we would skip real input.
        if self.written > pending.len() {
            self.written = pending.len();
        }
        if self.written < pending.len() {
            let fresh = pending[self.written..].to_vec();
            self.term
                .write_input(&fresh)
                .context("writing to the pty")?;
            self.written = pending.len();
        }
        Ok(())
    }

    /// The frame the current state owes the peer, if any, and the bookkeeping
    /// that says it has been offered.
    ///
    /// Split out from [`HostSession::offer_frame`] so the two ways of putting
    /// it on the wire — paced and unreliable, or final and reliable — differ
    /// only in the sending, never in what is sent.
    fn next_frame(&mut self) -> Option<Frame> {
        self.screen_tx.on_ack(self.input_rx.peer_ack());
        match self.screen_tx.make_frame(self.input_rx.ack()) {
            Ok(Some(f)) => {
                self.last_send = Some(Instant::now());
                Some(f)
            }
            // Nothing to send, or a diff that could not be built. Neither ends
            // the session.
            Ok(None) | Err(_) => None,
        }
    }

    fn offer_frame(&mut self) -> Option<SendOutcome> {
        if !self.due() {
            return None;
        }
        let frame = self.next_frame()?;
        Some(self.link.sink.send(&frame))
    }

    /// [`HostSession::offer_frame`], on a stream that is finished and
    /// acknowledged before this returns.
    ///
    /// For the last frame of a session only. See [`crate::link::FrameSink::send_final`].
    async fn offer_frame_reliably(&mut self) -> Option<SendOutcome> {
        if !self.due() {
            return None;
        }
        let frame = self.next_frame()?;
        Some(self.link.sink.send_final(&frame).await)
    }

    fn due(&self) -> bool {
        match self.last_send {
            // Idle: go now rather than waiting out an interval.
            None => true,
            Some(t) => t.elapsed() >= self.link.sink.pacing_interval(),
        }
    }

    /// Resize the PTY and the emulator. The next diff carries it.
    pub fn resize(&mut self, size: TermSize) -> Result<()> {
        if size == self.size {
            return Ok(());
        }
        self.term.resize(size).context("resizing the pty")?;
        self.size = size;
        Ok(())
    }

    /// Run until the child exits, waiting on descriptors rather than polling.
    ///
    /// Measured: the `IDLE_POLL` version this replaced cost 24-27 ms of CPU
    /// across 2 s for a DETACHED session with nothing to do — the 1.2% of a
    /// core the handoff recorded — against 0-1 ms for this one.
    ///
    /// The descriptors are duplicated out of the terminal before the loop so
    /// the arms borrow locals rather than `self`, which is what lets the body
    /// call `&mut self` methods afterwards (C1). A `dup` shares the file
    /// description, harmless here in a way it is NOT for the client's
    /// keyboard: this description is ours and we set its `O_NONBLOCK`
    /// ourselves in `Pty::spawn`.
    pub async fn run(&mut self) -> Result<i32> {
        let output = self.term.output_fd().try_clone_to_owned()?;
        let output = tokio::io::unix::AsyncFd::with_interest(output, tokio::io::Interest::READABLE)
            .context("waiting on the pty")?;
        let exit = match self.term.exit_wake().as_fd() {
            Some(fd) => Some(
                tokio::io::unix::AsyncFd::with_interest(
                    fd.try_clone_to_owned()?,
                    tokio::io::Interest::READABLE,
                )
                .context("waiting on the child")?,
            ),
            // Already gone when it was watched. The first turn below reports
            // the exit before anything waits, so there is nothing to miss.
            None => None,
        };

        // A frame taken off the source by the select, owed to the next turn.
        let mut pending: Option<Frame> = None;
        // The exit wake fired but `child_exited` disagreed. It is edge
        // triggered and will not fire twice, so re-check on a timer instead of
        // trusting the hint — the same rule that keeps PTY EOF out of this.
        let mut recheck_child = false;

        loop {
            let turn = match pending.take() {
                None => self.turn()?,
                Some(frame) => self.turn_with(Some(frame))?,
            };
            if let Some(code) = turn.exited {
                self.finish(code).await;
                return Ok(code);
            }

            // Bytes are still in the PTY buffer, and readiness for them has
            // already been delivered. Go round again rather than sleeping on
            // an edge that will not come.
            //
            // Honest about its status: NO test currently fails without this.
            // Removing it leaves the suite green, because a child that has
            // more to write supplies another edge when it writes, and the exit
            // wake supplies the last one. What it removes is a staleness
            // window - a detached session whose child bursts and then falls
            // quiet would hold an emulator behind the child until something
            // else happened, and the screen being current on reattach is the
            // whole reason a detached session keeps emulating at all. It is
            // kept as the cheap half of a guarantee whose expensive half
            // (`READ_BUDGET` versus the kernel's PTY buffer) is not ours.
            if self.term.more_output_waiting() {
                continue;
            }

            // Armed only when a frame is owed but paced out, so a session with
            // nothing to say holds no timer at all. `due()` goes true on the
            // lap after it fires, which is what stops it re-arming for ever.
            let mut deadline = if self.due() {
                None
            } else {
                Some(tokio::time::Instant::now() + self.link.sink.pacing_interval())
            };
            if std::mem::take(&mut recheck_child) {
                let at = tokio::time::Instant::now() + IDLE_POLL;
                deadline = Some(deadline.map_or(at, |d| d.min(at)));
            }

            // Nothing here touches `self`; every borrow starts after the
            // select expression has ended and dropped these futures (C1).
            //
            // There is deliberately NO `conn.closed()` arm. A closed
            // connection is permanently ready, so an arm watching one would
            // spin — and the host must not end the session anyway, since
            // outliving a vanished client is the entire point. `turn` re-reads
            // `close_reason` whenever something else wakes it, which is
            // exactly when the answer can matter.
            let wake: HostWake = tokio::select! {
                r = output.readable() => match r {
                    // Cleared HERE, having just established above that the PTY
                    // came up empty. Read-then-clear is the ordering `try_io`
                    // uses, and clearing while bytes remain would stall the
                    // screen until the child happened to write again.
                    Ok(mut g) => { g.clear_ready(); HostWake::Pty }
                    Err(e) => return Err(e).context("waiting on the pty"),
                },
                r = async { exit.as_ref().expect("armed").readable().await }, if exit.is_some() => match r {
                    Ok(mut g) => { g.clear_ready(); HostWake::Exit }
                    Err(e) => return Err(e).context("waiting on the child"),
                },
                Some(frame) = self.link.source.recv() => HostWake::Frame(frame),
                () = async { tokio::time::sleep_until(deadline.expect("armed")).await },
                    if deadline.is_some() => HostWake::Due,
            };

            match wake {
                HostWake::Frame(frame) => pending = Some(frame),
                HostWake::Exit => recheck_child = true,
                HostWake::Pty | HostWake::Due => {}
            }
        }
    }

    /// The last screen, and only then the close. `ls; exit` lives or dies here.
    ///
    /// Three separate things were losing it, and all three had to go:
    ///
    /// **The shell's last write and its exit are two events.** `turn` polls
    /// the pty and *then* reaps the child, so a shell that printed and exited
    /// in the same breath leaves its output in the pty buffer, unread, on the
    /// very turn that reports the exit. One more poll collects it.
    ///
    /// **Pacing has nothing left to defer to.** `offer_frame` is gated by
    /// `due()`, which at an 8 ms interval against a 4 ms poll is false on
    /// roughly half the turns. Normally that costs one interval; here it costs
    /// the screen, because there is no next interval. Clearing `last_send` is
    /// what makes the final offer unconditional.
    ///
    /// **`close` discards whatever is still in flight.** A datagram, or a
    /// stream whose writer task has not yet reached `open_uni`. So the final
    /// frame goes on a stream that is finished and *acknowledged* before the
    /// close is sent.
    ///
    /// Infallible on purpose: nothing here is worth reporting instead of the
    /// status of a shell that has already exited.
    pub async fn finish(&mut self, code: i32) {
        if self.term.poll().unwrap_or(false) {
            let snapshot = self.term.snapshot(1);
            self.screen_tx.update(snapshot);
        }
        self.last_send = None;
        self.offer_frame_reliably().await;
        self.close(code);
    }

    /// Tell the client the shell is gone, and with what status.
    ///
    /// The exit code has no field in the protocol and needs none. QUIC's own
    /// close carries an application error code, so the status travels on the
    /// mechanism that *is* the end of the session rather than in a frame that
    /// would have to arrive first — and a frame is exactly what cannot be
    /// relied on here, since the close discards whatever is still in flight.
    ///
    /// A code outside `u32` cannot come from a shell; `child_exited` invents
    /// `-1` for a child it can no longer wait on, and that becomes 255, the
    /// same thing every shell reports for "something went wrong out here".
    ///
    /// The reason phrase is [`SHELL_EXITED`] and is load-bearing, not
    /// decoration. See its own note.
    pub fn close(&self, code: i32) {
        let code = u32::try_from(code).unwrap_or(255);
        self.link
            .sink
            .connection()
            .close(quinn::VarInt::from_u32(code), SHELL_EXITED);
    }

    /// The authoritative screen, for tests. Nothing in the session loop reads
    /// it: the loop ships diffs and never inspects what it shipped.
    #[allow(dead_code)]
    pub fn screen(&self) -> &ScreenState {
        self.screen_tx.current()
    }
}

/// What woke [`ClientSession::run_on`].
///
/// Waking and acting are separate steps, and that is structural rather than
/// stylistic. `tokio::select!` keeps every arm's future alive while the
/// winning arm's body runs, so an arm body that reached for `self` would hold
/// a second borrow of a session another arm's future has already borrowed, and
/// the loop would not compile. Every arm therefore produces one of these and
/// touches nothing else; the whole session is borrowed afterwards, once.
enum Wake {
    /// Keystrokes — or zero of them, which is end of file on the keyboard.
    Keys(usize),
    Frame(Frame),
    Winch,
    /// The pacing deadline came round.
    Due,
    Closed(quinn::ConnectionError),
    /// A readiness that turned out to be nothing. Costs one lap.
    Nothing,
}

/// The host's half of the same idea. Separate from [`Wake`] because the two
/// loops wake for entirely different reasons and a shared enum would give each
/// of them variants it can never produce.
enum HostWake {
    /// The child wrote something.
    Pty,
    /// The child exited — a hint; `child_exited` is the authority.
    Exit,
    Frame(Frame),
    /// A frame was owed but paced out, and the pace has come round.
    Due,
}

/// Readiness on the keyboard, or never again once it has reached end of file.
///
/// `None` does not mean "not ready yet"; it means the arm is retired. Written
/// as a function rather than a `select!` precondition so the borrow of `keys`
/// is exactly the returned guard, and the loop can drop the keyboard in the
/// same breath as reading its last byte.
async fn keys_readable<K: AsRawFd>(
    keys: &mut Option<tokio::io::unix::AsyncFd<K>>,
) -> std::io::Result<tokio::io::unix::AsyncFdReadyMutGuard<'_, K>> {
    match keys {
        Some(k) => k.readable_mut().await,
        None => std::future::pending().await,
    }
}

/// The one application close on a session connection that means "the shell
/// exited, and the error code beside me is its status".
///
/// `ApplicationClosed` on its own says nothing: it is what *every* deliberate
/// close looks like, from anywhere, and the error code beside it is whatever
/// that closer chose. A reattach superseding an old attach, `accept_one`
/// tearing down a second inbound connection, a clean detach — all three are
/// application closes, and all three would have made the old client print
/// `exit 0` at somebody whose shell is still running on the far end. That is
/// not a cosmetic wrong answer: it is a user being told their work finished.
///
/// QUIC already carries a reason phrase, so distinguishing them costs nothing
/// on the wire. This is the only phrase [`exit_code`] accepts, and
/// [`HostSession::close`] is the only place that sends it.
pub const SHELL_EXITED: &[u8] = b"the shell exited";

/// Why the session ended, as an exit status.
///
/// The shell's exit code has no field in the protocol and needs none: the host
/// closes the QUIC connection with it as the application error code, so it
/// rides the mechanism that ends the session. Anything else closed the link —
/// a timeout, a reset, a host that was killed, or an application close that
/// was not [`SHELL_EXITED`] — and that is an error rather than a status,
/// because no shell said it.
fn exit_code(reason: &quinn::ConnectionError) -> Result<i32> {
    match reason {
        quinn::ConnectionError::ApplicationClosed(closed)
            if closed.reason.as_ref() == SHELL_EXITED =>
        {
            Ok(i32::try_from(closed.error_code.into_inner()).unwrap_or(255))
        }
        // An application close from somewhere that is not a shell finishing.
        // Saying so beats inventing a status the user would believe.
        quinn::ConnectionError::ApplicationClosed(closed) => Err(anyhow::anyhow!(
            "the host closed the session without the shell exiting: {}",
            String::from_utf8_lossy(&closed.reason)
        )),
        quinn::ConnectionError::TimedOut => Err(anyhow::anyhow!(
            "the link to the host timed out. Silence alone no longer ends a \
             session, so this is the transport giving up rather than the host \
             going quiet."
        )),
        other => Err(anyhow::anyhow!(
            "the link to the host ended without the shell exiting: {other}"
        )),
    }
}

/// The local half: paints the screen and sends keystrokes.
pub struct ClientSession {
    screen_rx: Receiver<ScreenState>,
    input_tx: Sender<InputState>,
    renderer: Renderer,
    link: Link,
    size: TermSize,
    last_send: Option<Instant>,
    /// The path last announced, so a change can be spotted and silence can be
    /// the default.
    announced: Option<PathDescription>,
    /// Whether the host is still answering, and what the user is told.
    link_state: LinkState,
    /// Frames that arrived and could not be applied, for the notice.
    ///
    /// This used to be an `eprintln!`, which was a bug rather than a
    /// diagnostic: the client's stderr IS the terminal it is painting, so the
    /// message desynchronised the renderer's model and nothing repainted it on
    /// a quiet session.
    rejected_total: u64,
    /// What is currently drawn as layer 1, so an unchanged notice does not
    /// rebuild an overlay every tick.
    shown: Option<Notice>,
    /// The phase [`ClientSession::shown`] was built for, and when.
    ///
    /// Both halves are needed: the instant paces the refresh, and the phase is
    /// what tells a refresh apart from a transition, which is never paced.
    built: Option<(Phase, Instant)>,
    /// The source address the link was working from, so a moved route can be
    /// spotted. Seeded in [`ClientSession::new`] from the path the connection
    /// came up over. See [`crate::roam`].
    route: crate::roam::RouteWatch,
    /// When the route was last probed, so `Silent` does not probe on every
    /// lap of a loop that wakes up to 125 times a second.
    probed_at: Option<Instant>,
}

impl ClientSession {
    pub fn new(size: TermSize, caps: TerminalCaps, link: Link) -> Result<ClientSession> {
        let blank = ScreenState::blank(size.rows, size.cols)?;
        let empty = InputState {
            seq: 1,
            pending: Vec::new(),
            size,
        };

        // The address this machine reaches the host from, read once, here.
        //
        // Here and not on the first probe of an outage, because the outage is
        // too late: `SILENT_AFTER` is two seconds, and walking out of Wi-Fi
        // range moves the route BEFORE the silence is noticed. A baseline
        // first taken inside `Silent` would read the address the machine had
        // already moved to, agree with it for ever, and never rebind -- which
        // is the whole case this exists for. At this moment the connection has
        // just been established over this very path, so the source address for
        // the peer is definitionally the address the link works from.
        //
        // One `bind`+`connect` pair per session, and `connect` sends no
        // packet. That is not a pace and does not soften the `Silent` rule:
        // the rule governs the REBIND, which `follow_route` still does only
        // while `Silent`. Nothing probes on a `Live` lap.
        //
        // A probe that cannot answer is not a reason to refuse a session that
        // has already connected. `None` is the old behaviour and stays safe:
        // `RouteWatch::moved` is false without a baseline, so the first probe
        // of an outage takes one instead.
        let seed = crate::roam::route_source(link.sink.connection().remote_address()).ok();

        Ok(ClientSession {
            screen_rx: Receiver::new(blank),
            input_tx: Sender::new(empty),
            renderer: Renderer::new(size, caps),
            link,
            size,
            last_send: None,
            announced: None,
            link_state: LinkState::new(Instant::now()),
            rejected_total: 0,
            shown: None,
            built: None,
            route: crate::roam::RouteWatch::new(seed),
            probed_at: None,
        })
    }

    /// Tell the user what connection they got — once, and then be quiet.
    ///
    /// Spec §10.3: oxutrm never does anything clever silently. On connect this
    /// prints one line. Called again with the same path it prints **nothing**,
    /// which is what makes the silence a property rather than an accident.
    /// Called again with a different path it announces the migration briefly,
    /// because walking from Wi-Fi to mobile should be explained rather than
    /// mysterious.
    ///
    /// Rung 4 reads as a warning, and that is not decoration: a session inside
    /// the SSH connection cannot daemonize and cannot be reattached, so
    /// degrading to it silently would remove both properties the project
    /// exists to provide while looking like success.
    pub fn announce<W: Write>(&mut self, path: &PathDescription, out: &mut W) -> Result<bool> {
        let same = self
            .announced
            .as_ref()
            .is_some_and(|old| old.rung == path.rung && old.remote == path.remote);
        if same {
            return Ok(false);
        }

        let line = match &self.announced {
            None => status_line(path),
            // A migration, not a fresh connect.
            Some(_) => format!(
                "oxutrm  path migrated \u{2192} {}  \u{b7}  {} ms",
                oxutrm_client::rung_label(path),
                path.rtt_ms
            ),
        };
        writeln!(out, "{line}").context("announcing the path")?;
        out.flush().context("flushing the terminal")?;

        // The line was written outside the renderer's model of the screen, so
        // that model is now wrong by one row. Anything less than a full
        // repaint would leave the terminal and the model disagreeing.
        self.renderer.invalidate();
        self.announced = Some(path.clone());
        Ok(true)
    }

    /// One turn: send `input`, apply what arrived, repaint if it changed.
    pub fn turn<W: Write>(&mut self, input: &[u8], out: &mut W) -> Result<Turn> {
        self.turn_with(input, None, out)
    }

    /// [`ClientSession::turn`], plus a frame the caller has already taken off
    /// the link.
    ///
    /// [`ClientSession::run`] learns that a frame is ready by awaiting one,
    /// and awaiting one consumes it. Handing it back here is what lets the
    /// drain loop below see it in order with the rest.
    ///
    /// A parameter rather than a one-slot pushback buffer on [`FrameSource`],
    /// deliberately. That slot would be mutable transport state which exactly
    /// one caller in the tree could use correctly — the shape this project has
    /// twice recorded as a mistake, because the rule for using it lives
    /// nowhere the compiler can see. Here the frame is simply owned by whoever
    /// is holding it, and there is nowhere for it to be stranded.
    pub fn turn_with<W: Write>(
        &mut self,
        input: &[u8],
        first: Option<Frame>,
        out: &mut W,
    ) -> Result<Turn> {
        let mut turn = Turn::default();

        if !input.is_empty() {
            let next = self.input_tx.current().append(input, self.size);
            self.input_tx.update(next);
            // A keystroke waits for nothing: pacing governs how often the
            // screen is offered, not how fast typing reaches the shell.
            self.last_send = None;
        }

        // ---- inbound: the screen -------------------------------------------
        self.take_frames(first, out, &mut turn)?;

        // ---- outbound: keystrokes and the size we want ---------------------
        turn.sent = self.offer_frame();
        Ok(turn)
    }

    /// Apply everything waiting on the link and repaint if anything landed.
    ///
    /// The inbound half of [`ClientSession::turn_with`], on its own so the
    /// end of a session can run it without the outbound half: once the
    /// connection is closed there is nobody left to offer a frame to, and
    /// asking a dead link to send one would only produce noise.
    fn take_frames<W: Write>(
        &mut self,
        first: Option<Frame>,
        out: &mut W,
        turn: &mut Turn,
    ) -> Result<()> {
        let mut painted = false;
        let mut next = first;
        while let Some(frame) = next.take().or_else(|| self.link.source.try_recv()) {
            // Streams can complete out of order. `on_frame` answers that from
            // the frame's own sequence numbers: an older one is Ok(false).
            match self.screen_rx.on_frame(&frame) {
                Ok(true) => {
                    turn.applied += 1;
                    painted = true;
                    // Wherever a frame is applied, the host was heard. The
                    // loop's `Wake::Frame` arm is not the only path here:
                    // `try_recv` below scavenges frames on pacing, keyboard
                    // and resize laps, and a frame applied on one of those
                    // used to repaint the screen underneath a box still
                    // saying nobody was answering. It also moves `last_heard`,
                    // which is what the `silent for Ns` counter is built from.
                    self.note_heard(Instant::now());
                }
                Ok(false) => {}
                // See the host's copy of this arm: a silently swallowed
                // BaseMismatch is a frozen screen that looks like a slow one.
                Err(_) => {
                    turn.rejected += 1;
                    // NOT `eprintln!`: the client's stderr is the terminal it
                    // is painting, so a message here desynchronises the
                    // renderer's model and nothing repaints it on a quiet
                    // session. The count reaches the user through the notice.
                    self.rejected_total = self.rejected_total.saturating_add(1);
                }
            }
        }
        if painted {
            self.renderer
                .render(out, self.screen_rx.state())
                .context("painting the terminal")?;
            out.flush().context("flushing the terminal")?;
        }
        Ok(())
    }

    /// Paint everything the host managed to deliver before it closed.
    ///
    /// The frames this collects are **not in flight**. They have arrived, been
    /// decoded, and are sitting in an mpsc channel; nothing on the network can
    /// lose them any more, and only returning early can. `tokio::select!`
    /// picks at random among ready arms, so once `conn.closed()` has fired,
    /// every queued frame had roughly even odds per lap of never being
    /// painted — which is to say the last screen of a session was a coin toss
    /// even when the host had delivered it perfectly.
    ///
    /// This terminates rather than hanging: a closed connection retires the
    /// datagram reader and the stream acceptor, and quinn deliberately lets
    /// already-received streams be drained from a closed connection ("which
    /// are necessarily finite"), so every sender is eventually dropped and
    /// `recv` yields `None`. The timeout is belt and braces on the one path
    /// where the user's own terminal is what is being held up.
    async fn drain<W: Write>(&mut self, out: &mut W) -> Result<Turn> {
        let mut turn = Turn::default();
        let drained = tokio::time::timeout(FINAL_DRAIN, async {
            while let Some(frame) = self.link.source.recv().await {
                self.take_frames(Some(frame), out, &mut turn)?;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await;
        // A timeout is not an error — the user is still owed their shell's
        // status. A failure to paint is, and is not swallowed by the wrapper.
        if let Ok(result) = drained {
            result?;
        }
        Ok(turn)
    }

    fn offer_frame(&mut self) -> Option<SendOutcome> {
        if !self.due() {
            return None;
        }
        self.input_tx.on_ack(self.screen_rx.peer_ack());
        let frame = match self.input_tx.make_frame(self.screen_rx.ack()) {
            Ok(Some(f)) => f,
            Ok(None) | Err(_) => return None,
        };
        self.last_send = Some(Instant::now());
        Some(self.link.sink.send(&frame))
    }

    fn due(&self) -> bool {
        match self.last_send {
            None => true,
            Some(t) => t.elapsed() >= self.link.sink.pacing_interval(),
        }
    }

    /// For tests and for the notice.
    pub fn rejected_total(&self) -> u64 {
        self.rejected_total
    }

    /// The clock is a parameter so the loop's behaviour can be tested without
    /// sleeping, exactly as `LinkState` is.
    fn note_heard(&mut self, now: Instant) {
        self.link_state.heard(now);
    }

    fn note_sent(&mut self, now: Instant) {
        self.link_state.sent(now);
    }

    /// One read from the keyboard, sent wherever it belongs.
    ///
    /// While a notice is showing the keyboard belongs to layer 1 -- and only
    /// then. A healthy session passes every byte to the host untouched.
    ///
    /// Which keys are commands is decided in [`LinkState::hold_keys`], from
    /// the phase, because it is decided by which box the user is reading:
    /// `Ctrl-\ q` under all of them, `s` and `d` only under `Confirming`.
    ///
    /// `Some(code)` means the user asked to close oxutrm, which is the one
    /// answer that ends the loop. A method rather than the body of the
    /// `Wake::Keys` arm because holding someone's typing through an outage and
    /// giving it back afterwards is the whole user-visible payload of this
    /// phase, and the arm itself cannot be reached from a test without a real
    /// terminal and a real two-second silence. The arm is left as a single
    /// call to this, so what is tested is what ships.
    fn route_keys<W: Write>(&mut self, keys: &[u8], out: &mut W) -> Result<Option<i32>> {
        if self.shown.is_none() {
            self.turn(keys, out)?;
            return Ok(None);
        }
        match self.link_state.hold_keys(keys) {
            Some(Command::Quit) => return Ok(Some(0)),
            Some(Command::SendHeld) => {
                let held = self.link_state.take_held();
                self.turn(&held, out)?;
            }
            Some(Command::DropHeld) => self.link_state.drop_held(),
            None => {}
        }
        Ok(None)
    }

    /// Say something, purely so that an answer is owed.
    ///
    /// A heartbeat exists to be answered: without one, an idle session cannot
    /// tell an outage from calm. `append(&[], size)` bumps the sequence
    /// exactly as `resize` does, which makes `state_moved` true and obliges
    /// the host to reply.
    ///
    /// Reports whether one went out, which is what lets a test hold the clock
    /// still and ask.
    fn heartbeat(&mut self, now: Instant) -> bool {
        if !self.link_state.heartbeat_due(now) {
            return false;
        }
        let next = self.input_tx.current().append(&[], self.size);
        self.input_tx.update(next);
        self.last_send = None;
        self.note_sent(now);
        true
    }

    /// What layer 1 should be showing at `now`, if anything.
    ///
    /// The text of a box already on the screen is refreshed at most once per
    /// [`NOTICE_REFRESH`]; a box that is not there yet, or that belongs to a
    /// different phase, is built at once. So the numbers settle down to
    /// something readable while the thing the numbers are ABOUT is still
    /// reported the instant it changes.
    fn notice_at(&mut self, now: Instant) -> Option<Notice> {
        let owed = self.input_tx.current().seq() != self.screen_rx.peer_ack();
        let phase = self.link_state.evaluate(now, owed);

        // Only `Silent` carries numbers that move on their own. `Confirming`
        // shows the held buffer, which changes only when the user types --
        // and when they do, they should see it.
        //
        // `built_for == phase` and not merely "a box is up": the box already
        // there has to be THIS phase's, or entering `Silent` would itself be
        // delayed by whenever the previous box happened to be built.
        if let Phase::Silent { .. } = phase
            && let Some((built_for, built_at)) = self.built
            && let Some(shown) = self.shown.as_ref()
            && built_for == phase
            && now.duration_since(built_at) < NOTICE_REFRESH
        {
            return Some(shown.clone());
        }
        self.built = Some((phase, now));

        match phase {
            Phase::Live => None,
            Phase::Silent { since } => {
                // Truncated, which reads as "at least this long" — the same
                // thing every stopwatch and `uptime` says. A rounded counter
                // would show "6s" from 5.5 s onward, and for a fault the user
                // may act on, overstating an outage is the worse error.
                let quiet = now.duration_since(since).as_secs();
                let stats = self.link.sink.connection().stats();
                let mut body = vec![format!(
                    "silent for {quiet}s - sent {} - lost {}",
                    stats.path.sent_packets, stats.path.lost_packets
                )];
                if self.rejected_total() > 0 {
                    body.push(format!("screen frames rejected: {}", self.rejected_total()));
                }
                // Someone typing into a dead screen cannot tell "kept" from
                // "discarded" until the `Confirming` box appears -- and if
                // they press `Ctrl-\ q` before it does, they will leave
                // assuming their typing was thrown away. This is the only
                // place that can tell them while it still matters.
                let held = self.link_state.held().len();
                if held > 0 {
                    body.push(format!("{held} bytes typed since - kept, not sent"));
                    // Present tense, and here rather than only in the box that
                    // comes afterwards: a cap the user is told about after the
                    // fact is a cap they could not have done anything about.
                    if self.link_state.held_is_full() {
                        body.push("The buffer is full; later keys are not being kept.".to_string());
                    }
                }
                Some(Notice {
                    headline: "no reply from host".to_string(),
                    body,
                    keys: vec![(
                        "Ctrl-\\ q".to_string(),
                        // What the key DOES, not what the host is doing. The
                        // silence being reported has a crashed host among its
                        // plausible causes, so "your shell keeps running on
                        // the host" -- which this used to say -- was the one
                        // claim the client is in no position to make. A
                        // description of the local action stays true either
                        // way.
                        "closes oxutrm here; it does not touch the host".to_string(),
                    )],
                })
            }
            Phase::Confirming => {
                let held = crate::linkstate::render_held(self.link_state.held());
                let mut body = vec![
                    format!(
                        "You typed {} bytes while offline:",
                        self.link_state.held().len()
                    ),
                    held,
                ];
                if self.link_state.held_is_full() {
                    body.push("The buffer is full; later keys were not kept.".to_string());
                }
                Some(Notice {
                    // What was observed, which is a frame arriving. Nothing
                    // reconnected: the QUIC connection never dropped, it went
                    // quiet and came back, and phase 1 has no reconnection
                    // machinery for a headline to imply. "reconnected" -- which
                    // this used to say -- named a mechanism oxutrm does not yet
                    // have.
                    headline: "the host is answering again - deliver what you typed?".to_string(),
                    body,
                    keys: vec![
                        ("Ctrl-\\ s".to_string(), "send it to the shell".to_string()),
                        ("Ctrl-\\ d".to_string(), "drop it".to_string()),
                    ],
                })
            }
        }
    }

    /// The window changed size. The renderer forgets what is painted and the
    /// next input diff tells the host.
    pub fn resize(&mut self, size: TermSize) {
        if size == self.size {
            return;
        }
        self.renderer.resize(size);
        self.size = size;

        // Layer 1 was laid out for the screen that just went away. Nothing
        // else will notice: the loop rebuilds the overlay only when the
        // notice's CONTENT changes, and a resize does not change a word of it,
        // so a `Confirming` notice — the one the user sits and reads, because
        // it is asking them a question — would keep the old geometry until
        // they pressed a key.
        //
        // The same content, laid out again, rather than `self.shown = None`:
        // `shown` has to keep mirroring what the overlay actually is. Clearing
        // it would leave the renderer holding a box that `shown` says is not
        // there, and on any lap where `notice_at` also returns `None` the
        // loop's `notice != self.shown` test reads `None != None` — false — so
        // `set_overlay(None)` never runs and the box is stranded on the screen
        // for the rest of the session.
        if let Some(n) = self.shown.as_ref() {
            self.renderer.set_overlay(Some(layout_notice(n, size)));
        }

        // Carried on the next diff, and worth going immediately: the shell is
        // drawing at the wrong width until it lands.
        let next = self.input_tx.current().append(&[], size);
        self.input_tx.update(next);
        self.last_send = None;
    }

    /// Drive the session until the shell exits or the link closes.
    ///
    /// The caller owns raw mode. [`oxutrm_client::RawGuard`] is entered before
    /// this and dropped after it, so `run` touches the terminal's bytes and
    /// never its settings.
    pub async fn run<W: Write>(&mut self, out: &mut W) -> Result<i32> {
        // `/dev/tty`, and NOT a duplicate of fd 0. That is not a detail.
        //
        // `AsyncFd` requires the descriptor to be non-blocking, and
        // `O_NONBLOCK` lives on the open file DESCRIPTION, which a `dup`
        // shares with the original. Making our copy non-blocking would
        // therefore make the user's shell's stdin non-blocking too, for the
        // rest of that shell's life, with nothing left to restore it —
        // `RawGuard` restores termios, which is a different thing entirely.
        // Opening the terminal afresh gives a description of our own to
        // spoil. It is the same terminal and the same input queue, so not a
        // keystroke is lost.
        let tty = Self::open_keyboard().context("opening the terminal to read the keyboard")?;
        self.run_on(tty, out).await
    }

    /// Open the controlling terminal, by a path `kqueue` will accept.
    ///
    /// **Not `/dev/tty`**, even though that is precisely the name for this.
    /// macOS refuses to register `/dev/tty` with `kqueue`: `EVFILT_READ` on it
    /// fails with `EINVAL` whichever mode it was opened in, so `AsyncFd`
    /// cannot watch it and the client dies the instant it tries — which is
    /// exactly what it did on the first two-machine run, immediately after ICE
    /// had punched and QUIC was up.
    ///
    /// `ttyname` of a descriptor that IS a terminal gives the device's own
    /// path (`/dev/ttys010`), and `kqueue` accepts that. Measured, both ways,
    /// inside a real tty; Linux accepts either.
    ///
    /// This keeps the property the fresh open exists for. `AsyncFd` requires a
    /// non-blocking descriptor, and `O_NONBLOCK` lives on the open file
    /// DESCRIPTION, which a `dup` shares: making a duplicate of fd 0
    /// non-blocking would make the user's shell's stdin non-blocking too, for
    /// the rest of that shell's life, with nothing left to restore it
    /// (`RawGuard` restores termios, which is a different thing entirely).
    /// Opening the device path afresh gives a description of our own to spoil.
    /// Same terminal, same input queue, not a keystroke lost.
    ///
    /// `/dev/tty` stays as the last candidate rather than disappearing: where
    /// none of 0/1/2 is a terminal but a controlling terminal exists, it is
    /// the only way to reach one, and on Linux it registers fine. A candidate
    /// list and not a `cfg`, so both platforms walk the same code.
    fn open_keyboard() -> std::io::Result<std::fs::File> {
        use std::os::fd::AsFd as _;
        use std::os::unix::ffi::OsStrExt as _;

        // `rustix::stdio` and not `BorrowedFd::borrow_raw`: `src/main.rs` is
        // `forbid(unsafe_code)`, and these are the same three descriptors
        // without the unsafe block.
        let standard = [
            rustix::stdio::stdin(),
            rustix::stdio::stdout(),
            rustix::stdio::stderr(),
        ];
        for fd in standard {
            if !rustix::termios::isatty(fd) {
                continue;
            }
            let Ok(name) = rustix::termios::ttyname(fd, Vec::new()) else {
                continue;
            };
            let path = std::path::Path::new(std::ffi::OsStr::from_bytes(name.as_bytes()));
            if let Ok(file) = std::fs::File::options().read(true).open(path) {
                // It named a terminal a moment ago; confirm what we actually
                // opened is one, rather than trusting the name.
                if rustix::termios::isatty(file.as_fd()) {
                    return Ok(file);
                }
            }
        }
        std::fs::File::options().read(true).open("/dev/tty")
    }

    /// [`ClientSession::run`], reading keystrokes from `keys`.
    ///
    /// The split exists so the loop can be tested, and it is the same reason
    /// [`oxutrm_client::RawGuard`] has `enter_on`: a test binary has no
    /// controlling terminal, `AsyncFd` cannot watch a regular file — `epoll`
    /// refuses one outright — and a loop that can only run against a real
    /// terminal is a loop with no tests at all. A socket pair is pollable and
    /// behaves like a keyboard in every way this code can tell.
    pub async fn run_on<K, W>(&mut self, keys: K, out: &mut W) -> Result<i32>
    where
        K: AsFd + AsRawFd + Read,
        W: Write,
    {
        // Done here rather than asked of the caller: a blocking descriptor
        // makes `try_io` below block the whole runtime on a keystroke that
        // never comes, and that is not a mistake to leave available.
        rustix::io::ioctl_fionbio(keys.as_fd(), true)
            .context("making the keyboard non-blocking")?;

        // The window size is asked of the KEYBOARD's descriptor. In `run` that
        // is `/dev/tty` — the controlling terminal, and the only descriptor in
        // this function that is certainly a terminal at all. Asking fd 1
        // instead means `oxutrm connect host > transcript.txt`, typed by
        // somebody sitting in a real terminal, has no window size to read.
        //
        // Duplicated rather than borrowed so the question survives the
        // keyboard: end of file retires the read arm below, and a terminal
        // that stopped producing input still changes shape.
        let window = keys
            .as_fd()
            .try_clone_to_owned()
            .context("duplicating the terminal to read its size")?;
        let mut keys = Some(
            tokio::io::unix::AsyncFd::with_interest(keys, tokio::io::Interest::READABLE)
                .context("watching the keyboard")?,
        );

        // Cloned OUT of the session, so the arm that waits for the link to
        // close borrows a local instead of `self`. See `Wake`.
        let conn = self.link.sink.connection().clone();
        let mut winch =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                .context("watching for window size changes")?;

        let mut buf = [0u8; 8192];
        // Now, so the first lap sends immediately: an attach owes the host a
        // frame before anything has happened, because that frame is what
        // carries our ack of zero (R5).
        let mut deadline = tokio::time::Instant::now();

        loop {
            let wake = tokio::select! {
                r = keys_readable(&mut keys) => match r {
                    Ok(mut guard) => match guard.try_io(|k| k.get_mut().read(&mut buf)) {
                        Ok(Ok(n)) => Wake::Keys(n),
                        // A signal arrived mid-read. Nothing was lost and
                        // nothing is wrong; the next lap reads again.
                        //
                        // Measured rather than assumed, because the guard is
                        // worth having either way: every signal handler this
                        // process installs sets SA_RESTART — tokio's, through
                        // `signal_hook_registry`, and `RawGuard`'s own — and
                        // the descriptor is non-blocking besides, so oxutrm's
                        // own signals cannot produce this. A handler installed
                        // by anything else in the process still can, and the
                        // cost of being wrong is a killed remote shell against
                        // one match arm.
                        Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => Wake::Nothing,
                        Ok(Err(e)) => return Err(e).context("reading the keyboard"),
                        // Readiness that evaporated; `try_io` has already
                        // cleared it, so the next lap will wait properly.
                        Err(_) => Wake::Nothing,
                    },
                    Err(e) => return Err(e).context("waiting on the keyboard"),
                },
                Some(frame) = self.link.source.recv() => Wake::Frame(frame),
                Some(()) = winch.recv() => Wake::Winch,
                () = tokio::time::sleep_until(deadline) => Wake::Due,
                reason = conn.closed() => Wake::Closed(reason),
            };

            // Every borrow of `self` starts HERE, after the select expression
            // has ended and dropped the futures above.
            match wake {
                Wake::Nothing => continue,
                // End of file on the keyboard. The session lives on: output
                // still arrives and the screen still paints. A terminal that
                // went away is not a reason to kill a remote shell — surviving
                // exactly that is what this project is for.
                //
                // Retiring the arm is not tidiness either. A descriptor at end
                // of file is readable FOR EVER, so an arm left watching one
                // spins as fast as the runtime can go.
                Wake::Keys(0) => {
                    keys = None;
                    continue;
                }
                Wake::Keys(n) => {
                    if let Some(code) = self.route_keys(&buf[..n], out)? {
                        return Ok(code);
                    }
                }
                Wake::Frame(frame) => {
                    self.note_heard(Instant::now());
                    self.turn_with(&[], Some(frame), out)?;
                }
                // A window that cannot be measured is not a reason to end a
                // remote shell. The `Keys(0)` arm above makes exactly that
                // argument about a keyboard that went away, and this one has
                // to follow it: "the local terminal changed shape" killing a
                // live session is the precise opposite of what this project
                // is for.
                //
                // Two live failures, one remedy. The descriptor may not be a
                // terminal — `oxutrm connect host > transcript.txt` used to
                // die on the first resize with `ENOTTY`, in a real terminal.
                // And the report may be `0x0`, which emulators emit while
                // tearing down and some multiplexers emit transiently on
                // detach. In both cases the size we already have is the last
                // thing that was true, and the next resize corrects it.
                Wake::Winch => {
                    // A failure is ignored, and silently. Same reasoning as
                    // the rejected-frame arm: this used to print onto the
                    // screen it was describing.
                    if let Ok(size) = terminal_size_of(&window) {
                        self.resize(size);
                    }
                    self.turn(&[], out)?;
                }
                Wake::Due => {
                    self.turn(&[], out)?;
                }
                // The link is gone, but what already arrived over it is not.
                // Paint it before answering, or `ls; exit` shows the user
                // nothing at all.
                Wake::Closed(reason) => {
                    self.drain(out).await?;
                    return exit_code(&reason);
                }
            }

            // Layer 1. Rebuilt only when the content actually changed, so a
            // steady notice costs one comparison per lap rather than a layout.
            let now = Instant::now();
            let notice = self.notice_at(now);
            if notice != self.shown {
                self.renderer
                    .set_overlay(notice.as_ref().map(|n| layout_notice(n, self.size)));
                self.shown = notice;
                self.renderer
                    .render(out, self.screen_rx.state())
                    .context("painting the notice")?;
                out.flush().context("flushing the terminal")?;
            }

            // Follow the route if it moved. Inside the loop rather than on a
            // timer of its own: `follow_route` is gated on `Silent` and paced
            // by `ROUTE_PROBE_EVERY`, so a healthy session reaches this line
            // ten times a second and does nothing but one `matches!`.
            //
            // After the notice, so the box describing the silence is already
            // on the screen before anything is done about it -- and the user
            // is told nothing about the rebind, because a rebind that has not
            // restored contact yet is not something the client can honestly
            // report.
            let _ = self.follow_route(now);

            // The `bool` is for the tests, which hold the clock still and ask
            // whether a prod was due. The loop does not care: it prods or it
            // does not, and either way the next lap is the same.
            let _ = self.heartbeat(now);

            // The next WAKE-UP, which is a different thing from `due()`, and
            // conflating the two is a busy loop rather than an optimisation.
            //
            // `due()` is the floor on how often a frame may be offered, and it
            // is driven by `last_send`. But `offer_frame` only sets `last_send`
            // when `make_frame` actually produced a frame, and `make_frame`
            // returns `None` whenever neither side has moved and no ack is
            // owed — which is precisely what a quiet session looks like. So in
            // a quiet session `last_send` never advances, `due()` stays true,
            // and a deadline derived from it is always already in the past:
            // `sleep_until` returns instantly, every lap, for ever.
            //
            // Asking again one interval from NOW costs one cheap check per
            // interval when there is nothing to say, and nothing at all when
            // there is — typing and resizing both clear `last_send` and send
            // inside the very `turn` above.
            //
            // The pacing interval, unconditionally. There used to be a second
            // arm here for "the tick that refreshes the counters", taking
            // `Duration::from_secs(1).min(pacing_interval)` whenever a notice
            // was up. `pacing_interval` is `clamp(rtt/2, 8ms, 100ms)`, so that
            // `min` is always the pacing interval and both arms were the same
            // expression, below a comment describing a one-second tick that
            // did not exist. Nothing is lost by dropping it: the loop already
            // wakes at least ten times a second, which is ten times as often
            // as `NOTICE_REFRESH` lets the counters move.
            deadline = tokio::time::Instant::now() + self.link.sink.pacing_interval();
        }
    }

    /// Follow this machine's route to the host, if it moved.
    ///
    /// Returns whether the session socket was actually swapped.
    ///
    /// **Only while `Silent`**, per design spec 4.2: a rebind moves our source
    /// port, which invalidates a punched NAT hole, so doing it to a working
    /// path breaks the path in order to test it. `Silent` means a reply has
    /// been owed for `SILENT_AFTER` with none arriving -- the path is already
    /// not working, so there is nothing left to break.
    ///
    /// Nothing here may end the session. A machine in the middle of an outage
    /// is exactly where `connect` fails with `ENETUNREACH` and where binding a
    /// fresh socket fails, and this runs during precisely that. Every failure
    /// is "no answer this time"; the next probe asks again. Same rule as "a
    /// send failure must never end a session", applied to the thing most
    /// likely to fail.
    fn follow_route(&mut self, now: Instant) -> bool {
        if !matches!(self.link_state.phase_now(), Phase::Silent { .. }) {
            return false;
        }
        if self
            .probed_at
            .is_some_and(|last| now.duration_since(last) < crate::roam::ROUTE_PROBE_EVERY)
        {
            return false;
        }
        self.probed_at = Some(now);

        let peer = self.link.sink.connection().remote_address();
        let Ok(seen) = crate::roam::route_source(peer) else {
            // Unroutable right now, which is an ordinary reading mid-outage
            // and not a fault. The baseline is left alone: a route we cannot
            // see is not a route that moved.
            return false;
        };

        if !self.route.moved(seen) {
            // Nothing changed against the baseline `ClientSession::new` seeded
            // when the connection came up -- or that seeding probe failed and
            // there is no baseline at all, in which case this reading becomes
            // one and the NEXT probe can act on it.
            self.route.settle(seen);
            return false;
        }

        // The route moved. Bind a fresh socket the same way the ladder bound
        // the first one -- wildcard, preferring 443 -- and hand it to the live
        // connection. QUIC is identified by connection IDs, not addresses, so
        // the connection itself does not notice.
        let cfg = oxutrm_net::NetConfig::default();
        let Ok(bound) = oxutrm_net::bind_socket(&cfg) else {
            return false;
        };
        let Ok(socket) = crate::ladder::adopt(bound) else {
            return false;
        };
        if self.rebind(socket).is_err() {
            // The old socket is still in place and still the one quinn holds:
            // `Link::rebind` only assigns after `rebind_abstract` succeeded.
            return false;
        }

        // Only now, so a failed rebind leaves the old baseline and the next
        // probe tries again rather than believing it has already moved.
        self.route.settle(seen);
        true
    }

    /// Move to a new local socket without dropping the connection.
    ///
    /// Called by [`ClientSession::follow_route`]. See [`Link::rebind`].
    pub fn rebind(&mut self, socket: Arc<tokio::net::UdpSocket>) -> Result<()> {
        self.link.rebind(socket)?;
        // The path changed; what is on the terminal is still correct, so
        // nothing is repainted. Only the address moved.
        Ok(())
    }

    /// The screen as applied, for tests. The renderer is what the user sees;
    /// this is the state behind it.
    #[allow(dead_code)]
    pub fn screen(&self) -> &ScreenState {
        self.screen_rx.state()
    }

    /// Applied screen frames that carried a diff, and those that carried a
    /// whole screen. See `Receiver::applied_kinds`. For tests: the loop does
    /// not care which kind arrived, and that indifference is the point.
    #[allow(dead_code)]
    #[must_use]
    pub fn applied_kinds(&self) -> (u64, u64) {
        self.screen_rx.applied_kinds()
    }

    /// For tests. The loop tracks the size it was given and asks the terminal
    /// directly for the current one.
    #[allow(dead_code)]
    pub fn size(&self) -> TermSize {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `use super::*` reaches the session module's own imports, not `crate`'s
    // other modules, so the route pace has to be named explicitly.
    use crate::roam::ROUTE_PROBE_EVERY;
    use oxutrm_net::{generate_cert, quic_client, quic_server};
    use oxutrm_proto::{ClientSpki, HostSpki, NatType, Rung};

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

    fn size() -> TermSize {
        TermSize { cols: 40, rows: 10 }
    }

    fn path_of(rung: Rung, rtt_ms: u32, mtu: u16, probes: u32, nat: NatType) -> PathDescription {
        PathDescription {
            rung,
            local: "127.0.0.1:1".parse().unwrap(),
            remote: "203.0.113.7:443".parse().unwrap(),
            probes_sent: probes,
            nat_type: nat,
            rtt_ms,
            mtu,
        }
    }

    async fn udp() -> Arc<tokio::net::UdpSocket> {
        Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap())
    }

    /// A host and a client joined by a real QUIC connection on loopback.
    async fn pair(shell: &str) -> (HostSession, ClientSession) {
        pair_on("127.0.0.1:0", shell).await
    }

    async fn pair_on(client_bind: &str, shell: &str) -> (HostSession, ClientSession) {
        let (cert, key, fingerprint) = generate_cert().unwrap();
        // The client now has an identity of its own, and the host has to be
        // told about it before it can listen at all.
        let (client_cert, client_key, client_fp) = generate_cert().unwrap();

        let host_sock = udp().await;
        let host_addr = host_sock.local_addr().unwrap();
        let (host_ep, _permit, _stun) =
            quic_server(&host_sock, cert, key, ClientSpki::new(client_fp))
                .await
                .unwrap();

        let client_sock = Arc::new(tokio::net::UdpSocket::bind(client_bind).await.unwrap());
        let accepting = tokio::spawn(async move {
            let incoming = host_ep.accept().await.expect("an inbound connection");
            let conn = incoming.await.expect("a completed handshake");
            (conn, host_ep)
        });

        let (client_conn, client_ep, _cstun) = quic_client(
            &client_sock,
            host_addr,
            HostSpki::new(fingerprint),
            client_cert,
            client_key,
        )
        .await
        .unwrap();
        let (host_conn, host_ep) = accepting.await.unwrap();

        let host = HostSession::spawn(
            "/bin/sh",
            size(),
            200,
            Link::new(host_conn, host_ep, host_sock),
        )
        .unwrap();
        let client = ClientSession::new(
            size(),
            caps(),
            Link::new(client_conn, client_ep, client_sock),
        )
        .unwrap();

        // The caller decides what the shell runs; `spawn` above starts one, so
        // the script is fed as input instead, which is also how a real session
        // works.
        let mut host = host;
        host.term.write_input(shell.as_bytes()).unwrap();
        (host, client)
    }

    /// The bug the loopback tests structurally could not catch.
    ///
    /// `loopback.rs` calls `on_ack(rx.ack())` directly, handing the sender the
    /// receiver's ack in-process and bypassing the frame entirely. On a real
    /// link an ack travels ONLY on a frame — so a side that has nothing of its
    /// own to say must still send one, or it never acknowledges anything.
    ///
    /// A user who is only watching output never types. Before the fix the host
    /// stayed pinned to whatever base it last heard about, every subsequent
    /// diff arrived as `BaseMismatch`, and the screen froze — recovering only
    /// by accident, when 32 further updates evicted that base from the ring and
    /// the sender fell back to full states.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_that_stops_typing_keeps_receiving_output() {
        let (mut host, mut client) = pair_on("127.0.0.1:0", "").await;
        let mut out = Vec::new();

        // One burst of typing, then the client goes quiet for good.
        host.term
            .write_input(b"printf 'first\r\n'\n")
            .expect("write");
        assert!(
            drive(
                &mut host,
                &mut client,
                &mut out,
                Duration::from_secs(20),
                |_, c| { text(c.screen()).contains("first") }
            )
            .await,
            "the client never saw the first output at all"
        );

        // Well past STATE_RING updates, so a run that only recovered by ring
        // eviction would still be frozen here. Nothing is typed from now on.
        for i in 0..40 {
            host.term
                .write_input(format!("printf 'line-{i}\\r\\n'\n").as_bytes())
                .expect("write");
        }

        let caught_up = drive(
            &mut host,
            &mut client,
            &mut out,
            Duration::from_secs(30),
            |_, c| text(c.screen()).contains("line-39"),
        )
        .await;
        assert!(
            caught_up,
            "the client stopped receiving once it stopped typing: an ack can \
             only travel on a frame, so a silent side must still send one\n\
             --- client ---\n{}",
            text(client.screen())
        );
    }

    /// Turn both loops until `f` holds, or give up.
    async fn drive(
        host: &mut HostSession,
        client: &mut ClientSession,
        out: &mut Vec<u8>,
        budget: Duration,
        f: impl Fn(&HostSession, &ClientSession) -> bool,
    ) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            host.turn().expect("host turn");
            client.turn(&[], out).expect("client turn");
            if f(host, client) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        false
    }

    fn text(s: &ScreenState) -> String {
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
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_client_screen_matches_the_host_over_real_quic() {
        // The deliverable: two sessions, a real QUIC connection, a scripted
        // shell, and the client's replicated screen compared against the
        // host's authority.
        let (mut host, mut client) = pair("printf 'alpha\\r\\nbeta\\r\\ngamma\\r\\n'\n").await;
        let mut out = Vec::new();

        assert!(
            drive(
                &mut host,
                &mut client,
                &mut out,
                Duration::from_secs(20),
                |_, c| { text(c.screen()).contains("gamma") }
            )
            .await,
            "the client never saw the output; screen was {:?}",
            text(client.screen())
        );

        // Let the two settle, then compare in full.
        drive(
            &mut host,
            &mut client,
            &mut out,
            Duration::from_secs(3),
            |h, c| c.screen().seq == h.screen().seq,
        )
        .await;

        assert_eq!(text(client.screen()), text(host.screen()));
        assert_eq!(client.screen().validate(), Ok(()));
        assert!(!out.is_empty(), "the renderer must have painted something");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keystrokes_reach_the_shell_and_the_answer_comes_back() {
        let (mut host, mut client) =
            pair("read line; printf 'you said %s\\r\\n' \"$line\"\n").await;
        let mut out = Vec::new();

        drive(
            &mut host,
            &mut client,
            &mut out,
            Duration::from_millis(400),
            |_, _| false,
        )
        .await;
        client.turn(b"ping\n", &mut out).expect("send input");

        assert!(
            drive(
                &mut host,
                &mut client,
                &mut out,
                Duration::from_secs(20),
                |_, c| { text(c.screen()).contains("you said ping") }
            )
            .await,
            "got {:?}",
            text(client.screen())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn input_is_never_written_to_the_shell_twice() {
        // The receiver's `pending` holds bytes until the client's next diff
        // trims them, so a loop that wrote all of it every turn would send the
        // same keystrokes again and again.
        let (mut host, mut client) =
            pair("read a; read b; printf 'first=%s second=%s\\r\\n' \"$a\" \"$b\"\n").await;
        let mut out = Vec::new();
        drive(
            &mut host,
            &mut client,
            &mut out,
            Duration::from_millis(400),
            |_, _| false,
        )
        .await;

        client.turn(b"one\n", &mut out).expect("first");
        drive(
            &mut host,
            &mut client,
            &mut out,
            Duration::from_millis(600),
            |_, _| false,
        )
        .await;
        client.turn(b"two\n", &mut out).expect("second");

        assert!(
            drive(
                &mut host,
                &mut client,
                &mut out,
                Duration::from_secs(20),
                |_, c| { text(c.screen()).contains("second=two") }
            )
            .await,
            "got {:?}",
            text(client.screen())
        );
        assert!(
            text(client.screen()).contains("first=one second=two"),
            "input was duplicated or lost: {:?}",
            text(client.screen())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_runaway_writer_coalesces_rather_than_queueing() {
        // A flood must cost frames in proportion to TIME, never in proportion
        // to how much the shell wrote.
        //
        // This used to assert `frames <= turns`, which the loop below makes
        // true by construction - it counts at most one frame per turn - so it
        // could not go red for any change to any code. What can go red is the
        // ratio between frames and the output that actually went past, which
        // `scrollback_len` counts monotonically at the source. A loop that
        // queued states would need a frame per screen; this one absorbs a
        // whole read budget of output into a single state and sends that.
        //
        // The pacing interval is deliberately NOT the bound asserted here.
        // Measured under this flood a turn costs ~46 ms, five times the 8 ms
        // floor, so `due()` is never the thing gating and a bound built on it
        // would hold for free. Shrink `oxutrm_term`'s READ_BUDGET to a few
        // bytes and the ratio below fails, which the old form did not.
        let (mut host, mut client) = pair("yes oxutrm-flood\n").await;
        let mut out = Vec::new();

        let mut turns = 0u64;
        let mut frames = 0u64;
        let mut applied = 0u64;
        let mut rejected = 0u64;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(4);
        while Instant::now() < deadline {
            let t = host.turn().expect("host turn");
            if t.sent.is_some() {
                frames += 1;
            }
            let ct = client.turn(&[], &mut out).expect("client turn");
            applied += ct.applied as u64;
            rejected += ct.rejected as u64;
            turns += 1;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let elapsed = started.elapsed();
        // Lines that scrolled off the top: the volume of output, counted as it
        // happened rather than inferred from the screen.
        let scrolled = host.screen().scrollback_len;

        let (diffs, full_states) = client.applied_kinds();
        eprintln!(
            "{turns} turns, {frames} frames, {applied} applied, {rejected} rejected, \
             {diffs} diffs, {full_states} full states, \
             {scrolled} lines scrolled in {elapsed:?}"
        );
        assert!(turns > 40, "only {turns} turns; the test proves little");
        // Under a flood the sender is always ahead of the acknowledgement, so
        // every frame it sends names a base the client has already left. That
        // is the normal condition here, not an edge case — and the client must
        // apply those frames, because each carries a screen strictly NEWER
        // than the one it is showing. Rejecting them halves the delivered
        // frame rate and throws away the freshest screen in the client's own
        // hand; measured before this was fixed, 44 of 89 frames were dropped
        // this way, carrying 1196 of 1788 payload bytes.
        //
        // Zero, not "few": there is no loss on this loopback link and no
        // reordering that would make a rejection legitimate.
        assert_eq!(
            rejected,
            0,
            "{rejected} of {} frames were dropped as unapplicable; the client is \
             painting a screen it knows to be superseded",
            applied + rejected
        );

        // `rejected == 0` alone does NOT prove the diff path works, and this is
        // the assertion that says so.
        //
        // A full state carries `from_state == 0` and applies unconditionally,
        // so a session whose base handling is completely broken also rejects
        // nothing: the sender's ring runs dry, every frame degrades to a whole
        // screen, and the screen still converges. That regime satisfies the
        // assertion above exactly as a healthy one does. It is the same
        // accidental rescue that hid the base-drift defect until somebody
        // measured it, and a gate that cannot tell the two apart is not a gate.
        //
        // On this loopback link the sender's ring is never under pressure, so
        // the full-state fallback should be the FIRST frame and essentially
        // nothing else.
        assert!(
            diffs > full_states * 4,
            "{diffs} diffs against {full_states} full states: the client is \
             converging on the full-state rescue rather than on diffs, which \
             costs a round trip and the whole bandwidth saving, and which \
             `rejected == 0` cannot see"
        );
        assert!(
            scrolled > 10_000,
            "only {scrolled} lines scrolled past in {elapsed:?}; that is not a flood, \
             so the ratio below would prove nothing"
        );
        assert!(
            frames * 200 < scrolled,
            "{frames} frames carried {scrolled} scrolled-off lines: fewer than 200 \
             lines per frame means the loop is delivering screens one at a time \
             rather than replacing them"
        );
        assert!(
            text(client.screen()).contains("oxutrm-flood"),
            "the client should be current, not stuck behind a backlog: {:?}",
            text(client.screen())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_state_too_large_for_a_datagram_goes_on_a_stream() {
        // A big screen full of distinct content will not fit in one datagram,
        // so channel selection has to reach for a stream. That is the path
        // that replaces fragmentation, and it must actually be taken.
        let big = TermSize {
            cols: 200,
            rows: 60,
        };
        let (cert, key, fingerprint) = generate_cert().unwrap();
        let (client_cert, client_key, client_fp) = generate_cert().unwrap();
        let host_sock = udp().await;
        let host_addr = host_sock.local_addr().unwrap();
        let (host_ep, _permit, _s) = quic_server(&host_sock, cert, key, ClientSpki::new(client_fp))
            .await
            .unwrap();
        let client_sock = udp().await;
        let accepting = tokio::spawn(async move {
            let inc = host_ep.accept().await.unwrap();
            (inc.await.unwrap(), host_ep)
        });
        let (cc, ce, _cs) = quic_client(
            &client_sock,
            host_addr,
            HostSpki::new(fingerprint),
            client_cert,
            client_key,
        )
        .await
        .unwrap();
        let (hc, he) = accepting.await.unwrap();

        let mut host =
            HostSession::spawn("/bin/sh", big, 200, Link::new(hc, he, host_sock)).unwrap();
        let mut client = ClientSession::new(big, caps(), Link::new(cc, ce, client_sock)).unwrap();

        // Fill the screen with varied, poorly compressible content.
        host.term
            .write_input(b"i=0; while [ $i -lt 60 ]; do printf '\\033[3%dm%s-%d\\r\\n' $((i%8)) $(head -c 60 /dev/urandom | od -An -tx1 | tr -d ' \\n') $i; i=$((i+1)); done\n")
            .unwrap();

        let mut out = Vec::new();

        // Fill the host's screen with the CLIENT HELD BACK. With no ack from
        // the client the host has nothing to diff against, so every frame is a
        // full state — and a full state for a 200x60 screen of distinct
        // truecolor cells cannot fit in a datagram. That is the ring-miss and
        // first-attach case the stream path exists for, and holding the client
        // back reproduces it deterministically instead of hoping a diff
        // happens to come out large.
        // SIZE is what must pick the channel, so the test has to know the size
        // it is being measured against. A bare "a stream was used" flag is
        // satisfied by any reason at all - most importantly by a peer with
        // datagrams disabled, where every frame took a stream and no frame was
        // ever too large for anything. Pin the limit down first.
        let limit = host
            .link
            .sink
            .connection()
            .max_datagram_size()
            .expect("this test is meaningless unless datagrams are actually available");

        let fill_by = Instant::now() + Duration::from_secs(30);
        // The largest frame observed going out on a stream, against the
        // datagram limit in force when it went.
        let mut streamed_over_the_limit: Option<(usize, usize)> = None;
        while Instant::now() < fill_by {
            let t = host.turn().expect("host turn");
            if let Some(SendOutcome::Stream { bytes, .. }) = t.sent {
                let now = host
                    .link
                    .sink
                    .connection()
                    .max_datagram_size()
                    .expect("datagrams must not have gone away mid-test");
                // Compare against the limit in force AT THIS MOMENT, never a
                // cached one. quinn raises the datagram limit as path-MTU
                // discovery completes (observed here: 1382 -> 1414 mid-test),
                // so pinning the opening value made this test fail whenever
                // discovery happened to finish inside the window. That is the
                // very mistake `FrameSink::send` documents avoiding -- it asks
                // the connection per frame precisely because the limit moves.
                // The property under test is unchanged and still exact: this
                // frame took a stream BECAUSE it exceeded the limit that
                // applied when it was sent.
                if bytes > now {
                    streamed_over_the_limit = Some((bytes, now));
                }
            }
            if streamed_over_the_limit.is_some() && text(host.screen()).contains("-59") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        assert!(
            text(host.screen()).contains("-59"),
            "the shell never filled the screen, so nothing large was ever \
             offered\n--- host ---\n{}",
            text(host.screen())
        );
        let (bytes, limit_then) = streamed_over_the_limit.unwrap_or_else(|| {
            panic!(
                "no frame went on a stream BECAUSE it exceeded the {limit}-byte \
                 datagram limit, even with a full 200x60 truecolor screen and no ack \
                 to diff against. Either channel selection is not choosing by size, \
                 or nothing large was ever built"
            )
        });
        assert!(bytes > limit_then);

        // Now let the client in and wait for the screens to agree.
        let converged = drive(
            &mut host,
            &mut client,
            &mut out,
            Duration::from_secs(30),
            |h, c| text(c.screen()) == text(h.screen()),
        )
        .await;
        assert!(
            converged,
            "the client never caught up after the oversized state\n--- host ---\n{}\n--- client ---\n{}",
            text(host.screen()),
            text(client.screen())
        );
        assert_eq!(client.screen().validate(), Ok(()));
    }

    /// A detached session must stop working for a screen nobody will see -
    /// **and must keep draining the pty anyway**.
    ///
    /// Both halves matter, and the second is the one that bites. Skipping the
    /// drain is the obvious way to make a detached session cheap, and it
    /// deadlocks the child: a pty whose output nobody reads fills its buffer
    /// and the writer blocks forever. So the child here writes far more than
    /// the buffer holds and then exits, and the test waits for that exit. If
    /// the drain ever stops, the child never exits and this fails on its
    /// bound rather than hanging.
    ///
    /// Measured before the fix: a detached session whose child wrote five
    /// lines a second cost 17-20% of a core, indefinitely, with no way to
    /// reclaim it. A quiet one cost 1.2%, which is why it went unnoticed for
    /// so long - the cost is proportional to output, not to the poll.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_detached_session_stops_building_frames_but_keeps_draining() {
        // The `sleep` is load-bearing: the bulk output has to land AFTER the
        // detach, or the test proves nothing about draining while detached.
        // Without it this passed on macOS and failed on Linux under a loaded
        // full-suite run, because the child reached `exit` before the host had
        // observed the close - an ordering this test used to assume.
        let (mut host, client) =
            pair("printf 'before\\r\\n'; sleep 2; seq 1 20000; exit 0\n").await;

        // Up first, so we are measuring a detach and not a session that never
        // started.
        let mut out = Vec::new();
        let mut c = client;
        assert!(
            drive(
                &mut host,
                &mut c,
                &mut out,
                Duration::from_secs(20),
                |_, c| { text(c.screen()).contains("before") }
            )
            .await,
            "the session was not up before detaching"
        );

        // The client goes away without a graceful shutdown of the session.
        c.link.sink.connection().close(0u32.into(), b"gone");
        drop(c);

        // Wait for quinn to give the connection up, then drive the host alone.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut saw_detached = false;
        let mut exited = None;
        let mut frames_after_detach = 0usize;
        while Instant::now() < deadline {
            let turn = host.turn().expect("turn");
            if turn.detached {
                saw_detached = true;
                if turn.sent.is_some() {
                    frames_after_detach += 1;
                }
            }
            // Recorded on FIRST sight and not broken on: `child_exited` reports
            // `Some(-1)` once the child has been reaped, and leaving the loop
            // here is what made this test assume the child could not finish
            // before the close was noticed.
            if exited.is_none() {
                exited = turn.exited;
            }
            if saw_detached && exited.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        assert!(saw_detached, "the host never noticed the client was gone");
        assert_eq!(
            frames_after_detach, 0,
            "the host built frames for a peer that was gone"
        );
        // The child wrote ~100 KiB, far past any pty buffer, and then exited.
        // Reaching its exit is the proof that the drain never stopped.
        assert_eq!(
            exited,
            Some(0),
            "the child never exited, so the pty stopped being drained and it \
             blocked on a full buffer"
        );
    }

    /// One frame from the client, as the host's select would receive it.
    async fn client_frame(host: &mut HostSession) -> Frame {
        tokio::time::timeout(Duration::from_secs(5), host.link.source.recv())
            .await
            .expect("the client's frame never arrived")
            .expect("the source closed")
    }

    /// The property Task 2 takes away from quinn and this task gives back.
    /// A host whose client stopped speaking must stop building frames for it,
    /// or an abandoned session burns a core for ever on a screen nobody will
    /// see. Measured at 17-20% of a core for a child writing five lines a
    /// second, which is why this is not a micro-optimisation.
    #[tokio::test]
    async fn a_host_whose_client_went_quiet_detaches_on_its_own_clock() {
        let (mut host, _client) = pair("/bin/sh").await;
        let t = Instant::now();

        let turn = host.turn_at(t, None).expect("a turn while attached");
        assert!(
            !turn.detached,
            "detached while the client was still speaking"
        );

        let turn = host
            .turn_at(t + DETACH_AFTER + Duration::from_secs(1), None)
            .expect("a turn after the client went quiet");
        assert!(
            turn.detached,
            "the host is still building frames for a client that stopped \
             answering {DETACH_AFTER:?} ago"
        );
    }

    /// Detaching must not be a one-way door: the whole point of holding the
    /// connection open is that a peer coming back is heard instantly.
    ///
    /// `!turn.detached` alone would still pass if the `screen_stale` catch-up
    /// snapshot were dropped from `turn_at` -- undetached is not the same
    /// claim as caught up, and this reattach path only became reachable in
    /// this phase, so that gap is one this phase created. `turn.sent.is_some()`
    /// alone is not enough either: the returning client's own keystroke owes
    /// an ack, and an ack travels on a frame regardless of whether the screen
    /// moved, so that frame would exist even with `screen_stale` dropped. The
    /// only assertion that actually distinguishes the two is what the frame
    /// *contains* once applied on the client: the "moved" text the child
    /// wrote while nobody was attached.
    #[tokio::test]
    async fn a_returning_client_reattaches_the_host() {
        let (mut host, mut client) = pair("/bin/sh").await;
        let t = Instant::now();
        let late = t + DETACH_AFTER + Duration::from_secs(1);

        assert!(host.turn_at(late, None).expect("a turn").detached);

        // The screen moves while nobody is attached. A poll while still
        // detached is what sets `screen_stale`, so the catch-up on reattach
        // has something real to prove. Polled until the host's OWN terminal
        // has actually rendered "moved" -- bounded by a timeout rather than a
        // fixed lap count, so a loaded runner where `/bin/sh` takes longer
        // than a guessed budget to echo does not false-fail this test. A few
        // more laps once the marker first appears settle any trailing byte of
        // shell prompt BEFORE the reattach turn -- otherwise a late byte
        // landing exactly on the reattach turn's own poll would set `moved`
        // there too, and the mutation this test guards against would go
        // undetected for the wrong reason.
        host.term.write_input(b"echo moved\n").unwrap();
        let poll_budget = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut elapsed = Duration::ZERO;
        loop {
            assert!(
                tokio::time::Instant::now() < poll_budget,
                "the child's \"moved\" output never reached the host's own \
                 terminal within 10s of real polling"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
            elapsed += Duration::from_millis(20);
            host.turn_at(late + elapsed, None)
                .expect("a turn while still detached");
            if text(&host.term.snapshot(1)).contains("moved") {
                break;
            }
        }
        let mut still_away = None;
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            elapsed += Duration::from_millis(50);
            still_away = Some(
                host.turn_at(late + elapsed, None)
                    .expect("a turn while still detached"),
            );
        }
        assert!(
            still_away.expect("polled at least once").detached,
            "detached before the client had a chance to speak"
        );

        // The client speaks again. Any frame is evidence of a peer.
        let mut out = Vec::new();
        client.turn(b"x", &mut out).expect("the client types");
        let frame = client_frame(&mut host).await;

        let turn = host
            .turn_at(late + elapsed + Duration::from_millis(100), Some(frame))
            .expect("a turn with the client back");
        assert!(
            !turn.detached,
            "a client that came back was not heard; the screen would stay frozen"
        );
        assert!(
            turn.sent.is_some(),
            "the returning turn built no frame; the screen the client moved while \
             away would stay frozen even though `detached` cleared"
        );

        // The frame the reattach turn sent has to be RECEIVED and APPLIED to
        // find out whether it is the catch-up or an empty ack-only diff of a
        // stale base -- `turn.sent.is_some()` is true either way.
        let reattach_frame =
            tokio::time::timeout(Duration::from_secs(5), client.link.source.recv())
                .await
                .expect("the host's reattach frame never arrived")
                .expect("the source closed");
        client
            .turn_with(&[], Some(reattach_frame), &mut out)
            .expect("the client applies the reattach frame");
        assert!(
            text(client.screen()).contains("moved"),
            "the returning turn did not carry the screen the child moved while \
             nobody was attached -- an ack-only frame of a stale base would \
             also satisfy `turn.sent.is_some()`, so this is the real guard\n\
             --- client ---\n{}",
            text(client.screen())
        );
    }

    /// A blip is not a departure. HEARTBEAT_IDLE is 5s and DETACH_AFTER is 30s,
    /// and the gap between them is what stops an ordinary quiet moment from
    /// freezing the emulator behind the child.
    #[tokio::test]
    async fn an_ordinary_quiet_moment_does_not_detach_the_host() {
        let (mut host, _client) = pair("/bin/sh").await;
        let t = Instant::now();

        let turn = host
            .turn_at(t + crate::linkstate::HEARTBEAT_IDLE * 2, None)
            .expect("a turn a couple of heartbeats in");
        assert!(
            !turn.detached,
            "detached after two heartbeats; a quiet session is not an absent one"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_session_survives_the_client_changing_its_own_local_address() {
        // The headline feature: a session that outlives an IP change. This is
        // the ONLY migration QUIC has - a client may change its own local
        // address. There is deliberately no test that moves the REMOTE
        // address: the protocol has no mechanism for it, and a test built on
        // that assumption would invite someone to "fix" it later by adding
        // something that cannot exist.
        //
        // The move is to a different local IP, not merely a different port.
        // 127.0.0.0/8 is entirely loopback on Linux, so 127.0.0.2 is a real
        // second address to migrate onto, and the 4-tuple changes in both
        // halves rather than one.
        // Roaming needs a SECOND local address to move to. Linux has all of
        // 127.0.0.0/8 on `lo`; macOS gives `lo0` only 127.0.0.1, and a second
        // one needs `sudo ifconfig lo0 alias 127.0.0.2`, which a test may not
        // require. Probed rather than cfg-ed, so a host that has the alias
        // runs this on either platform.
        if tokio::net::UdpSocket::bind("127.0.0.2:0").await.is_err() {
            eprintln!(
                "SKIP the_session_survives_the_client_changing_its_own_local_address: \
                 this host has no second loopback IP \
                 (macOS: sudo ifconfig lo0 alias 127.0.0.2)"
            );
            return;
        }
        let (mut host, mut client) = pair_on("127.0.0.1:0", "printf 'before-roam\r\n'\n").await;
        let mut out = Vec::new();

        assert!(
            drive(
                &mut host,
                &mut client,
                &mut out,
                Duration::from_secs(20),
                |_, c| { text(c.screen()).contains("before-roam") }
            )
            .await,
            "the session was not up before roaming"
        );
        let seq_before = client.screen().seq;

        let old_addr = client.link.socket.local_addr().unwrap();
        assert_eq!(old_addr.ip().to_string(), "127.0.0.1");

        let moved = Arc::new(tokio::net::UdpSocket::bind("127.0.0.2:0").await.unwrap());
        client.rebind(moved).expect("rebind");
        let new_addr = client.link.socket.local_addr().unwrap();

        assert_ne!(
            old_addr.ip(),
            new_addr.ip(),
            "the local IP must actually have changed"
        );
        assert_eq!(new_addr.ip().to_string(), "127.0.0.2");

        // The screen already painted is still correct: migration moves an
        // address, it does not reset state.
        assert_eq!(client.screen().seq, seq_before);
        assert!(text(client.screen()).contains("before-roam"));

        // And new output crosses the new path.
        host.term.write_input(b"printf 'after-roam\r\n'\n").unwrap();
        assert!(
            drive(
                &mut host,
                &mut client,
                &mut out,
                Duration::from_secs(20),
                |_, c| { text(c.screen()).contains("after-roam") }
            )
            .await,
            "the session did not survive the rebind; screen was {:?}",
            text(client.screen())
        );

        // Both halves still agree afterwards.
        drive(
            &mut host,
            &mut client,
            &mut out,
            Duration::from_secs(5),
            |h, c| c.screen().seq == h.screen().seq,
        )
        .await;
        assert_eq!(text(client.screen()), text(host.screen()));
        assert_eq!(client.screen().validate(), Ok(()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_connect_line_is_printed_once_and_then_there_is_silence() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let mut out = Vec::new();

        let path = path_of(Rung::StunPunch, 38, 1392, 0, NatType::EndpointIndependent);
        assert!(client.announce(&path, &mut out).expect("announce"));
        let first = String::from_utf8(out.clone()).unwrap();
        assert!(first.contains("oxutrm"), "got {first:?}");
        assert!(first.contains("punched"));
        assert!(first.contains("38 ms"));
        assert!(first.contains("mtu 1392"));
        assert_eq!(first.lines().count(), 1, "exactly one line");

        // Then silence: the same path announced again says nothing at all.
        out.clear();
        assert!(!client.announce(&path, &mut out).expect("announce"));
        assert!(
            out.is_empty(),
            "a repeat announcement must be silent, got {out:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_path_change_announces_itself() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let mut out = Vec::new();

        client
            .announce(
                &path_of(Rung::StunPunch, 38, 1392, 0, NatType::EndpointIndependent),
                &mut out,
            )
            .expect("first");
        out.clear();

        // Walking from Wi-Fi to mobile should be explained, not mysterious.
        let better = path_of(Rung::Ipv6Direct, 11, 1452, 0, NatType::None);
        assert!(client.announce(&better, &mut out).expect("second"));
        let line = String::from_utf8(out).unwrap();
        assert!(line.contains("migrated"), "got {line:?}");
        assert!(line.contains("IPv6 direct"), "got {line:?}");
        assert!(line.contains("11 ms"), "got {line:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rung_four_reads_as_a_warning() {
        // A session inside the SSH connection cannot daemonize and cannot be
        // reattached. Degrading to it silently would remove both properties
        // the project exists to provide, while looking like success.
        let (_host, mut client) = pair("sleep 30\n").await;
        let mut out = Vec::new();
        client
            .announce(
                &path_of(Rung::SshTunnel, 45, 1200, 0, NatType::Unknown),
                &mut out,
            )
            .expect("announce");
        let line = String::from_utf8(out).unwrap();
        assert!(line.contains("[warning]"), "got {line:?}");
        assert!(line.contains("SSH tunnel"), "got {line:?}");
        assert!(
            line.contains("not detachable"),
            "the user must be told what this connection cannot do: {line:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_birthday_path_reports_what_it_cost() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let mut out = Vec::new();
        client
            .announce(
                &path_of(Rung::Birthday, 61, 1200, 312, NatType::Symmetric),
                &mut out,
            )
            .expect("announce");
        let line = String::from_utf8(out).unwrap();
        assert!(
            line.contains("312 probes"),
            "the cost must be visible: {line:?}"
        );
        assert!(line.contains("symmetric NAT"), "got {line:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_resize_travels_from_the_client_to_the_shell() {
        let (mut host, mut client) = pair("sleep 30\n").await;
        let mut out = Vec::new();
        drive(
            &mut host,
            &mut client,
            &mut out,
            Duration::from_millis(500),
            |_, _| false,
        )
        .await;

        let bigger = TermSize {
            cols: 100,
            rows: 30,
        };
        client.resize(bigger);

        assert!(
            drive(
                &mut host,
                &mut client,
                &mut out,
                Duration::from_secs(20),
                |h, _| { h.size == bigger }
            )
            .await,
            "the host never resized"
        );
        assert!(
            drive(
                &mut host,
                &mut client,
                &mut out,
                Duration::from_secs(20),
                |_, c| { c.screen().cols == 100 && c.screen().rows == 30 }
            )
            .await,
            "the new geometry never came back: {:?}",
            (client.screen().cols, client.screen().rows)
        );
        assert_eq!(client.screen().validate(), Ok(()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_host_notices_when_the_shell_exits() {
        let (mut host, mut client) = pair("exit 9\n").await;
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let t = host.turn().expect("host turn");
            client.turn(&[], &mut out).expect("client turn");
            if let Some(code) = t.exited {
                assert_eq!(code, 9);
                return;
            }
            assert!(Instant::now() < deadline, "the shell never exited");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pacing_comes_from_the_connection_and_stays_within_its_bounds() {
        let (host, _client) = pair("sleep 30\n").await;
        let interval = host.link.sink.pacing_interval();
        assert!(
            interval >= Duration::from_millis(8) && interval <= Duration::from_millis(100),
            "pacing interval {interval:?} is outside clamp(rtt/2, 8ms, 100ms)"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_host_sets_term_from_the_emulator_and_not_from_the_client() {
        // negotiate_term takes no arguments, so a differently-capable client
        // reattaching cannot change the child's TERM.
        let (term_name, colorterm) = oxutrm_term::negotiate_term();
        assert_eq!(term_name, "xterm-256color");
        assert_eq!(colorterm.as_deref(), Some("truecolor"));

        let (mut host, mut client) = pair("printf 'TERM=%s\\r\\n' \"$TERM\"\n").await;
        let mut out = Vec::new();
        assert!(
            drive(
                &mut host,
                &mut client,
                &mut out,
                Duration::from_secs(20),
                |_, c| { text(c.screen()).contains("TERM=xterm-256color") }
            )
            .await,
            "the shell's TERM was not what the emulator supports: {:?}",
            text(client.screen())
        );
    }

    // ---- ClientSession::run ------------------------------------------------

    /// A socket pair standing in for the keyboard.
    ///
    /// The first half goes to the session, the second is what a person would
    /// be typing on. A socket pair is pollable, which a regular file is not:
    /// `epoll` refuses one outright, so a test that fed the loop a temporary
    /// file would fail at `AsyncFd::with_interest` and prove nothing about the
    /// loop at all.
    fn keyboard() -> (
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ) {
        std::os::unix::net::UnixStream::pair().expect("a socket pair")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_carries_typing_to_the_shell_and_its_exit_code_back() {
        // One assertion, both directions. The shell can only exit 42 because
        // the client read "exit 42" off the keyboard and put it on the wire,
        // and the client can only report 42 because the host hung it on the
        // QUIC close and this loop unpicked it.
        let (mut host, mut client) = pair("").await;
        let (keys, mut typing) = keyboard();
        let host_loop = tokio::spawn(async move { host.run().await });

        typing.write_all(b"exit 42\n").expect("type");

        let mut out = Vec::new();
        let code = tokio::time::timeout(Duration::from_secs(20), client.run_on(keys, &mut out))
            .await
            .expect("the client loop never finished")
            .expect("the client loop failed");

        assert_eq!(code, 42, "the exit status did not survive the trip");
        assert_eq!(
            host_loop.await.expect("host task").expect("host loop"),
            42,
            "the host disagrees about what the shell did"
        );
    }

    /// `ls; exit` — the single most user-visible thing this project can get
    /// wrong.
    ///
    /// This test used to say `printf ...; sleep 1; exit 0`, with a comment
    /// calling the sleep load-bearing "because closing a QUIC connection
    /// discards whatever is still in flight". The comment was right that
    /// something was rescuing the test and wrong about what: the sleep was not
    /// working around a property of QUIC, it was working around three defects
    /// in this file, and it is a rescue no real user has. A shell that prints
    /// and exits in the same breath is not an edge case — it is what every
    /// last command of every session does.
    ///
    /// Measured before the fix, with the sleep removed and nothing else
    /// changed: 30 runs, 30 failures, screen entirely blank. Not flaky —
    /// reliably red. The sleep stays out so this can fail again.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_paints_what_the_host_sent() {
        let (mut host, mut client) = pair("").await;
        let (keys, mut typing) = keyboard();
        let host_loop = tokio::spawn(async move { host.run().await });

        typing
            .write_all(b"printf 'marker-here\\r\\n'\nexit 0\n")
            .expect("type");

        let mut out = Vec::new();
        let code = tokio::time::timeout(Duration::from_secs(30), client.run_on(keys, &mut out))
            .await
            .expect("the client loop never finished")
            .expect("the client loop failed");

        assert_eq!(code, 0);
        assert!(
            text(client.screen()).contains("marker-here"),
            "the loop never took the host's output in; screen was {:?}",
            text(client.screen())
        );
        assert!(!out.is_empty(), "the renderer was never asked to paint");
        let _ = host_loop.await;
    }

    /// A burst bigger than `READ_BUDGET`, then silence, then a marker — all
    /// the way through the real loops rather than a hand-driven turn.
    ///
    /// **What this does NOT guard, stated because it was checked.** It was
    /// written to catch the event-driven loop sleeping on bytes it had not
    /// read, and it does not: with the `more_output_waiting` check removed it
    /// still passes, three runs out of three. The child's own later writes
    /// each supply a fresh readiness edge, and an attached client's acks wake
    /// the loop besides, so the backlog gets drained anyway. It earns its
    /// place as the only test that pushes more than `READ_BUDGET` through the
    /// real loops at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_carries_a_burst_bigger_than_the_read_budget() {
        let (mut host, mut client) = pair("").await;
        let (keys, mut typing) = keyboard();
        let host_loop = tokio::spawn(async move { host.run().await });

        typing
            .write_all(b"seq 1 40000; printf 'tail-marker\\r\\n'; sleep 3; exit 0\n")
            .expect("type");

        let mut out = Vec::new();
        let code = tokio::time::timeout(Duration::from_secs(30), client.run_on(keys, &mut out))
            .await
            .expect("the client loop never finished")
            .expect("the client loop failed");

        assert_eq!(code, 0);
        assert!(
            text(client.screen()).contains("tail-marker"),
            "the tail of the burst never arrived: the loop slept on bytes it \
             had not read. Screen was {:?}",
            text(client.screen())
        );
        let _ = host_loop.await;
    }

    /// A DETACHED session whose child floods the PTY must still reach the
    /// child's exit — through the real loop, with nobody attached.
    ///
    /// This is the guard on the loop's core wiring: with nobody there, the
    /// PTY is the ONLY thing that can wake it. Fault injected — remove the
    /// pty arm from the select and this fails at its 30 s bound, with the
    /// child blocked writing into a buffer nobody is emptying, which is the
    /// same deadlock as skipping the drain arrived at from the other side.
    ///
    /// It does NOT discriminate the `more_output_waiting` refinement: with
    /// that check removed it still passes, because the child's own writes and
    /// finally the exit wake supply the edges. See the note in `run`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_detached_session_still_drains_a_flooding_child_to_its_exit() {
        let (mut host, client) = pair("sleep 1; seq 1 40000; exit 7\n").await;

        // Take the peer away before the flood starts.
        client.link.sink.connection().close(0u32.into(), b"gone");
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(10);
        while host.link.sink.connection().close_reason().is_none() {
            host.turn().expect("turn");
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(Instant::now() < deadline, "the connection never closed");
        }

        let code = tokio::time::timeout(Duration::from_secs(30), host.run())
            .await
            .expect(
                "the detached loop never reached the child's exit: it is \
                     asleep on bytes it did not read, and the child is blocked \
                     writing into a PTY nobody is draining",
            )
            .expect("the host loop failed");
        assert_eq!(code, 7, "the child did not run to completion");
    }

    /// The client's half of the same bug, on its own.
    ///
    /// `run_paints_what_the_host_sent` above cannot show this one: on loopback
    /// the host now waits for its final frame to be acknowledged, which gives
    /// the client's loop time to wake on the frame and paint it long before
    /// the close lands. Measured — with the client's drain removed and the
    /// host's fix in place, that test passed 20 out of 20.
    ///
    /// So the drain is exercised where it is decidable instead: the whole
    /// session happens with the client's loop never running, so every frame
    /// the host delivered is decoded and sitting in an mpsc channel at the
    /// moment the connection closes. Those frames are not in flight and
    /// nothing on the network can lose them — only returning without looking.
    #[tokio::test(flavor = "multi_thread")]
    async fn frames_already_taken_off_a_closed_link_are_still_painted() {
        let (mut host, mut client) = pair("printf 'last-word\\r\\n'\nexit 3\n").await;

        // The host runs to completion by hand. The client is never driven, so
        // it acknowledges nothing and paints nothing.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut exited = None;
        while Instant::now() < deadline {
            if let Some(code) = host.turn().expect("host turn").exited {
                exited = Some(code);
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let code = exited.expect("the shell never exited");
        assert_eq!(code, 3);
        host.finish(code).await;

        // The connection is closed before a single frame has been looked at.
        let mut out = Vec::new();
        let turn = client
            .drain(&mut out)
            .await
            .expect("draining a closed link");
        assert!(
            turn.applied > 0,
            "nothing was taken off the link after it closed, so a session that \
             ends the moment it produces output shows the user nothing"
        );
        assert!(
            text(client.screen()).contains("last-word"),
            "the shell's last output was dropped along with the connection; \
             screen was {:?}",
            text(client.screen())
        );
        assert!(!out.is_empty(), "the drained frames were never painted");
    }

    /// A SIGWINCH used to be able to kill a live session.
    ///
    /// `terminal_size()` asked **fd 1**, and nothing in oxutrm requires fd 1
    /// to be a terminal: `RawGuard::enter` asserts `isatty(0)` and the
    /// keyboard is opened on `/dev/tty` by name. So `oxutrm connect host >
    /// transcript.txt`, typed by somebody sitting in a real terminal, died on
    /// the first window resize with `ENOTTY` — and the `?` on that `Err`
    /// carried it straight out of the loop. A `0x0` report, which emulators
    /// emit while tearing down, did the same through the `ensure!`.
    ///
    /// The keyboard here is a socket pair, which answers `tcgetwinsize`
    /// exactly the way a redirected stdout does. So this is that session:
    /// every resize is unmeasurable, and the shell must still be the thing
    /// that decides when the session ends.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_window_resize_that_cannot_be_measured_does_not_end_the_session() {
        let (mut host, mut client) = pair("").await;
        let (keys, mut typing) = keyboard();
        assert!(
            terminal_size_of(&keys).is_err(),
            "this test needs a keyboard that cannot answer tcgetwinsize"
        );
        let host_loop = tokio::spawn(async move { host.run().await });

        // A resize storm for the whole life of the session, so that a signal
        // certainly lands after `run_on` has installed its own listener —
        // otherwise this would be a test that cannot fail.
        let winching = tokio::spawn(async move {
            loop {
                let _ = rustix::process::kill_process(
                    rustix::process::getpid(),
                    rustix::process::Signal::WINCH,
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });

        // Typed only once the storm has been running for a while, so the
        // session outlives many resizes rather than racing the first one.
        let typist = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            typing.write_all(b"exit 7\n").expect("type");
            typing
        });

        let mut out = Vec::new();
        let code = tokio::time::timeout(Duration::from_secs(30), client.run_on(keys, &mut out))
            .await
            .expect("the client loop never finished")
            .expect(
                "a window resize ended the session: the local terminal changing shape \
                 is not a reason to kill a remote shell",
            );
        assert_eq!(code, 7, "the shell's own status did not come back");
        assert_eq!(
            client.size(),
            size(),
            "an unreadable window size was adopted anyway; the last size that WAS \
             measured is the only honest answer"
        );

        winching.abort();
        let _ = typist.await;
        let _ = host_loop.await;
    }

    #[tokio::test]
    async fn a_keyboard_at_end_of_file_neither_ends_the_session_nor_spins() {
        // Two failures, one test, because they are the two halves of the same
        // arm. Ending the session on end of file kills a live remote shell
        // because the local terminal went away — the thing this project exists
        // to survive. And LEAVING the arm in place instead is a silent spin: a
        // descriptor at end of file is readable for ever, so the loop would
        // wake on it as fast as the runtime can go, for the life of the
        // session, with a perfectly correct screen the whole time.
        //
        // Nothing is typed at all: the host is driven by hand and closes on
        // its own, so the session's whole life happens with the keyboard shut.
        //
        // Measured, by leaving the arm in place: this does not merely go over
        // the bar below, it HANGS. An always-ready `AsyncFd` arm never yields,
        // so on a current-thread runtime it starves the host task and the
        // timeout with it. Recorded because a regression here will look like a
        // stuck test rather than a failing one, and because it is why the bar
        // cannot be the only thing standing here — the `assert_eq!` on the
        // exit code is what fails cleanly if the arm ends the session instead.
        let (mut host, mut client) = pair("").await;
        let (keys, typing) = keyboard();
        drop(typing);

        let idle = Duration::from_secs(2);
        let host_loop = tokio::spawn(async move {
            let deadline = Instant::now() + idle;
            while Instant::now() < deadline {
                host.turn().expect("host turn");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            host.close(5);
            host
        });

        let before = thread_cpu_millis();
        let mut out = Vec::new();
        let code = tokio::time::timeout(idle * 4, client.run_on(keys, &mut out))
            .await
            .expect("a keyboard at end of file ended the session, or hung it")
            .expect("the client loop failed");
        let spent = thread_cpu_millis() - before;
        let _host = host_loop.await.expect("host task");

        assert_eq!(code, 5, "the session did not outlive the keyboard");
        assert!(
            spent < 400,
            "the loop burned {spent} ms of CPU across {} ms with the keyboard \
             shut: an arm was left watching a descriptor at end of file",
            idle.as_millis()
        );
    }

    fn application_close(code: u32, reason: &'static [u8]) -> quinn::ConnectionError {
        quinn::ConnectionError::ApplicationClosed(quinn::ApplicationClose {
            error_code: quinn::VarInt::from_u32(code),
            reason: reason.into(),
        })
    }

    #[test]
    fn only_the_hosts_own_close_is_an_exit_status() {
        // A host that was killed and a path that went away end the session
        // too. Reporting either as "the shell exited 0" would be a lie the
        // user acts on, so the status has to come from the shell or not at
        // all.
        assert_eq!(exit_code(&application_close(42, SHELL_EXITED)).unwrap(), 42);
        assert!(exit_code(&quinn::ConnectionError::TimedOut).is_err());
        assert!(exit_code(&quinn::ConnectionError::LocallyClosed).is_err());

        // And `ApplicationClosed` alone is not enough, which is the part that
        // was wrong. Every deliberate close in the system looks like this and
        // carries whatever error code its closer chose: a reattach superseding
        // an old attach, `accept_one` tearing down a second inbound
        // connection, a clean detach. Each one used to print
        // "exit 0" at a user whose shell is still running on the far end.
        for reason in [
            b"superseded by a newer attach".as_slice(),
            b"only one connection is served".as_slice(),
            b"detached".as_slice(),
            b"".as_slice(),
        ] {
            let got = exit_code(&application_close(0, reason));
            assert!(
                got.is_err(),
                "an application close reading {:?} was reported to the user as \
                 `exit 0`, so a live shell looks like a finished one",
                String::from_utf8_lossy(reason)
            );
        }
    }

    /// Does the CPU clock this file's two spin guards depend on actually
    /// measure CPU on THIS platform? Ported off `/proc`, and a guard whose
    /// instrument reads zero passes every time while proving nothing.
    #[test]
    fn the_cpu_clock_measures_work_and_not_wall_time() {
        let a = thread_cpu_millis();
        std::thread::sleep(Duration::from_millis(300));
        let slept = thread_cpu_millis() - a;

        let b = thread_cpu_millis();
        let start = Instant::now();
        let mut x: u64 = 0;
        while start.elapsed() < Duration::from_millis(300) {
            x = x.wrapping_add(1);
        }
        let spun = thread_cpu_millis() - b;

        assert!(x > 0, "the spin was optimised away");
        assert!(slept < 50, "sleeping cost {slept} ms of CPU");
        assert!(
            spun > 200,
            "spinning for 300 ms measured only {spun} ms of CPU"
        );
    }

    /// A DETACHED host session should be WAITING, not polling — this is the
    /// case the whole complaint was about: sessions nobody is attached to,
    /// burning CPU on a shared box.
    ///
    /// Attached is deliberately not what is measured. A host whose peer has
    /// stopped acking keeps retransmitting at the pacing rate, which is
    /// correct and is not idling; measuring that instead gave 195 wakes of
    /// real work and told us nothing about polling.
    #[tokio::test]
    async fn a_detached_host_session_waits_instead_of_polling() {
        let (mut host, client) = pair("").await;
        // Let the shell print its prompt, then take the peer away.
        for _ in 0..20 {
            host.turn().expect("host turn");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // The client goes away without a graceful shutdown, as a dropped
        // network does. A bare `drop` is not enough: with no idle timeout,
        // quinn would keep the connection alive indefinitely rather than
        // ever noticing on its own.
        client.link.sink.connection().close(0u32.into(), b"gone");
        drop(client);
        // quinn needs a moment to decide the connection is gone.
        let deadline = Instant::now() + Duration::from_secs(5);
        while host.link.sink.connection().close_reason().is_none() {
            host.turn().expect("host turn");
            tokio::time::sleep(Duration::from_millis(25)).await;
            assert!(Instant::now() < deadline, "the connection never closed");
        }

        let window = Duration::from_secs(2);
        let before = thread_cpu_millis();
        let _ = tokio::time::timeout(window, host.run()).await;
        let spent = thread_cpu_millis() - before;
        // Measured both ways rather than picked, by reinstating the
        // `IDLE_POLL` loop and rerunning: polling costs 24-27 ms across this
        // window — the 1.2% of a core the handoff recorded for a quiet
        // detached session — and waiting costs 0-1 ms. The bar sits in the
        // middle of that gap, near neither.
        assert!(
            spent < 10,
            "a detached host session burned {spent} ms of CPU across {} ms \
             doing nothing: it is polling, not waiting",
            window.as_millis()
        );
    }

    /// This THREAD's CPU time, in milliseconds.
    ///
    /// Per-thread and not per-process: the test binary runs several tests at
    /// once, and a process-wide figure would be measuring them instead.
    /// `#[tokio::test]` with no flavor is a current-thread runtime, so the
    /// loop under test and every task it spawns stay on this one thread.
    ///
    /// `CLOCK_THREAD_CPUTIME_ID` rather than `/proc/thread-self/stat`, which
    /// is what this used to read: the `/proc` version made both spin guards
    /// Linux-only, so the platform the CPU work was about to happen on had no
    /// spin guard at all. It is also finer — `/proc` quantises to USER_HZ,
    /// which is 10 ms, while this is nanoseconds.
    fn thread_cpu_millis() -> u64 {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
        t.tv_sec as u64 * 1_000 + t.tv_nsec as u64 / 1_000_000
    }

    #[tokio::test]
    async fn an_idle_loop_does_not_spin() {
        // THE test for the pacing deadline, and it has to measure CPU because
        // nothing about the SCREEN can see this bug. A loop whose wake-up is
        // derived from `due()` instead of from the clock finds its deadline
        // permanently in the past the moment both sides fall quiet — because
        // `offer_frame` only advances `last_send` when `make_frame` actually
        // produced a frame — and `sleep_until` then returns instantly, for
        // ever. The session stays perfectly correct and burns a whole core.
        let (mut host, mut client) = pair("").await;
        let (keys, _typing) = keyboard();

        // The host is driven by hand and slowly. `HostSession::run` polls at
        // 250 Hz and this test measures the thread both loops share, so the
        // host's own cadence must not be what is being weighed. It still acks,
        // which is the point: an unacked client always has something to send
        // and would never reach the quiet state this is about.
        let idle = Duration::from_secs(2);
        let host_loop = tokio::spawn(async move {
            let deadline = Instant::now() + idle;
            while Instant::now() < deadline {
                host.turn().expect("host turn");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            host.close(0);
            host
        });

        let before = thread_cpu_millis();
        let mut out = Vec::new();
        let code = tokio::time::timeout(idle * 4, client.run_on(keys, &mut out))
            .await
            .expect("the client loop never finished")
            .expect("the client loop failed");
        let spent = thread_cpu_millis() - before;
        let _host = host_loop.await.expect("host task");

        assert_eq!(code, 0);
        // A spinning loop spends the whole wall-clock window on a core; a
        // paced one spends a few milliseconds. The bar sits well below the
        // spin and well above the healthy figure, so it is a gap rather than a
        // threshold tuned to one observation.
        assert!(
            spent < 400,
            "the idle loop burned {spent} ms of CPU across {} ms of wall clock: \
             that is a spin, not a pace",
            idle.as_millis()
        );
    }

    /// Frames the receiver cannot apply used to go to stderr, which IS the
    /// terminal being painted: the message desynchronised the renderer's model
    /// and nothing repainted it on a quiet session. They are diagnostics about
    /// the link, so they belong in the link's own notice.
    #[tokio::test]
    async fn a_rejected_frame_is_counted_rather_than_printed() {
        let (_host, mut session) = pair("/bin/sh").await;
        let bad = Frame {
            my_state: 9,
            from_state: 7,
            ack_state: 0,
            flags: 0,
            payload: vec![0xff, 0xff, 0xff],
        };

        let mut out = Vec::new();
        let turn = session.turn_with(&[], Some(bad), &mut out).unwrap();

        assert_eq!(turn.rejected, 1);
        assert_eq!(
            session.rejected_total(),
            1,
            "the count did not reach the notice"
        );
    }

    #[tokio::test]
    async fn silence_raises_a_notice_and_a_frame_clears_it() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;

        // `t` is captured before `pair` performs a real QUIC handshake, so the
        // session's own `last_heard` — set in `ClientSession::new` — is
        // strictly later than `t` by however long that took. Saying we heard
        // from the host AT `t` moves the origin back onto the test's clock, so
        // the elapsed times below are exactly what they say and not a
        // handshake shorter.
        session.note_heard(t);
        session.note_sent(t);
        assert!(session.notice_at(t + Duration::from_secs(1)).is_none());

        let notice = session.notice_at(t + Duration::from_secs(3));
        assert!(notice.is_some(), "no notice after three seconds of silence");
        assert!(notice.unwrap().headline.contains("no reply"));

        session.note_heard(t + Duration::from_secs(4));
        assert!(session.notice_at(t + Duration::from_secs(4)).is_none());
    }

    #[tokio::test]
    async fn the_notice_names_the_counters_it_can_actually_observe() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        // The clock's origin, pinned to the test's: see the test above.
        session.note_heard(t);
        session.note_sent(t);
        // And the lap the owing begins on, which the grace period runs from.
        assert!(session.notice_at(t).is_none());

        let n = session.notice_at(t + Duration::from_secs(6)).unwrap();
        let shown = painted_words(&n);

        assert!(shown.contains("6s"), "no silence duration: {shown}");
        assert_claims_nothing_it_cannot_see(&shown);
    }

    /// A user typing into a dead screen cannot tell "kept" from "discarded"
    /// until the `Confirming` box appears -- and `Ctrl-\ q`, which the silence
    /// box does offer, ends the session before it ever does. Somebody who quit
    /// there would leave believing their typing had been thrown away.
    #[tokio::test]
    async fn the_silent_notice_says_that_blind_typing_is_being_kept() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        assert!(session.notice_at(t).is_none());

        let bare = session
            .notice_at(t + Duration::from_secs(3))
            .expect("no notice after three seconds of silence");
        session.shown = Some(bare.clone());
        assert!(
            !painted_words(&bare).contains("kept"),
            "the box talks about a buffer before anything was typed: {}",
            painted_words(&bare)
        );

        let mut out = Vec::new();
        session.route_keys(b"make test\r", &mut out).unwrap();

        let shown = painted_words(
            &session
                .notice_at(t + Duration::from_secs(5))
                .expect("the notice vanished"),
        );
        assert!(
            shown.contains("10 bytes"),
            "the box does not say how much is being kept: {shown}"
        );
        assert!(
            shown.contains("kept"),
            "someone typing blind is never told their keys are being kept: {shown}"
        );
        assert_claims_nothing_it_cannot_see(&shown);
    }

    /// And the cap is reported while it is still costing keystrokes, not
    /// afterwards in a box that reviews what survived. A limit someone is told
    /// about after the fact is a limit they could not have acted on.
    #[tokio::test]
    async fn a_full_buffer_is_reported_while_it_is_still_filling() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        assert!(session.notice_at(t).is_none());
        session.shown = session.notice_at(t + Duration::from_secs(3));
        assert!(session.shown.is_some());

        let mut out = Vec::new();
        session
            .route_keys(&vec![b'x'; crate::linkstate::MAX_HELD], &mut out)
            .unwrap();

        let shown = painted_words(
            &session
                .notice_at(t + Duration::from_secs(5))
                .expect("the notice vanished"),
        );
        assert!(
            shown.contains("full"),
            "the buffer stopped accepting keystrokes and the box did not say \
             so: {shown}"
        );
        assert_claims_nothing_it_cannot_see(&shown);
    }

    /// During an outage the client keeps retransmitting, so `sent_packets`
    /// climbs at the pacing rate -- as often as 125 times a second. Rebuilding
    /// the box on each change costs two `Paragraph` renders, a clone of the
    /// whole cell grid, a diff and a flush every time, and puts a number in
    /// front of the user that churns far too fast to read, inside a box whose
    /// entire job is to be read.
    #[tokio::test]
    async fn the_silence_counters_are_rebuilt_at_most_once_a_second() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        assert!(session.notice_at(t).is_none());

        let first = session
            .notice_at(t + Duration::from_secs(3))
            .expect("no notice after three seconds of silence");
        session.shown = Some(first.clone());

        // Something the box reports moves. In a live outage this is the
        // retransmit counters; here it is a number a test can set.
        session.rejected_total = 1;

        assert_eq!(
            session.notice_at(t + Duration::from_millis(3_400)),
            Some(first.clone()),
            "the box was rebuilt 400 ms after the last one"
        );
        let later = session
            .notice_at(t + Duration::from_millis(4_100))
            .expect("the notice vanished instead of refreshing");
        assert_ne!(later, first, "the counters never refreshed at all");
        assert!(
            painted_words(&later).contains("rejected: 1"),
            "{}",
            painted_words(&later)
        );
    }

    /// Only the refresh is paced. A change of phase is the thing the box
    /// exists to announce, and waiting up to a second to announce it would
    /// leave the user typing into a screen whose box is a second out of date.
    #[tokio::test]
    async fn a_change_of_phase_repaints_at_once_however_recent_the_refresh() {
        let (_host, mut session) = with_notice().await;
        let mut out = Vec::new();
        session.route_keys(b"make test\r", &mut out).unwrap();

        // A hundred milliseconds after the silence box was last built, the
        // host answers.
        let now = std::time::Instant::now();
        session.note_heard(now);
        let n = session
            .notice_at(now)
            .expect("the question about the held input never appeared");

        assert!(
            n.headline.contains("answering again"),
            "the box still reports the outage the host has already ended: {}",
            n.headline
        );
    }

    /// And the transition out of a notice altogether is just as immediate:
    /// a healthy session must not keep a stale box for up to a second.
    #[tokio::test]
    async fn returning_to_live_clears_the_box_at_once() {
        let (_host, mut session) = with_notice().await;

        let now = std::time::Instant::now();
        session.note_heard(now);

        assert_eq!(
            session.notice_at(now),
            None,
            "a box outlived the silence it was reporting"
        );
    }

    /// The other notice, and the one that went unguarded: the check above
    /// reads only the `Silent` box, which is how "reconnected" survived in a
    /// phase where nothing reconnects.
    ///
    /// Reached without sleeping, along the path the loop actually takes: a
    /// notice is up, something is typed into it, and then the host answers.
    #[tokio::test]
    async fn the_confirming_notice_states_only_what_the_client_can_observe() {
        let (_host, mut session) = with_notice().await;
        let mut out = Vec::new();
        session.route_keys(b"make test\r", &mut out).unwrap();

        // A frame arrives. That the host is answering again is the whole of
        // what the client learns from it -- not that anything reconnected,
        // because the connection never dropped, and not that the shell is
        // well, which no frame can say.
        let now = std::time::Instant::now();
        session.note_heard(now);

        let n = session
            .notice_at(now)
            .expect("the host answered with input held, and nothing was asked");
        let shown = painted_words(&n);

        assert!(
            shown.contains("10 bytes"),
            "this is not the notice that asks about the held input: {shown}"
        );
        assert_claims_nothing_it_cannot_see(&shown);
    }

    /// Every word a notice puts on the screen: headline, body and key list.
    /// A guard that reads only the body is how a claim came to sit in a key
    /// list unnoticed, and reading only the `Silent` notice is how another
    /// came to sit in a headline.
    fn painted_words(n: &Notice) -> String {
        std::iter::once(n.headline.clone())
            .chain(n.body.iter().cloned())
            .chain(n.keys.iter().flat_map(|(k, d)| [k.clone(), d.clone()]))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// What phase 1 forbids layer 1 to say, wherever in the box it says it.
    ///
    /// Nothing reconnects yet, so the notice may not name a mechanism oxutrm
    /// does not have. And from here a dead network and a crashed host are
    /// indistinguishable, so it may not vouch for the far end at all -- not
    /// even hedged. The hedge belongs in the exit message, which is read once
    /// the session has ended and can point at `oxutrm host --list`; the box
    /// has room to say what a key DOES, and that stays true either way.
    fn assert_claims_nothing_it_cannot_see(shown: &str) {
        let lower = shown.to_lowercase();
        assert!(
            !lower.contains("safe"),
            "claimed the session is safe, which the client cannot know: {shown}"
        );
        assert!(
            !lower.contains("retry") && !lower.contains("reconnect"),
            "phase 1 promised a reconnection that does not exist: {shown}"
        );
        for claim in ["keeps running", "still running", "is running"] {
            assert!(
                !lower.contains(claim),
                "asserted the shell's state, which the client cannot see: {shown}"
            );
        }
    }

    /// Ctrl-\, the prefix layer 1 listens for. `linkstate`'s own copy is
    /// private to that module, and a test that reached for it would be
    /// asserting the constant rather than the keystroke.
    const CTRL_BACKSLASH: u8 = 0x1c;

    /// A client with a real `Silent` notice showing, left exactly as the loop
    /// leaves it: the phase decided by `notice_at`, and `shown` mirroring what
    /// the overlay is.
    async fn with_notice() -> (HostSession, ClientSession) {
        let t = std::time::Instant::now();
        let (host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        // The lap the owing begins on, and it is not decoration: the grace
        // period is measured from when the reply started being owed, so a
        // fixture that jumped straight to three seconds would be asking about
        // an owing three seconds long that had only just started.
        assert!(session.notice_at(t).is_none());
        let notice = session.notice_at(t + Duration::from_secs(3));
        assert!(notice.is_some(), "the fixture raised no notice");
        session.shown = notice;
        (host, session)
    }

    /// A client sitting in the `Confirming` box with `held` typed blind: a
    /// notice went up, the user typed into it, and the host started answering
    /// again. The only phase that offers `Ctrl-\ s` and `Ctrl-\ d`.
    async fn with_confirming_notice(held: &[u8]) -> (HostSession, ClientSession) {
        let (host, mut session) = with_notice().await;
        let mut out = Vec::new();
        session.route_keys(held, &mut out).expect("hold the typing");

        let now = std::time::Instant::now();
        session.note_heard(now);
        let notice = session.notice_at(now);
        assert!(notice.is_some(), "the fixture asked the user nothing");
        session.shown = notice;
        (host, session)
    }

    /// Wait until a frame is sitting in the client's source, without applying
    /// it. `try_recv` in the code under test is what must pick it up.
    ///
    /// `FrameSource` has no `has_frame` (or equivalent peek) to poll, so this
    /// falls back to sleeping briefly and trusting `try_recv` inside `turn` to
    /// find what arrived. That is a timing proxy, not a direct wait, and this
    /// project has recorded that a timing proxy becomes a race when the thing
    /// it proxied moves.
    async fn wait_for_frame(_session: &mut ClientSession) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// A frame that arrives on a pacing lap rather than through the frame arm
    /// still counts as hearing from the host. Without this the picture comes
    /// back to life underneath a box saying nobody is answering.
    #[tokio::test]
    async fn a_scavenged_frame_clears_the_notice() {
        let (mut host, mut session) = with_notice().await;
        let mut out = Vec::new();

        // The host answers. The frame lands in the channel, but nothing wakes
        // the loop's frame arm -- this is the pacing lap that scavenges it.
        host.turn().expect("the host takes a turn");
        wait_for_frame(&mut session).await;
        session.turn(&[], &mut out).expect("a pacing lap");

        assert!(
            matches!(session.link_state.phase_now(), Phase::Live),
            "a frame was applied and the client still believes the host is silent: {:?}",
            session.link_state.phase_now()
        );
    }

    /// The counter is built from `last_heard`, so a scavenged frame must move
    /// it or the box overstates the outage for as long as the box is up.
    #[tokio::test]
    async fn a_scavenged_frame_takes_the_notice_down() {
        let (mut host, mut session) = with_notice().await;
        let mut out = Vec::new();

        host.turn().expect("the host takes a turn");
        wait_for_frame(&mut session).await;
        session.turn(&[], &mut out).expect("a pacing lap");

        assert!(
            session.notice_at(Instant::now()).is_none(),
            "the notice survived a frame that was applied"
        );
    }

    /// `heard` clears a half-typed prefix, and this task makes `heard` run far
    /// more often. A `Ctrl-\` and its letter genuinely arrive in two reads;
    /// a frame landing between them must not eat the command.
    #[tokio::test]
    async fn a_frame_between_the_prefix_and_its_letter_does_not_eat_the_command() {
        let (mut host, mut session) = with_confirming_notice(b"echo hi\r").await;
        let mut out = Vec::new();

        // The prefix arrives at the end of one read...
        session
            .route_keys(&[CTRL_BACKSLASH], &mut out)
            .expect("the prefix is held");
        // ...a frame is applied between the two reads...
        host.turn().expect("the host takes a turn");
        wait_for_frame(&mut session).await;
        session.turn(&[], &mut out).expect("a pacing lap");
        // ...and the letter arrives in the next.
        session
            .route_keys(b"d", &mut out)
            .expect("the letter lands");

        assert!(
            session.link_state.held().is_empty(),
            "Ctrl-\\ d did not drop the held buffer: the frame ate the prefix"
        );
    }

    /// Everything the client has said to the host this session. Input is
    /// cumulative -- the host tracks how much of it has been written to the
    /// shell -- so this is where a keystroke lands if it was passed through.
    fn spoken(session: &ClientSession) -> Vec<u8> {
        session.input_tx.current().pending.clone()
    }

    /// The healthy path, and the one a regression would break silently: with
    /// nothing showing, every byte belongs to the host and nothing is held.
    #[tokio::test]
    async fn keys_reach_the_host_untouched_while_nothing_is_showing() {
        let (_host, mut session) = pair("/bin/sh").await;
        let mut out = Vec::new();

        assert_eq!(session.route_keys(b"ls -l\r", &mut out).unwrap(), None);

        assert!(
            spoken(&session).ends_with(b"ls -l\r"),
            "typing did not reach the host: {:?}",
            spoken(&session)
        );
        assert!(
            session.link_state.held().is_empty(),
            "typing was held while the session was healthy"
        );
    }

    /// The payload of the phase: what is typed at a dead link is kept rather
    /// than thrown at a host that cannot hear it.
    #[tokio::test]
    async fn typing_into_a_notice_is_held_and_not_sent() {
        let (_host, mut session) = with_notice().await;
        let before = spoken(&session);
        let mut out = Vec::new();

        assert_eq!(session.route_keys(b"rm -rf /", &mut out).unwrap(), None);

        assert_eq!(
            session.link_state.held(),
            b"rm -rf /",
            "blind typing was not kept"
        );
        assert_eq!(
            spoken(&session),
            before,
            "blind typing was sent at a host that is not answering"
        );
    }

    /// `Ctrl-\ q` is the notice's own key, so it must not reach the shell as
    /// two stray bytes, and it must end the client with a status of its own
    /// rather than one invented for a shell that never exited.
    #[tokio::test]
    async fn the_quit_key_ends_the_client_with_a_status_of_zero() {
        let (_host, mut session) = with_notice().await;
        let before = spoken(&session);
        let mut out = Vec::new();

        let answer = session
            .route_keys(&[CTRL_BACKSLASH, b'q'], &mut out)
            .unwrap();

        assert_eq!(answer, Some(0), "the quit key did not end the client");
        assert_eq!(spoken(&session), before, "the command reached the shell");
    }

    /// The other half of holding it: giving it back, in one piece and in
    /// order, once the user has looked at the screen and said so.
    #[tokio::test]
    async fn the_send_key_delivers_what_was_typed_blind() {
        let (_host, mut session) = with_confirming_notice(b"make test\r").await;
        let mut out = Vec::new();

        let answer = session
            .route_keys(&[CTRL_BACKSLASH, b's'], &mut out)
            .unwrap();

        assert_eq!(answer, None, "sending the held input ended the session");
        assert!(
            spoken(&session).ends_with(b"make test\r"),
            "the held input was not delivered: {:?}",
            spoken(&session)
        );
        assert!(
            session.link_state.held().is_empty(),
            "the held input was delivered and kept, so it can arrive twice"
        );
    }

    /// And dropping it must actually drop it: a `d` that left the buffer full
    /// would deliver the discarded keys at the next `s`.
    #[tokio::test]
    async fn the_drop_key_throws_the_blind_typing_away() {
        let (_host, mut session) = with_confirming_notice(b"make test\r").await;
        let mut out = Vec::new();
        let before = spoken(&session);

        let answer = session
            .route_keys(&[CTRL_BACKSLASH, b'd'], &mut out)
            .unwrap();

        assert_eq!(answer, None, "dropping the held input ended the session");
        assert!(session.link_state.held().is_empty(), "the drop kept it");
        assert_eq!(spoken(&session), before, "the drop sent it instead");
    }

    /// The `Silent` box lists exactly one key, and the two it does not list
    /// must not work.
    ///
    /// `Ctrl-\ s` there would throw the held bytes at a link the client has
    /// just told the user is not answering -- and empty the buffer, so the
    /// `Confirming` review that is the entire point of holding never happens.
    /// `Ctrl-\ d` would discard someone's typing with no confirmation at all.
    /// Both are kept as typing instead, which is what the user meant by
    /// pressing keys into a box that does not offer them.
    #[tokio::test]
    async fn the_silent_notice_does_not_honour_the_keys_it_does_not_offer() {
        let (_host, mut session) = with_notice().await;
        let mut out = Vec::new();
        session.route_keys(b"make test\r", &mut out).unwrap();
        let before = spoken(&session);

        assert_eq!(
            session
                .route_keys(&[CTRL_BACKSLASH, b's'], &mut out)
                .unwrap(),
            None
        );
        assert_eq!(
            spoken(&session),
            before,
            "the held input was delivered to a host the box says is not answering"
        );

        assert_eq!(
            session
                .route_keys(&[CTRL_BACKSLASH, b'd'], &mut out)
                .unwrap(),
            None
        );
        assert!(
            session.link_state.held().starts_with(b"make test\r"),
            "someone's blind typing was discarded by a key the box never \
             offered: {:?}",
            session.link_state.held()
        );
    }

    /// And `Ctrl-\ q` is the key every box does offer, in every phase.
    #[tokio::test]
    async fn the_quit_key_works_under_the_confirming_notice_too() {
        let (_host, mut session) = with_confirming_notice(b"make test\r").await;
        let mut out = Vec::new();

        assert_eq!(
            session
                .route_keys(&[CTRL_BACKSLASH, b'q'], &mut out)
                .unwrap(),
            Some(0),
        );
    }

    /// Without a heartbeat an idle session cannot tell an outage from calm,
    /// and the user finds out by typing into a screen that died ten minutes
    /// ago. With one, a reply is owed and the silence becomes visible.
    ///
    /// The clock is a parameter, so this asks the question at five seconds
    /// without waiting five seconds.
    #[tokio::test]
    async fn an_idle_session_prods_the_host_after_five_quiet_seconds() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        let before = session.input_tx.current().seq();

        assert!(
            !session.heartbeat(t + Duration::from_secs(4)),
            "prodded a session that had only been quiet for four seconds"
        );
        assert_eq!(session.input_tx.current().seq(), before);

        assert!(session.heartbeat(t + Duration::from_secs(5)));
        assert_eq!(
            session.input_tx.current().seq(),
            before + 1,
            "the heartbeat did not move the sequence, so the host owes no reply \
             and the silence stays invisible"
        );
        assert!(
            session.last_send.is_none(),
            "the heartbeat waits for the pacing interval it exists to pre-empt"
        );

        // And not again until another five quiet seconds have passed: the
        // heartbeat is 0.2 Hz, not a poll.
        assert!(!session.heartbeat(t + Duration::from_secs(6)));
    }

    /// The caller's half of `notice_at`'s question: have we said something the
    /// host has not acknowledged? Reading it lets a test wait for a REAL ack
    /// rather than assume one, which is the difference between exercising the
    /// clock and exercising a fixture.
    fn reply_owed(c: &ClientSession) -> bool {
        c.input_tx.current().seq() != c.screen_rx.peer_ack()
    }

    /// A session that heartbeats and is answered never leaves `Live`.
    ///
    /// The composed defect, and the one no per-task test could see because
    /// each of them holds the clock still around a single transition. The
    /// heartbeat bumps the sequence every `HEARTBEAT_IDLE` (5 s), so a reply is
    /// owed from that instant; if the grace period is measured from the last
    /// thing we HEARD rather than from when the owing began, then five seconds
    /// of perfectly healthy calm are already past `SILENT_AFTER` (2 s) and the
    /// very next lap paints "no reply from host". Every idle session, every
    /// five seconds, for ever -- and while it is up, `route_keys` diverts the
    /// keyboard into the held buffer.
    ///
    /// Two full cycles, with the host really answering in between, and the
    /// clock supplied rather than slept through.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_heartbeating_session_that_is_answered_never_raises_a_notice() {
        let (mut host, mut client) = pair("/bin/sh").await;
        let mut out = Vec::new();

        // Settle first: the fake clock below is only honest if it starts from
        // a state where the host has acked everything the client has said.
        assert!(
            drive(
                &mut host,
                &mut client,
                &mut out,
                Duration::from_secs(20),
                |_, c| !reply_owed(c)
            )
            .await,
            "the host never acked the client, so nothing below is about the clock"
        );

        let mut now = std::time::Instant::now();
        client.note_heard(now);
        client.note_sent(now);

        for cycle in 1..=2u32 {
            now += crate::linkstate::HEARTBEAT_IDLE;
            assert!(
                client.heartbeat(now),
                "cycle {cycle}: no heartbeat was due after five quiet seconds"
            );
            assert_eq!(
                client.notice_at(now),
                None,
                "cycle {cycle}: a notice was raised on the very lap the heartbeat \
                 went out, for a reply owed for zero milliseconds. Every idle \
                 session would flash this every {} seconds",
                crate::linkstate::HEARTBEAT_IDLE.as_secs()
            );

            // The host answers, as a healthy one does.
            assert!(
                drive(
                    &mut host,
                    &mut client,
                    &mut out,
                    Duration::from_secs(20),
                    |_, c| !reply_owed(c)
                )
                .await,
                "cycle {cycle}: the heartbeat was never answered"
            );
            now += Duration::from_millis(120);
            client.note_heard(now);
            assert_eq!(
                client.notice_at(now),
                None,
                "cycle {cycle}: a notice survived the host answering"
            );
        }
    }

    /// A healthy session, through the REAL loop, for longer than the
    /// heartbeat: no notice may ever be painted.
    ///
    /// Both idle-CPU guards run for two seconds, which is below
    /// `HEARTBEAT_IDLE`, so what the heartbeat does *inside* the loop was
    /// entirely unpinned -- and nothing anywhere asserted the composed
    /// property that a working session shows nothing at all. That is how C1
    /// shipped: every one of its parts passed its own review.
    ///
    /// The assertion reads the bytes that went to the terminal, because that
    /// is what the user sees. The headline is one uniformly styled span, so if
    /// it is ever painted its bytes appear in `out` contiguously.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_healthy_session_paints_no_notice_across_several_heartbeats() {
        let (mut host, mut client) = pair("").await;
        let (keys, mut typing) = keyboard();
        let host_loop = tokio::spawn(async move { host.run().await });

        // Longer than two heartbeats, and quiet throughout: the client says
        // nothing, the shell prints nothing, and the only traffic is the
        // heartbeat and the host's answer to it.
        typing.write_all(b"sleep 12\nexit 0\n").expect("type");

        let mut out = Vec::new();
        let code = tokio::time::timeout(Duration::from_secs(60), client.run_on(keys, &mut out))
            .await
            .expect("the client loop never finished")
            .expect("the client loop failed");
        let _ = host_loop.await;

        assert_eq!(code, 0);
        let painted = String::from_utf8_lossy(&out);
        assert!(
            !painted.contains("reply from host"),
            "a healthy session told the user the host had stopped answering"
        );
        assert!(
            client.shown.is_none(),
            "the session ended with a notice still up"
        );
    }

    /// The loop rebuilds layer 1 only when the notice's CONTENT changes, and a
    /// resize changes not one word of it. So the resize itself has to lay the
    /// box out again, or a `Confirming` notice — the one the user sits and
    /// reads, because it is asking them a question — keeps the geometry of a
    /// screen that is gone until they press a key.
    #[tokio::test]
    async fn a_resize_lays_the_notice_out_again_for_the_new_screen() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);

        // Raise a notice and paint it, exactly as `run_on` does -- including
        // the earlier lap on which the reply started being owed.
        assert!(session.notice_at(t).is_none());
        let notice = session.notice_at(t + Duration::from_secs(3)).unwrap();
        session
            .renderer
            .set_overlay(Some(layout_notice(&notice, session.size)));
        session.shown = Some(notice.clone());
        let mut painted = Vec::new();
        session
            .renderer
            .render(&mut painted, session.screen_rx.state())
            .unwrap();

        let small = TermSize { cols: 24, rows: 8 };
        session.resize(small);
        let mut after = Vec::new();
        session
            .renderer
            .render(&mut after, session.screen_rx.state())
            .unwrap();

        // What a renderer that never saw the old screen paints, for the same
        // content and the same state. Equality is the assertion: the box is
        // where the NEW screen puts it, and not where the old one did.
        let mut fresh = Renderer::new(small, caps());
        fresh.set_overlay(Some(layout_notice(&notice, small)));
        let mut expected = Vec::new();
        fresh
            .render(&mut expected, session.screen_rx.state())
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&after),
            String::from_utf8_lossy(&expected),
            "the notice kept the geometry of the screen that went away"
        );
    }

    /// The rule from spec 4.2, and the one that costs something to get wrong:
    /// a rebind moves our source port and invalidates a punched NAT hole, so
    /// doing it to a link that is working breaks the path in order to test it.
    #[tokio::test]
    async fn a_healthy_session_never_probes_the_route() {
        let (_host, mut session) = pair("/bin/sh").await;
        let t = Instant::now();
        session.note_heard(t);

        assert!(
            !session.follow_route(t),
            "probed a healthy link; a rebind on a working path breaks it"
        );
        assert!(
            session.probed_at.is_none(),
            "a healthy link cost a probe syscall"
        );
        assert!(
            !session.follow_route(t + ROUTE_PROBE_EVERY * 5),
            "probed a healthy link after several intervals"
        );
        assert!(
            session.probed_at.is_none(),
            "a healthy link cost a probe syscall after several intervals"
        );
    }

    /// Probing is gated even inside `Silent`: the loop wakes every 8-100ms and
    /// a bind/connect pair on every lap is up to 125 a second.
    #[tokio::test]
    async fn probing_is_paced_while_silent() {
        let (_host, mut session) = with_notice().await;
        let t = Instant::now();

        session.follow_route(t);
        let after_first = session.probed_at;
        assert!(
            after_first.is_some(),
            "the first probe in Silent did not run"
        );

        session.follow_route(t + ROUTE_PROBE_EVERY / 2);
        assert_eq!(
            session.probed_at, after_first,
            "probed twice inside one ROUTE_PROBE_EVERY"
        );

        session.follow_route(t + ROUTE_PROBE_EVERY * 2);
        assert_ne!(
            session.probed_at, after_first,
            "the probe never resumed after its interval"
        );
    }

    /// Silence is not evidence that the route moved. A host can go quiet with
    /// this machine's address exactly where it was -- a crash, a wedged
    /// process, congestion -- and a rebind costs a punched NAT hole, so it is
    /// spent only on a reading that actually disagrees with the baseline.
    ///
    /// The fixture connects over loopback and stays there, so the probe agrees
    /// with the baseline `ClientSession::new` seeded. `probed_at` is asserted
    /// too: without it this passes just as well if a gate returns before
    /// probing at all, which would make it a guard that cannot fail.
    #[tokio::test]
    async fn a_probe_that_finds_the_route_unchanged_does_not_rebind() {
        let (_host, mut session) = with_notice().await;
        let t = Instant::now();
        let before = session.link.socket.local_addr().expect("a bound socket");

        assert!(
            !session.follow_route(t),
            "rebound on a route that had not moved"
        );
        assert!(
            session.probed_at.is_some(),
            "the probe never ran, so this asserts nothing about rebinding"
        );
        assert_eq!(
            session.link.socket.local_addr().expect("a bound socket"),
            before,
            "the session socket was swapped though the route was unchanged"
        );
    }

    /// The baseline is taken when the connection comes up, not on the first
    /// probe of the outage. `SILENT_AFTER` is two seconds, so walking out of
    /// Wi-Fi range moves the route BEFORE the silence is noticed: a baseline
    /// first read inside `Silent` would read the address already moved to,
    /// agree with it for ever, and never rebind -- inert for the one case this
    /// whole phase exists for.
    #[tokio::test]
    async fn the_baseline_is_taken_when_the_connection_comes_up() {
        let (_host, session) = pair("/bin/sh").await;
        let elsewhere: std::net::IpAddr = "10.46.18.101".parse().expect("a literal address");

        // A baseline is what makes a different address readable as a move.
        // With none, `moved` is false for everything and no outage can ever
        // reach the rebind.
        assert!(
            session.route.moved(elsewhere),
            "a fresh session had no baseline; the first probe of an outage \
             would adopt whatever it found and never rebind"
        );
        // And it was not taken by the loop: this is one reading at startup,
        // not probing on `Live` laps.
        assert!(
            session.probed_at.is_none(),
            "the seeding probe went through the loop's paced path"
        );
    }

    /// The composed test for phase 2, and the reason there is one: phase 1's
    /// Critical bug lived across a seam no single task owned, and every
    /// per-task test holds the clock still. This one lets the real loop run.
    ///
    /// It asserts the phase's whole user-visible claim in one place: silence
    /// raises a box, the session does NOT die under it -- which is the entire
    /// change -- and the box comes down by itself when the host speaks again.
    /// Before phase 2 the client exited at ~33 s with an error instead.
    ///
    /// Real elapsed time, not an injected clock: the thing under test is a
    /// real quinn connection's own idle-timeout machinery, which an injected
    /// clock cannot reach. That makes it too slow for the default suite, so
    /// it is `#[ignore]`d. Run it explicitly with:
    ///
    /// ```text
    /// cargo test -j4 --bin oxutrm outlives_a_silence -- --nocapture --ignored --test-threads=1
    /// ```
    ///
    /// Do not shorten `outage` below 31 s: the whole assertion is that the
    /// session outlives the 30 s `max_idle_timeout` that used to kill it, and
    /// anything under that would prove nothing about the timeout either way.
    ///
    /// **`close_reason().is_none()` below is a sanity check, not a guard
    /// against the timeout regression.** Restoring `max_idle_timeout(Some(30s))`
    /// in `crates/oxutrm-net/src/quic.rs` and rerunning this test does NOT
    /// make it fail -- verified, not assumed; see the task report. The client
    /// keeps resending its unacknowledged input every pacing interval
    /// throughout the "silence" (the host session never acks it), and
    /// quinn's transport ACKs those packets, and answers keep-alives,
    /// entirely at the connection's own background task -- work that runs
    /// whether or not `HostSession::turn_at` is ever called. Two live,
    /// unsuspended processes on loopback can therefore never reproduce the
    /// failure this phase fixes, which was a HOST PROCESS suspended by
    /// `SIGSTOP` and therefore unable to run that background task at all.
    /// Only a real process being stopped -- the hand test -- exercises that.
    /// The direct, mutation-sensitive guard on the config itself is
    /// `the_transport_imposes_no_idle_timeout` in
    /// `crates/oxutrm-net/src/quic.rs`. What THIS test's timing does prove,
    /// and what failed before `notice_at` was moved inside the loop below: a
    /// silence long enough to have been fatal raises the notice and the
    /// notice comes down again on its own once the host answers.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "real 35s wall-clock outage; run explicitly, see doc comment"]
    async fn a_session_outlives_a_silence_that_used_to_kill_it() {
        let (mut host, mut client) = pair("/bin/sh").await;
        let mut out = Vec::new();
        let t = Instant::now();

        client.note_heard(t);
        client.note_sent(t);

        // Long enough to have been fatal: the old max_idle_timeout was 30s and
        // the client died at ~33s in the hand test. Real elapsed time, because
        // the thing under test is a real quinn connection's own timers, and an
        // injected clock cannot reach those.
        let outage = Duration::from_secs(35);
        let deadline = tokio::time::Instant::now() + outage;
        while tokio::time::Instant::now() < deadline {
            // The loop's own laps, with the host saying nothing at all.
            client
                .turn(&[], &mut out)
                .expect("the session survives the lap");
            // `run`'s own loop calls `notice_at` once per lap (see the
            // `Wake::Due` arm above) -- that is what advances `LinkState`'s
            // grace-period clock. `evaluate` is edge-triggered on being
            // asked, not on wall-clock time passing underneath it, so a loop
            // that never asks would still see `Live` on its first question
            // 35s in and this composed test would prove nothing.
            let _ = client.notice_at(Instant::now());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(
            client.notice_at(Instant::now()).is_some(),
            "no notice after {outage:?} of silence"
        );
        // Sanity, not the regression guard -- see the doc comment above for
        // why this cannot be made to fail by restoring the old timeout here.
        assert!(
            client.link.sink.connection().close_reason().is_none(),
            "the connection died under the notice: {:?}",
            client.link.sink.connection().close_reason()
        );

        // The host comes back. The notice must come down on its own -- through
        // the scavenging path Task 1 fixed, since nothing here wakes a frame arm.
        host.turn().expect("the host answers at last");
        tokio::time::sleep(Duration::from_millis(200)).await;
        client
            .turn(&[], &mut out)
            .expect("a lap that scavenges the frame");

        assert!(
            client.notice_at(Instant::now()).is_none(),
            "the host answered and the notice stayed up"
        );
    }

    /// A short, **non-`#[ignore]`d** sibling of the test above, so CI runs
    /// the real-clock half of the composed-test story on every default
    /// `cargo test`, not only when someone remembers `--ignored`.
    ///
    /// Every other notice test in this file injects instants and holds the
    /// clock still. This one and the 35s test above are the only two that
    /// let `LinkState::evaluate`'s edge-triggered `owed_since` and the
    /// scavenging clear path run against a real clock. This one starts from
    /// a genuinely SYNCED session (the host acks the client's first input
    /// before the silence begins), so the notice's later rise is driven by
    /// `heartbeat_due` firing at `HEARTBEAT_IDLE` and then `SILENT_AFTER`
    /// more before that owing is old enough to report -- roughly 7s in the
    /// worst case -- rather than by the from-construction seq mismatch every
    /// fresh `ClientSession` starts with, which is what the 35s test above
    /// relies on instead (silence from the very first lap of the session,
    /// a different and equally real case, but not one that exercises
    /// `heartbeat_due` at all). 9s of real polling budgets both real timers
    /// with margin for a loaded runner.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_short_silence_raises_and_clears_the_notice_on_a_real_clock() {
        let (mut host, mut client) = pair("/bin/sh").await;
        let mut out = Vec::new();

        // Get to a genuinely synced state before the silence starts: the
        // client speaks once, the host applies it and answers, and the
        // client applies that answer -- so `input_tx.current().seq() ==
        // screen_rx.peer_ack()` and nothing is owed, exactly as a healthy
        // attach looks. Without this the notice would rise from the same
        // from-construction mismatch the 35s test above already covers, and
        // this test would prove nothing extra about `HEARTBEAT_IDLE`.
        client.turn(&[], &mut out).expect("the client's first lap");
        let frame = client_frame(&mut host).await;
        host.turn_with(Some(frame))
            .expect("the host answers the first lap");
        let reply = tokio::time::timeout(Duration::from_secs(5), client.link.source.recv())
            .await
            .expect("the host's first reply never arrived")
            .expect("the source closed");
        client
            .turn_with(&[], Some(reply), &mut out)
            .expect("the client applies the host's first ack");
        assert!(
            client.notice_at(Instant::now()).is_none(),
            "not synced before the silence began; this test would prove \
             nothing about HEARTBEAT_IDLE"
        );

        let t = Instant::now();
        client.note_heard(t);
        client.note_sent(t);

        // HEARTBEAT_IDLE (5s) until the client's own heartbeat makes a reply
        // owed, plus SILENT_AFTER (2s) more before that owing is old enough
        // to raise the notice -- 9s of real polling budgets both with
        // margin.
        let outage = Duration::from_secs(9);
        let deadline = tokio::time::Instant::now() + outage;
        while tokio::time::Instant::now() < deadline {
            client
                .turn(&[], &mut out)
                .expect("the session survives the lap");
            // `run`'s own loop calls both of these every lap -- see its
            // `Wake::Due` arm. The heartbeat is what actually generates the
            // owed reply this time, since (unlike the 35s test) this one
            // starts synced.
            let _ = client.heartbeat(Instant::now());
            let _ = client.notice_at(Instant::now());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(
            client.notice_at(Instant::now()).is_some(),
            "no notice after {outage:?}, well past HEARTBEAT_IDLE + SILENT_AFTER"
        );

        // The host answers again; the notice must clear through the same
        // scavenging path Task 1 fixed.
        host.turn().expect("the host answers at last");
        tokio::time::sleep(Duration::from_millis(200)).await;
        client
            .turn(&[], &mut out)
            .expect("a lap that scavenges the frame");

        assert!(
            client.notice_at(Instant::now()).is_none(),
            "the host answered and the notice stayed up"
        );
    }
}

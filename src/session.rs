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

// The client half of this module runs while it owns the screen: nothing on
// that path may print, or it lands raw on the painted raw-mode terminal.
// The one deliberate exception, on the host's own stderr, is
// `#[expect]`-ed at its call site.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result};

use oxutrm_client::{PopupView, Renderer, layout_popup, status_line, terminal_size_of};
use oxutrm_proto::{Frame, PathDescription, ScreenState, TermSize, TerminalCaps};
use oxutrm_sync::{InputState, Receiver, Sender, SyncState as _};
use oxutrm_term::HostTerm;

use crate::activity::{Activity, Kind};
use crate::link::{Link, SendOutcome};
use crate::linkstate::{LinkState, Phase};
use crate::quality::{Quality, Reading};
use crate::rebuild::{AttemptOutcome, Rebuild};
use crate::ui::{Command, LinkChange, Mode, Ui};
use crate::view::{Facts, Identity, StandbyFacts};

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
                    #[cfg_attr(
                        not(test),
                        expect(
                            clippy::print_stderr,
                            reason = "the host daemon's own stderr, from turn_at on \
                                      HostSession; never a client's screen"
                        )
                    )]
                    {
                        eprintln!("oxutrm: host dropped an unapplicable input frame: {e}");
                    }
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
    /// A thin wrapper over [`HostSession::run_with_attaches`], with a receiver
    /// whose sender has already been dropped. `Some(a) = attaches.recv()`
    /// makes a closed receiver disable that arm rather than make it hot — see
    /// `run_with_attaches`' own note — so this costs nothing and every
    /// existing caller and test is unchanged.
    pub async fn run(&mut self) -> Result<i32> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        // Dropped, not merely unnamed. `let (_tx, ...)` binds the sender for
        // the whole `await`, which leaves an open-and-empty channel whose
        // `recv()` pends for ever — the behaviour is the same, but it is not
        // the mechanism the sentence above describes, and it means the arm
        // that `Some(a) = ...` is there to disable is never actually
        // disabled by anything `run` does.
        drop(tx);
        self.run_with_attaches(&mut rx).await
    }

    /// [`HostSession::run`], plus a second inbound connection completing an
    /// attach exchange elsewhere in the process. Carries the whole
    /// [`crate::attach_exchange::Attached`] rather than just the [`Link`],
    /// because [`HostSession::adopt`] needs the size and
    /// [`HostSession::on_attached`] the role: a primary is adopted at once, a
    /// standby is parked until the client's first frame arrives on it.
    ///
    /// The descriptors are duplicated out of the terminal before the loop so
    /// the arms borrow locals rather than `self`, which is what lets the body
    /// call `&mut self` methods afterwards (C1). A `dup` shares the file
    /// description, harmless here in a way it is NOT for the client's
    /// keyboard: this description is ours and we set its `O_NONBLOCK`
    /// ourselves in `Pty::spawn`.
    pub async fn run_with_attaches(
        &mut self,
        attaches: &mut tokio::sync::mpsc::Receiver<crate::attach_exchange::Attached>,
    ) -> Result<i32> {
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
        // The client's parked standby (spec §3.5): a link held, not used,
        // until the client's first frame arrives on it. A local rather than a
        // field so the arm watching it borrows the loop, not `self` (C1).
        let mut standby: Option<Link> = None;
        // A standby whose first frame has arrived, owed to the next turn with
        // that frame. `promote_standby` does the reset and the feed together,
        // in that order.
        let mut promote: Option<(Link, Frame)> = None;
        // The exit wake fired but `child_exited` disagreed. It is edge
        // triggered and will not fire twice, so re-check on a timer instead of
        // trusting the hint — the same rule that keeps PTY EOF out of this.
        let mut recheck_child = false;

        loop {
            let turn = match (promote.take(), pending.take()) {
                // `pending` is always empty here: a lap sets one or the other.
                (Some((link, first)), _) => self
                    .promote_standby(link, first)
                    .context("switching to the standby")?,
                (None, None) => self.turn()?,
                (None, Some(frame)) => self.turn_with(Some(frame))?,
            };
            if let Some(code) = turn.exited {
                // Closed, not dropped: its control server holds the
                // connection open for as long as it is not.
                if let Some(parked) = standby.take() {
                    close_as_exited(parked.sink.connection(), code);
                }
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
                // A closed mpsc receiver (the `run()` wrapper's, forever)
                // yields `None` immediately, and `Some(a) = ...` disables the
                // arm rather than making it hot — the same reason there is no
                // `conn.closed()` arm above.
                Some(a) = attaches.recv() => HostWake::Attached(a),
                // A parked standby carries nothing until the client fails
                // over onto it, so this arm is quiet until it matters. A
                // closed one yields `None` once, and is then un-parked.
                f = async { standby.as_mut().expect("armed").source.recv().await },
                    if standby.is_some() => match f {
                        Some(frame) => HostWake::StandbyFrame(frame),
                        None => HostWake::StandbyGone,
                    },
            };

            match wake {
                HostWake::Frame(frame) => pending = Some(frame),
                HostWake::Exit => recheck_child = true,
                HostWake::Pty | HostWake::Due => {}
                HostWake::Attached(a) => self.on_attached(a, &mut standby)?,
                HostWake::StandbyFrame(frame) => {
                    let link = standby.take().expect("the arm was armed");
                    promote = Some((link, frame));
                }
                // Its connection is already closed, which is what ended the
                // source; there is nothing left to close.
                HostWake::StandbyGone => standby = None,
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
        close_as_exited(self.link.sink.connection(), code);
    }

    /// The authoritative screen, for tests. Nothing in the session loop reads
    /// it: the loop ships diffs and never inspects what it shipped.
    #[allow(dead_code)]
    pub fn screen(&self) -> &ScreenState {
        self.screen_tx.current()
    }

    /// Swap a freshly attached link in for the current one.
    ///
    /// Design spec §8.5: both sync channels restart at sequence 1 and the
    /// first datagram of the new attach is a full state. `screen_stale` is
    /// what forces that snapshot on the next turn.
    pub fn adopt(&mut self, link: Link, size: TermSize) -> Result<()> {
        self.adopt_as(link, size, TAKEN_OVER)
    }

    /// [`HostSession::adopt`], closing the displaced link with `reason`:
    /// [`TAKEN_OVER`] for a newer attach, [`SWITCHED`] for the client's own
    /// standby.
    pub fn adopt_as(&mut self, link: Link, size: TermSize, reason: &'static [u8]) -> Result<()> {
        // Close the displaced connection FIRST, and say why. A displaced
        // client that is merely dropped reports silence, which is the one
        // thing that did not happen.
        self.link
            .sink
            .connection()
            .close(quinn::VarInt::from_u32(0), reason);

        self.link = link;
        // `resize`, not `self.size = size`. The field means "the size the
        // terminal currently IS", and writing it directly left the emulator
        // and the pty at the FIRST client's geometry while every frame
        // announced the newcomer's. Nothing downstream healed it either: the
        // `input_rx` seeded below carries this same size, so `turn_at`'s
        // `wanted != self.size` self-heal is false on every subsequent turn
        // and the client only re-sends a size on a window change. A newcomer
        // on a differently-sized terminal is the common case for reattach.
        self.resize(size)
            .context("resizing for the second attach")?;

        // §8.5. New generation, so both channels restart at 1. The screen
        // itself is NOT reset — the emulator kept running — so the base state
        // is blank and `screen_stale` forces the snapshot that fills it.
        // Built from `self.size`, which the resize above has just made current
        // (and which `resize` leaves untouched when the size did not change).
        let blank = ScreenState::blank(self.size.rows, self.size.cols)?;
        self.screen_tx = oxutrm_sync::Sender::new(blank);
        self.input_rx = Receiver::new(InputState {
            seq: 1,
            pending: Vec::new(),
            size,
        });
        self.written = 0;
        self.last_send = None;
        self.screen_stale = true;
        // An attach has just completed and the client sends immediately, so
        // "now" is true rather than optimistic — the same reasoning as `spawn`.
        self.last_heard = Instant::now();
        Ok(())
    }

    /// One completed attach, by role. `standby` is the loop's local slot.
    ///
    /// Every link this lets go of is closed, and with a reason: a dropped
    /// link is not closed at all, because its control server holds a handle
    /// to the connection for as long as the connection is open.
    pub(crate) fn on_attached(
        &mut self,
        a: crate::attach_exchange::Attached,
        standby: &mut Option<Link>,
    ) -> Result<()> {
        match a.role {
            crate::control::Role::Primary => {
                // A takeover. The standby belongs to the displaced client, and
                // leaving it parked would let that client take the session back
                // by failing over onto it, without going through ssh.
                if let Some(old) = standby.take() {
                    old.sink
                        .connection()
                        .close(quinn::VarInt::from_u32(0), TAKEN_OVER);
                }
                self.adopt(a.link, a.client_size)
                    .context("adopting a second attach")
            }
            crate::control::Role::Standby => {
                // One slot. A newer standby supersedes the older one.
                //
                // Nothing checks that the client which asked for this one is
                // still the primary's. The listener can finish a standby's
                // exchange just after a takeover, so the displaced client's
                // standby may land here. It does no harm: that client exits on
                // its `TAKEN_OVER` and never sends on it. It stays parked
                // until a standby the new client finds supersedes it, or the
                // next primary attach drops it.
                if let Some(old) = standby.replace(a.link) {
                    old.sink
                        .connection()
                        .close(quinn::VarInt::from_u32(0), SUPERSEDED);
                }
                Ok(())
            }
        }
    }

    /// The client has failed over: its first frame arrived on the parked
    /// standby. Adopt the standby, then take that frame in, in one turn.
    ///
    /// Reset first, THEN feed. `adopt_as` restarts the input receiver at a
    /// fresh generation, and the client's first frame after its own reset
    /// belongs to that generation. Fed first, it would meet the old
    /// generation's receiver, be taken for a stale frame and thrown away.
    ///
    /// The size is the session's current one. The client's frame carries its
    /// real size in `InputState`, and `turn_at` reconciles it.
    pub(crate) fn promote_standby(&mut self, link: Link, first: Frame) -> Result<Turn> {
        let size = self.size;
        self.adopt_as(link, size, SWITCHED)?;
        self.turn_with(Some(first))
    }
}

/// Close `conn` saying the shell exited, with `code` as its status.
///
/// A code outside `u32` cannot come from a shell; see [`HostSession::close`].
fn close_as_exited(conn: &quinn::Connection, code: i32) {
    let code = u32::try_from(code).unwrap_or(255);
    conn.close(quinn::VarInt::from_u32(code), SHELL_EXITED);
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
    /// A rebuild attempt finished, one way or another.
    Rebuilt(AttemptOutcome),
    /// A standby search or probe reported back.
    Standby(crate::standby::StandbyEvent),
    /// The standby's connection closed, with this reason.
    StandbyClosed(quinn::ConnectionError),
    Closed(quinn::ConnectionError),
    /// A readiness that turned out to be nothing. Costs one lap.
    Nothing,
}

/// Was this link closed because a newer attach displaced it?
///
/// The reason phrase, and not merely "an application close": every deliberate
/// close looks like one, and [`exit_code`] exists because they mean different
/// things. This is the one the host sends as it adopts a newcomer.
fn is_takeover(reason: &quinn::ConnectionError) -> bool {
    matches!(
        reason,
        quinn::ConnectionError::ApplicationClosed(closed) if closed.reason.as_ref() == TAKEN_OVER
    )
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
    /// A second attach completed. Carries the whole thing, because the
    /// session needs the size as well as the link.
    Attached(crate::attach_exchange::Attached),
    /// A frame arrived on the parked standby: the client has failed over.
    StandbyFrame(Frame),
    /// The parked standby's connection is gone.
    StandbyGone,
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
/// [`close_as_exited`] is the only place that sends it: for the session link
/// (through [`HostSession::close`]) and for a parked standby at exit.
pub const SHELL_EXITED: &[u8] = b"the shell exited";

/// Why a link was closed because another one arrived.
///
/// Read by the displaced client so it can say it was taken over rather than
/// reporting silence. Spec §6; the `Displaced` state itself is B4.
pub const TAKEN_OVER: &[u8] = b"taken over by a newer attach";

/// The close reason for a primary replaced by its own client's standby
/// (spec §3.5). Not `TAKEN_OVER`: nobody else took anything, and the client
/// must not read its own failover as a displacement.
pub const SWITCHED: &[u8] = b"switched to the standby link";

/// The close reason for a parked standby replaced by a newer one. Not
/// `SWITCHED`: nothing switched to it, it was never used at all.
pub const SUPERSEDED: &[u8] = b"superseded by a newer standby";

/// Why the client closed a link of its own: it built a better one.
///
/// Local, and it never reaches anybody. The host on the far end of a link this
/// closes has already adopted the replacement -- that is what made the swap
/// possible -- so this is the client tidying up a connection whose successor
/// is already carrying the session. It exists so that a `close` here is as
/// legible in a packet trace as [`TAKEN_OVER`] is.
pub const REBUILT: &[u8] = b"replaced by a rebuilt link";

/// What `conn` reports right now, as plain values for [`Quality`].
fn reading_of(conn: &quinn::Connection) -> Reading {
    let stats = conn.stats();
    Reading {
        rtt: conn.rtt(),
        sent: stats.path.sent_packets,
        lost: stats.path.lost_packets,
        tx_bytes: stats.udp_tx.bytes,
        rx_bytes: stats.udp_rx.bytes,
    }
}

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
    /// The primary's path: the one `announce` printed, until a migration, a
    /// failover or a landed rebuild replaces it.
    path: Option<PathDescription>,
    /// Whether the host is still answering, and what the user is told.
    link_state: LinkState,
    /// Frames that arrived and could not be applied, for the popup.
    ///
    /// This used to be an `eprintln!`, which was a bug rather than a
    /// diagnostic: the client's stderr IS the terminal it is painting, so the
    /// message desynchronised the renderer's model and nothing repainted it on
    /// a quiet session.
    rejected_total: u64,
    /// What is drawn as layer 1, so an unchanged popup is not laid out again
    /// every lap. Mirrors the overlay exactly: `None` means no overlay.
    shown: Option<PopupView>,
    /// The popup's state, and where keystrokes go.
    ui: Ui,
    /// A minute of the primary link's measurements, for the popup.
    quality: Quality,
    /// What oxutrm did to keep the session alive.
    activity: Activity,
    /// Which session this is. `None` where it was not reached over ssh --
    /// the fixtures in this file's tests.
    identity: Option<Identity>,
    /// The source address the link was working from, so a moved route can be
    /// spotted. Seeded in [`ClientSession::new`] from the path the connection
    /// came up over. See [`crate::roam`].
    route: crate::roam::RouteWatch,
    /// When the route was last probed, so `Silent` does not probe on every
    /// lap of a loop that wakes up to 125 times a second.
    ///
    /// Cleared on any lap that is not `Silent`, so the pace belongs to one
    /// outage rather than to the session: see [`ClientSession::follow_route`].
    probed_at: Option<Instant>,
    /// How to get back into this session, and the attempt in flight if there
    /// is one. `None` where there is nothing to rebuild through -- the
    /// fixtures in this file's own tests, which build a link directly rather
    /// than over ssh.
    rebuild: Option<Rebuild>,
    /// Why the last rebuild attempt failed, for the popup.
    ///
    /// Spec §5.1: the popup says what the last attempt produced, when it
    /// produced anything. Cleared when the phase leaves `Recovering`, so a
    /// later outage never opens by reporting an older one's reason.
    last_failure: Option<String>,
    /// The standby link, and the search for one (spec §3). `None` for a
    /// host that did not offer one; see [`ClientSession::with_standby`].
    standby: Option<crate::standby::Standby>,
}

impl ClientSession {
    /// `rebuild` is what a lost link is rebuilt through: the ssh target and
    /// the session id the host named, or `None` for a session that cannot be
    /// rebuilt because it was not reached over ssh. It is a parameter rather
    /// than a setter so that every construction site has to say which it is —
    /// a session that silently could not rebuild would look exactly like one
    /// whose network never came back.
    pub fn new(
        size: TermSize,
        caps: TerminalCaps,
        link: Link,
        rebuild: Option<Rebuild>,
    ) -> Result<ClientSession> {
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
            path: None,
            link_state: LinkState::new(Instant::now()),
            rejected_total: 0,
            shown: None,
            ui: Ui::new(),
            quality: Quality::new(Instant::now()),
            activity: Activity::new(),
            identity: None,
            route: crate::roam::RouteWatch::new(seed),
            probed_at: None,
            rebuild,
            last_failure: None,
            standby: None,
        })
    }

    /// Search for, and fail over onto, a standby (spec §3). Only for a host
    /// that offered one; see `connect::offers_standby`.
    pub(crate) fn with_standby(mut self, s: crate::standby::Standby) -> ClientSession {
        self.standby = Some(s);
        self
    }

    /// Which session this is, for the popup's title and status rows.
    pub(crate) fn with_identity(mut self, id: Identity) -> ClientSession {
        self.identity = Some(id);
        self
    }

    /// Keep the activity log in `activity` -- the one with the file, which
    /// `connect` opens -- instead of the ring-only log `new` starts with.
    pub(crate) fn with_activity(mut self, activity: Activity) -> ClientSession {
        self.activity = activity;
        self
    }

    /// Tell the user what connection they got — once, and then be quiet.
    ///
    /// Spec §10.3: oxutrm never does anything clever silently. On connect this
    /// prints one line. Called again with the same path it prints **nothing**,
    /// which is what makes the silence a property rather than an accident.
    /// Called again with a different path it records the migration in the
    /// activity log, where the popup shows it: walking from Wi-Fi to mobile
    /// should be explained rather than mysterious, and the session owns the
    /// screen by then.
    ///
    /// Rung 4 reads as a warning, and that is not decoration: a session inside
    /// the SSH connection cannot daemonize and cannot be reattached, so
    /// degrading to it silently would remove both properties the project
    /// exists to provide while looking like success.
    pub fn announce<W: Write>(&mut self, path: &PathDescription, out: &mut W) -> Result<bool> {
        let same = self
            .path
            .as_ref()
            .is_some_and(|old| old.rung == path.rung && old.remote == path.remote);
        if same {
            return Ok(false);
        }

        if self.path.is_some() {
            // A migration, not a fresh connect. The session owns the screen
            // by now, so it goes to the activity log, not the terminal.
            self.activity.record(
                Kind::Link,
                &format!("path migrated \u{2192} {}", oxutrm_client::rung_label(path)),
            );
            self.path = Some(path.clone());
            return Ok(true);
        }

        writeln!(out, "{}", status_line(path)).context("announcing the path")?;
        out.flush().context("flushing the terminal")?;

        // The line was written outside the renderer's model of the screen, so
        // that model is now wrong by one row. Anything less than a full
        // repaint would leave the terminal and the model disagreeing.
        self.renderer.invalidate();
        self.path = Some(path.clone());
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
                Ok(applied) => {
                    if applied {
                        turn.applied += 1;
                        painted = true;
                    }
                    // The host was heard, whether or not the frame changed
                    // anything. `Ok(false)` is what an ACK-ONLY frame reports
                    // -- the answer a host with an unmoved screen gives a
                    // heartbeat, repeating its own state number so there is
                    // nothing to apply -- and on an idle session it is the
                    // only proof of life there will ever be until somebody
                    // types. Tying this to `Ok(true)` left the popup saying
                    // nobody was answering up for ever on exactly the session
                    // that had just recovered, with nothing owed any more:
                    // `evaluate` returns early while `Silent` so the counter
                    // does not restart, and only `heard` can leave it.
                    //
                    // `Err` is deliberately NOT heard. A frame the receiver
                    // could not apply leaves the screen frozen, and the
                    // popup is the only place the rejected count is
                    // reported -- going back to `Live` there would take the
                    // one explanation away and leave a stale screen with none.
                    //
                    // The loop's `Wake::Frame` arm is not the only path here:
                    // `try_recv` below scavenges frames on pacing, keyboard
                    // and resize laps, and a frame that landed on one of those
                    // used to repaint the screen underneath a popup still
                    // saying nobody was answering. It also moves
                    // `last_heard`, which is what the `silent for Ns`
                    // counter is built from.
                    self.note_heard(Instant::now());
                }
                // See the host's copy of this arm: a silently swallowed
                // BaseMismatch is a frozen screen that looks like a slow one.
                Err(_) => {
                    turn.rejected += 1;
                    // NOT `eprintln!`: the client's stderr is the terminal it
                    // is painting, so a message here desynchronises the
                    // renderer's model and nothing repaints it on a quiet
                    // session. The count reaches the user through the popup.
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

    /// For tests and for the popup.
    pub fn rejected_total(&self) -> u64 {
        self.rejected_total
    }

    /// For tests: hands a still-live `ClientSession`'s link to
    /// [`HostSession::adopt`], which is what a Tier B reattach test needs a
    /// second, independent link for. Consumes the whole session rather than
    /// exposing the field as `pub`, because nothing else about a
    /// `ClientSession` whose link has been taken makes sense to keep using.
    ///
    /// `oxutrm` is a bin-only crate with no `[lib]` target, and the one
    /// integration test (`tests/serve_exits.rs`) is a black-box subprocess
    /// test that never links this module's internals — so the only caller is
    /// the `#[cfg(test)] mod tests` block below. `#[cfg(test)]` here compiles
    /// this only for that build, where it IS used, rather than suppressing a
    /// dead-code warning on a method the production binary would otherwise
    /// carry unused.
    #[cfg(test)]
    pub(crate) fn link_take(self) -> Link {
        self.link
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
    /// The popup decides ([`Ui::keys`]): while it is shown every key is its
    /// own and nothing is sent or held; while it is closed, `Ctrl-\` opens
    /// it and every other byte goes to the host untouched -- or, while the
    /// link is down, is held for the question asked when it answers again.
    ///
    /// `Some(code)` means the user asked to close oxutrm, which is the one
    /// answer that ends the loop. A method rather than the body of the
    /// `Wake::Keys` arm because the arm cannot be reached from a test without
    /// a real terminal; the arm is a single call to this, so what is tested
    /// is what ships.
    fn route_keys<W: Write>(&mut self, keys: &[u8], out: &mut W) -> Result<Option<i32>> {
        self.route_keys_at(keys, Instant::now(), out)
    }

    /// [`ClientSession::route_keys`] for a read taken at `now`, so a test can
    /// answer the question after [`crate::ui::ANSWER_GUARD`] without sleeping.
    fn route_keys_at<W: Write>(
        &mut self,
        keys: &[u8],
        now: Instant,
        out: &mut W,
    ) -> Result<Option<i32>> {
        let phase = self.link_state.phase_now();
        let routed = self.ui.keys(keys, phase, now);
        if !routed.to_host.is_empty() {
            self.turn(&routed.to_host, out)?;
        }
        if !routed.to_hold.is_empty() {
            self.link_state.hold_keys(&routed.to_hold);
        }
        match routed.command {
            Some(Command::Quit) => return Ok(Some(0)),
            Some(Command::SendHeld) => {
                let held = self.link_state.take_held();
                self.activity.record(
                    Kind::Input,
                    &format!("held input sent ({} bytes)", held.len()),
                );
                self.turn(&held, out)?;
            }
            Some(Command::DropHeld) => {
                let n = self.link_state.held().len();
                self.link_state.drop_held();
                self.activity
                    .record(Kind::Input, &format!("held input dropped ({n} bytes)"));
            }
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

    /// One lap of layer 1: decide the phase, move the popup on, and return
    /// what it should show at `now` -- `None` while it is closed.
    ///
    /// Built afresh on every lap it is open and compared by the loop with
    /// what is on the screen. Every number in it that moves by itself is in
    /// whole seconds, so an open popup with nothing happening repaints
    /// rarely and costs one comparison otherwise; a change of phase is
    /// reported the lap it happens.
    fn popup_at(&mut self, now: Instant) -> Option<PopupView> {
        let owed = self.input_tx.current().seq() != self.screen_rx.peer_ack();
        let phase = self.link_state.evaluate(now, owed);
        match self.ui.tick(phase, now) {
            Some(LinkChange::WentSilent) => self.activity.record(Kind::Link, "silent"),
            Some(LinkChange::Back { outage }) => {
                let text = format!(
                    "live again via {}, outage {:.1} s",
                    crate::view::path_label(self.path.as_ref()),
                    outage.as_secs_f64()
                );
                self.activity.record(Kind::Link, &text);
            }
            None => {}
        }
        self.ui.visible(phase).then(|| self.view(phase, now))
    }

    /// What the popup says at `now`.
    fn view(&self, phase: Phase, now: Instant) -> PopupView {
        let lingering = match self.ui.mode() {
            Mode::Lingering { outage, .. } => Some(outage),
            _ => None,
        };
        let standby = self.standby.as_ref().map(|s| StandbyFacts {
            path: s.path(),
            rtt: s.rtt(),
            probe: s.probe(),
            searching: s.searching(),
            next_search: s.next_search(),
            last_failure: s.last_failure(),
        });
        crate::view::build(&Facts {
            identity: self.identity.as_ref(),
            phase,
            lingering,
            last_heard: self.link_state.last_heard(),
            path: self.path.as_ref(),
            quality: &self.quality,
            rejected: self.rejected_total(),
            standby,
            rebuild_failure: self.last_failure.as_deref(),
            held: self.link_state.held(),
            held_full: self.link_state.held_is_full(),
            activity: &self.activity,
            now,
            wall: SystemTime::now(),
        })
    }

    /// One sample of the primary link, if a second has passed since the
    /// last. Rides on the loop's laps; nothing is armed for it.
    fn sample_quality(&mut self, now: Instant) {
        if !self.quality.due(now) {
            return;
        }
        let outage = self.link_state.phase_now().is_outage();
        self.quality
            .push(now, reading_of(self.link.sink.connection()), outage);
    }

    /// Whether the popup at `now` is reporting an outage: the question the
    /// tests that predate the popup asked of the notice, which closed the
    /// moment the host answered. The popup lingers instead, so "is it up"
    /// no longer means "is the host silent".
    #[cfg(test)]
    fn outage_at(&mut self, now: Instant) -> bool {
        self.popup_at(now).is_some_and(|v| {
            matches!(
                v.marker,
                oxutrm_client::Marker::Silent | oxutrm_client::Marker::Recovering
            )
        })
    }

    /// One lap of the rebuild loop.
    ///
    /// Called once per lap of [`ClientSession::run_on`], with the same `now`
    /// the popup was built from, and a method rather than the body of that
    /// loop for the reason [`ClientSession::route_keys`] is one: the loop
    /// itself cannot be reached from a test without a real terminal and a real
    /// twenty seconds of silence, and the clock being a parameter is what lets
    /// this be driven instead. The loop calls nothing else, so what is tested
    /// is what ships.
    ///
    /// Two rules, and the second is the one worth stating: **whichever path
    /// revives first wins**. A frame on the old link has already taken the
    /// phase out of `Recovering` by the time this runs, and the attempt in
    /// flight is then not merely redundant -- left running, it could land and
    /// swap the transport under a session that had already come back.
    fn rebuild_step(&mut self, now: Instant, outcomes: &tokio::sync::mpsc::Sender<AttemptOutcome>) {
        let phase = self.link_state.phase_now();
        let size = self.size;
        let Some(rebuild) = self.rebuild.as_mut() else {
            return;
        };

        let Phase::Recovering { attempt, next_try } = phase else {
            // `stood_down` and not a bare `cancel`: the displacing latch
            // belongs to the outage too. Left set, the next `TAKEN_OVER` on
            // this link -- somebody else attaching, an hour later, over a
            // healthy network -- would be excused as our own rebuild, and the
            // `conn.closed()` arm that reports it would stay disabled for the
            // rest of the session.
            rebuild.stood_down();
            // Belongs to the outage that has just ended, not to the session:
            // the same reasoning as `follow_route`'s `probed_at`.
            self.last_failure = None;
            return;
        };

        // One at a time. `next_try` is not a pace to catch up on: a lap that
        // ran late must start one attempt, not the several it was "due".
        if rebuild.is_running() || now < next_try {
            return;
        }
        rebuild.begin(size, outcomes.clone());
        self.link_state.begin_attempt(now);
        self.activity.record(
            Kind::Rebuild,
            &format!("attempt {} started", attempt.saturating_add(1)),
        );
    }

    /// A rebuild attempt failed for a reason worth retrying.
    fn rebuild_failed(&mut self, why: String, now: Instant) {
        self.record_attempt_failed(&why);
        self.link_state.attempt_failed(now);
        // Kept for the popup's recovering section.
        self.last_failure = Some(why);
    }

    /// The log entry for a failed rebuild attempt, retried or not.
    fn record_attempt_failed(&mut self, why: &str) {
        if let Phase::Recovering { attempt, .. } = self.link_state.phase_now() {
            self.activity.record(
                Kind::Rebuild,
                &format!("attempt {} failed: {why}", attempt.saturating_add(1)),
            );
        }
    }

    /// A rebuild attempt got an answer that repeating the question will not
    /// change. The session ends with it, so the log says how: the error is
    /// what `run_connect` prints once the raw guard is dropped.
    fn rebuild_refused(&mut self, why: &str) -> anyhow::Error {
        self.record_attempt_failed(why);
        anyhow::anyhow!("this session cannot be resumed: {why}")
    }

    /// A rebuild attempt landed: its link replaces the one that stopped
    /// answering, and its path and attach are the session's from here.
    fn rebuild_landed(&mut self, e: crate::connect::Established, now: Instant) -> Result<()> {
        self.swap_in(e.link, now)?;
        if let Some(id) = self.identity.as_mut() {
            id.attach_id = e.attach_id;
        }
        self.activity.record(
            Kind::Rebuild,
            &format!("landed via {}", oxutrm_client::rung_label(&e.path)),
        );
        self.path = Some(e.path);
        Ok(())
    }

    /// Swap the standby in for a primary that stopped answering (spec §3.5).
    /// Returns whether there was one to swap in.
    fn fail_over<W: Write>(&mut self, now: Instant, out: &mut W) -> Result<bool> {
        let Some(e) = self.standby.as_mut().and_then(|s| s.take_for_failover(now)) else {
            return Ok(false);
        };
        self.swap_in_as(e.link, now, SWITCHED)
            .context("failing over to the standby")?;
        // Recorded before the first frame goes: the switch has happened, and
        // a turn that errors must not leave it out of the log.
        if let Some(id) = self.identity.as_mut() {
            id.attach_id = e.attach_id;
        }
        self.activity.record(
            Kind::Failover,
            &format!(
                "switched to standby ({})",
                oxutrm_client::rung_label(&e.path)
            ),
        );
        self.path = Some(e.path);
        // Spec §3.5 step 2: the host adopts the standby on our first frame
        // on it, so that frame goes now.
        self.turn(&[], out)?;
        Ok(true)
    }

    /// One standby decision (see [`crate::standby::Standby::step`]), with
    /// what it starts recorded. The loop acts on the returned action.
    ///
    /// "probing standby" is recorded for the first probe since the probe
    /// state was last reset to `Idle` (a rebuild attempt or a forgotten
    /// answer resets it, so it can recur within one outage): a probe that
    /// keeps failing is retried every `PROBE_RETRY`, and an entry per retry
    /// would alternate with its "probe failed" so that nothing folds, and a
    /// long outage would push everything useful out of the ring. Only a
    /// probe from `Idle` is a first one; a retry comes from `Failed`.
    fn standby_step(
        &mut self,
        phase: Phase,
        now: Instant,
        rebuild_running: bool,
    ) -> crate::standby::StandbyAction {
        let Some(s) = self.standby.as_mut() else {
            return crate::standby::StandbyAction::Nothing;
        };
        let first_probe = s.probe() == crate::linkstate::ProbeState::Idle;
        let action = s.step(phase, now, rebuild_running);
        match action {
            crate::standby::StandbyAction::Search { .. } => {
                self.activity.record(Kind::Standby, "search started");
            }
            crate::standby::StandbyAction::Probe { .. } if first_probe => {
                self.activity.record(Kind::Failover, "probing standby");
            }
            _ => {}
        }
        action
    }

    /// A standby search or probe reported back. Returns the connection the
    /// loop should watch for closing, when a search found one that was kept.
    fn on_standby_event(
        &mut self,
        event: crate::standby::StandbyEvent,
        now: Instant,
    ) -> Option<quinn::Connection> {
        let s = self.standby.as_mut()?;
        match event {
            crate::standby::StandbyEvent::Found { search, e } => {
                let path = e.path.clone();
                if !s.found(search, *e) {
                    return None;
                }
                let watch = s.connection();
                self.activity.record(
                    Kind::Standby,
                    &format!(
                        "found {}, {} ms",
                        oxutrm_client::rung_label(&path),
                        path.rtt_ms
                    ),
                );
                watch
            }
            crate::standby::StandbyEvent::NotFound { search, reason } => {
                let text = format!("not found: {reason}");
                if s.not_found(search, now, reason) {
                    self.activity.record(Kind::Standby, &text);
                }
                None
            }
            crate::standby::StandbyEvent::Probed { answered } => {
                if s.probed(answered, now) {
                    self.activity.record(
                        Kind::Failover,
                        if answered {
                            "probe answered"
                        } else {
                            "probe failed"
                        },
                    );
                }
                None
            }
        }
    }

    /// The standby's connection closed. Whatever the reason, the primary is
    /// untouched: a standby's close is information, not a reason to end
    /// anything.
    fn on_standby_closed(&mut self, reason: &quinn::ConnectionError, now: Instant) {
        if self.standby.as_mut().is_some_and(|s| s.lost(now, reason)) {
            self.activity
                .record(Kind::Standby, &format!("lost: {reason}"));
        }
    }

    /// Put a freshly rebuilt link in place of the one that stopped answering.
    ///
    /// The client's half of [`HostSession::adopt`], and deliberately its
    /// mirror image: design spec §8.5 says both ends reset their sequence
    /// counters at every attach and the host's first datagram is a full state.
    /// The host has just done exactly that on its side -- this attempt reached
    /// it as an ordinary attach -- so anything less here would leave the two
    /// ends disagreeing about the base from the first frame onwards.
    ///
    /// **Input the host had not acknowledged is dropped rather than replayed.**
    /// `adopt` resets the host's `written` counter along with its receiver, so
    /// carrying the old pending bytes across would write them to the shell a
    /// second time. Anything typed while the link was down is not in here at
    /// all: it is held in [`LinkState`] and still needs the user's answer.
    ///
    /// The phase is deliberately NOT left. Landing is not the same as being
    /// answered, and the first frame to arrive is what returns the phase to
    /// `Live` -- or to `Confirming`, if there is blind typing to ask about.
    /// What the phase DOES get is a fresh schedule, and that is not a detail:
    /// see [`LinkState::rebuilt`] for the self-displacing loop it prevents.
    fn swap_in(&mut self, link: Link, now: Instant) -> Result<()> {
        self.swap_in_as(link, now, REBUILT)?;
        // Whatever we displaced, we have now displaced. A `TAKEN_OVER` after
        // this point belongs to somebody else.
        if let Some(rebuild) = self.rebuild.as_mut() {
            rebuild.swapped();
        }
        // The host dropped the parked standby when it adopted this attach,
        // so ours is a corpse.
        if let Some(s) = self.standby.as_mut() {
            s.forget(now);
        }
        Ok(())
    }

    /// [`ClientSession::swap_in`], closing the old link with `reason`:
    /// [`REBUILT`] for a rebuild, [`SWITCHED`] for a failover onto the
    /// standby (spec §3.5).
    ///
    /// The rebuild's displacement latch is NOT cleared here, and that is the
    /// difference that matters (ruling B1). A failover is not our rebuild
    /// landing: an ssh attempt started before it may still reach the host
    /// and close this new link as taken over, and that close has to be read
    /// as our own doing, not as the end of the session. The latch is cleared
    /// when frames make the phase `Live` again (`Rebuild::stood_down`), or by
    /// a rebuild that lands.
    pub(crate) fn swap_in_as(
        &mut self,
        link: Link,
        now: Instant,
        reason: &'static [u8],
    ) -> Result<()> {
        // Closed first, and with a reason, for the same reason `adopt` closes
        // the displaced one with `TAKEN_OVER`: a connection that is merely
        // dropped is indistinguishable from one that went quiet.
        self.link
            .sink
            .connection()
            .close(quinn::VarInt::from_u32(0), reason);

        self.link = link;

        let blank = ScreenState::blank(self.size.rows, self.size.cols)?;
        self.screen_rx = Receiver::new(blank);
        self.input_tx = Sender::new(InputState {
            seq: 1,
            pending: Vec::new(),
            size: self.size,
        });
        self.last_send = None;

        // The baseline the route probe compares against belongs to the path
        // that has just gone away. Re-seeded here for the same reason
        // `ClientSession::new` seeds it at all: taken later, inside the next
        // outage, it would already be the address the machine had moved to.
        self.route = crate::roam::RouteWatch::new(
            crate::roam::route_source(self.link.sink.connection().remote_address()).ok(),
        );
        self.probed_at = None;
        self.last_failure = None;

        // A full backoff before another attempt, and the old link's failures
        // left behind with it.
        self.link_state.rebuilt(now);
        // A new connection, whose counters start at zero.
        self.quality.new_segment(now);
        Ok(())
    }

    /// The window changed size. The renderer forgets what is painted and the
    /// next input diff tells the host.
    pub fn resize(&mut self, size: TermSize) {
        if size == self.size {
            return;
        }
        self.renderer.resize(size);
        self.size = size;

        // Layer 1 was laid out for the screen that just went away, and the
        // loop rebuilds it only when the VIEW changes -- which a resize does
        // not. The same view, laid out again, rather than `self.shown =
        // None`: `shown` has to keep mirroring what the overlay is, or a lap
        // whose view is also `None` would never clear a box stranded on the
        // screen.
        if let Some(v) = self.shown.as_ref() {
            self.renderer.set_overlay(Some(layout_popup(v, size)));
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
        //
        // `mut`, because a rebuild swaps the link underneath the session and
        // this has to follow it. Left pointing at the old connection, the arm
        // would watch a connection that has just been closed on purpose --
        // permanently ready, so the loop would spin and then report the
        // session as over.
        let mut conn = self.link.sink.connection().clone();

        // Where a rebuild attempt reports back. A LOCAL and never a field, so
        // the arm below borrows this and not `self` (C1) -- exactly as
        // `HostSession::run_with_attaches` holds its receiver, and for the
        // same reason. Depth one: only one attempt ever runs at a time.
        let (outcomes_tx, mut outcomes) = tokio::sync::mpsc::channel::<AttemptOutcome>(1);
        // A `TAKEN_OVER` close that has already been explained: our own
        // rebuild reaching the host. The ARM is disabled rather than the wake
        // being ignored, because a closed connection is permanently ready, so
        // an ignored wake would come straight back on every lap for as long as
        // the rebuild ran.
        //
        // It stays disabled until a swap puts a live connection in `conn` --
        // including when the attempt that displaced us then fails. That is not
        // an oversight: the old link is closed either way, this client is
        // still in `Recovering` and still trying, and re-arming the arm on a
        // connection that is closed for ever is the spin above, with an exit
        // at the end of it.
        let mut takeover_expected = false;

        // Where standby searches and probes report back. A local, for the
        // reason `outcomes` is one (C1). Two deep: at most one search and one
        // probe are ever in flight.
        let (standby_tx, mut standby_rx) =
            tokio::sync::mpsc::channel::<crate::standby::StandbyEvent>(2);
        // The standby's connection, watched for closing. A local for the same
        // reason `conn` is, and seeded here because a standby may already be
        // in place when the loop starts.
        let mut standby_conn: Option<quinn::Connection> = self
            .standby
            .as_ref()
            .and_then(crate::standby::Standby::connection);
        // The search and the probe in flight. Held so they end with the loop:
        // dropping a `JoinHandle` detaches its task, and a search left running
        // would go on holding the primary and a socket of its own.
        let mut _search_task: Option<crate::attach_exchange::AbortOnDrop> = None;
        let mut _probe_task: Option<crate::attach_exchange::AbortOnDrop> = None;

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
                Some(outcome) = outcomes.recv() => Wake::Rebuilt(outcome),
                Some(event) = standby_rx.recv() => Wake::Standby(event),
                // Quiet until the standby goes away; a closed connection is
                // ready for ever, which is why the handler disarms it.
                reason = async { standby_conn.as_ref().expect("armed").closed().await },
                    if standby_conn.is_some() => Wake::StandbyClosed(reason),
                reason = conn.closed(), if !takeover_expected => Wake::Closed(reason),
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
                // A rebuild attempt finished.
                Wake::Rebuilt(outcome) => {
                    if let Some(rebuild) = self.rebuild.as_mut() {
                        rebuild.finished();
                    }
                    match outcome {
                        AttemptOutcome::Landed(established) => {
                            self.rebuild_landed(*established, Instant::now())
                                .context("swapping in the rebuilt link")?;
                            // Both of these belong to the link that has just
                            // been replaced: the arm has to watch the new
                            // connection, and a takeover on the old one can no
                            // longer arrive because nothing is watching it.
                            conn = self.link.sink.connection().clone();
                            takeover_expected = false;
                            // `swap_in` has forgotten (and closed) the
                            // standby, which the host dropped as it adopted
                            // this attach. A search still running over the
                            // old primary is disowned with it, and stopped.
                            standby_conn = None;
                            _search_task = None;
                        }
                        // The network, not the far end. The loop keeps its
                        // cadence and the popup explains the last try.
                        AttemptOutcome::Retry(why) => self.rebuild_failed(why, Instant::now()),
                        // An answer, and repeating the question gets the same
                        // one. `run_connect` prints this after the raw guard
                        // is dropped and exits non-zero.
                        AttemptOutcome::Definite(why) => {
                            return Err(self.rebuild_refused(&why));
                        }
                    }
                }
                Wake::Standby(event) => {
                    if let Some(watch) = self.on_standby_event(event, Instant::now()) {
                        standby_conn = Some(watch);
                    }
                }
                // Whatever the reason, the standby is gone; `lost` decides
                // whether it is worth recording.
                Wake::StandbyClosed(reason) => {
                    standby_conn = None;
                    self.on_standby_closed(&reason, Instant::now());
                }
                // The link is gone, but what already arrived over it is not.
                // Paint it before answering, or `ls; exit` shows the user
                // nothing at all.
                Wake::Closed(reason) => {
                    // A rebuild that reached the host displaces this link --
                    // the host closes it as it adopts the newcomer, which is
                    // what `HostSession::adopt` is for. Reporting that as the
                    // end of the session would end it at the exact moment it
                    // was rescued (spec §5.3).
                    //
                    // `may_have_displaced_us` and not `is_running`: an attempt
                    // can get far enough for the host to adopt it -- closing
                    // this link -- and fail AFTERWARDS, say with ssh dying
                    // while it waits for `Established`. That failure clears
                    // `is_running` before the close arrives, and the client
                    // would exit saying the host closed the session, in the
                    // one scenario this arm exists for. The latch outlives the
                    // attempt and is cleared by the swap instead.
                    //
                    // With no rebuild in the picture at all, a takeover is
                    // somebody else attaching and today's behaviour stands,
                    // until B4 makes it `Displaced`.
                    if is_takeover(&reason)
                        && self
                            .rebuild
                            .as_ref()
                            .is_some_and(Rebuild::may_have_displaced_us)
                    {
                        takeover_expected = true;
                        continue;
                    }
                    self.drain(out).await?;
                    return exit_code(&reason);
                }
            }

            // Layer 1. Laid out again only when the view actually changed, so
            // an open popup with nothing new costs one comparison per lap.
            let now = Instant::now();
            let view = self.popup_at(now);
            self.sample_quality(now);
            if view != self.shown {
                self.renderer
                    .set_overlay(view.as_ref().map(|v| layout_popup(v, self.size)));
                self.shown = view;
                self.renderer
                    .render(out, self.screen_rx.state())
                    .context("painting the popup")?;
                out.flush().context("flushing the terminal")?;
            }

            // Follow the route if it moved. Inside the loop rather than on a
            // timer of its own: `follow_route` is gated on `Silent` or
            // `Recovering` and paced by `ROUTE_PROBE_EVERY`, so a healthy
            // session reaches this line ten times a second and does nothing
            // but one `matches!`.
            //
            // After the popup, so what describes the silence is already on
            // the screen before anything is done about it -- and the user
            // is told nothing about the rebind, because a rebind that has not
            // restored contact yet is not something the client can honestly
            // report.
            //
            // A moved route is also news for the standby search (spec §3.1):
            // an interface that was absent may now be present.
            if self.follow_route(now)
                && let Some(s) = self.standby.as_mut()
            {
                s.route_moved(now);
            }

            // The `bool` is for the tests, which hold the clock still and ask
            // whether a prod was due. The loop does not care: it prods or it
            // does not, and either way the next lap is the same.
            let _ = self.heartbeat(now);

            // After the popup, for the same reason the route probe is: what
            // explains the silence is on the screen before anything is done
            // about it. After the probe, too -- a rebind is the cheaper
            // of the two ways back and costs no ssh at all.
            self.rebuild_step(now, &outcomes_tx);

            // The standby (spec §3). After the rebuild step, so an attempt
            // started on this very lap already counts as running: nothing is
            // failed over onto while one is (ruling B1). The step decides and
            // the loop acts, so nothing spawned here borrows `self`.
            let rebuild_running = self.rebuild.as_ref().is_some_and(Rebuild::is_running);
            let phase = self.link_state.phase_now();
            let action = self.standby_step(phase, now, rebuild_running);
            match action {
                crate::standby::StandbyAction::Nothing => {}
                crate::standby::StandbyAction::Search { search } => {
                    let primary = self.link.sink.connection().clone();
                    let size = self.size;
                    let s = self.standby.as_ref().expect("it just stepped");
                    let (cfg, admit_for) = (s.cfg.clone(), s.admit_for);
                    let tx = standby_tx.clone();
                    let task = tokio::spawn(async move {
                        // No filter means the primary's own route could not
                        // be read, and a search without one could land on the
                        // primary's path: no search at all is the safe
                        // answer.
                        let event = match admit_for(primary.remote_address()) {
                            None => crate::standby::StandbyEvent::NotFound {
                                search,
                                reason: "the primary's own route could not be read, \
                                         so no search could run safely"
                                    .to_string(),
                            },
                            Some(admit) => {
                                match crate::control::request_standby(primary, size, cfg, admit)
                                    .await
                                {
                                    Ok(e) => crate::standby::StandbyEvent::Found {
                                        search,
                                        e: Box::new(e),
                                    },
                                    Err(e) => crate::standby::StandbyEvent::NotFound {
                                        search,
                                        reason: format!("{e:#}"),
                                    },
                                }
                            }
                        };
                        let _ = tx.send(event).await;
                    });
                    _search_task = Some(crate::attach_exchange::AbortOnDrop(task.abort_handle()));
                }
                crate::standby::StandbyAction::Probe { nonce } => {
                    if let Some(standby) = self.standby.as_ref().and_then(|s| s.connection()) {
                        let tx = standby_tx.clone();
                        let task = tokio::spawn(async move {
                            let answered = crate::control::probe(standby, nonce).await;
                            let _ = tx
                                .send(crate::standby::StandbyEvent::Probed { answered })
                                .await;
                        });
                        _probe_task =
                            Some(crate::attach_exchange::AbortOnDrop(task.abort_handle()));
                    }
                }
                crate::standby::StandbyAction::FailOver => {
                    if self.fail_over(now, out)? {
                        conn = self.link.sink.connection().clone();
                        standby_conn = None;
                        // `take_for_failover` disowned any search running
                        // over the old primary; this stops it.
                        _search_task = None;
                        takeover_expected = false;
                    }
                }
            }

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
            // wakes at least ten times a second.
            deadline = tokio::time::Instant::now() + self.link.sink.pacing_interval();
        }
    }

    /// Follow this machine's route to the host, if it moved.
    ///
    /// Returns whether the session socket was actually swapped.
    ///
    /// **Only while `Silent` or `Recovering`**, per design spec 4.2: a rebind
    /// moves our source port, which invalidates a punched NAT hole, so doing
    /// it to a working path breaks the path in order to test it. Both phases
    /// mean a reply has been owed with none arriving -- the path is already
    /// not working, so there is nothing left to break. `Recovering` cannot be
    /// excluded: it is silence that has lasted past `REBUILD_AFTER`, still on
    /// the same broken path, and the whole reason it never tears the old link
    /// down is that either path -- this probe reviving the old one, or a
    /// rebuilt link -- may win. Stopping the probe at the exact moment
    /// recovery becomes necessary would leave a client that changed networks
    /// mid-outage with neither path available.
    ///
    /// Nothing here may end the session. A machine in the middle of an outage
    /// is exactly where `connect` fails with `ENETUNREACH` and where binding a
    /// fresh socket fails, and this runs during precisely that. Every failure
    /// is "no answer this time"; the next probe asks again. Same rule as "a
    /// send failure must never end a session", applied to the thing most
    /// likely to fail.
    ///
    /// # What seeding the baseline at connection time costs
    ///
    /// The baseline is read once, in [`ClientSession::new`], and after that
    /// only a probe taken while `Silent` can replace it. That is deliberate --
    /// see the comment there for why reading it first inside the outage is
    /// inert -- but it is not free, and the price is worth writing down rather
    /// than rediscovering.
    ///
    /// A route may change perfectly benignly while the link is `Live`: a VPN
    /// comes up, an interface metric shifts, the machine moves between two
    /// networks that both reach the host. Nothing probes on a `Live` lap, so
    /// the baseline is now stale, and it stays stale until an outage. If a
    /// *transient* stall then raises `Silent` -- two seconds of host-side
    /// hesitation, not a moved route -- the first probe of that outage
    /// disagrees with the stale baseline, reads it as a move, and spends a
    /// working NAT hole rebinding a path that was about to recover.
    ///
    /// Accepted, with the alternative stated so the trade is visible: the only
    /// way to keep the baseline fresh is to probe on `Live` laps, which means
    /// a bind/`connect` pair every second for the whole life of every healthy
    /// session, for ever, to catch a case that costs one rebind. One rebind
    /// costs a NAT hole that rungs 2 and 3 re-punch; it does not cost the
    /// connection, because QUIC is identified by connection IDs and the
    /// migration is exactly what this whole path is for. The rebind settles
    /// the new address as the baseline, so the mistake is made once and not
    /// once a second.
    fn follow_route(&mut self, now: Instant) -> bool {
        if !self.link_state.phase_now().is_outage() {
            // The pace belongs to one outage, not to the session. Left set
            // across a return to `Live`, `probed_at` would also swallow the
            // FIRST probe of the next outage whenever that outage began within
            // `ROUTE_PROBE_EVERY` of the previous probe -- and the first probe
            // is the one that matters, because it is what notices a route that
            // has just moved.
            //
            // Deliberate, and worth saying plainly: that skip is not reachable
            // through today's constants. Leaving `Silent` resets the owing, so
            // a second `Silent` costs another `SILENT_AFTER` (2 s), which is
            // longer than `ROUTE_PROBE_EVERY` (1 s) -- the cooldown has always
            // expired by the time the next outage exists. The clear is here so
            // that stays true of the mechanism rather than of an accident of
            // two numbers in different modules, either of which could move
            // without anyone thinking about this line. It costs one store on a
            // path that was already returning.
            self.probed_at = None;
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
    use crate::view::words;
    use oxutrm_client::Marker;
    use oxutrm_host::ssh::SshLauncher;
    use oxutrm_proto::{NatType, Rung};

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

    /// A host and a client joined by a real QUIC connection on loopback.
    async fn pair(shell: &str) -> (HostSession, ClientSession) {
        pair_on("127.0.0.1:0", shell, None).await
    }

    /// A UDP relay the test can blackhole in both directions.
    ///
    /// The in-process pair cannot reproduce a path outage: two live quinn
    /// endpoints on loopback keep ACKing each other whatever the session layer
    /// does, and `SIGSTOP` against a real host does not do it either -- a
    /// stopped process still has a kernel receiving into its socket buffer,
    /// so on resume it drains the backlog and ACKs it, and the client
    /// recovers instantly. Neither is an outage. **Packets have to actually
    /// be dropped**, which is what this does.
    struct Relay {
        addr: std::net::SocketAddr,
        blackhole: Arc<std::sync::atomic::AtomicBool>,
        _task: tokio::task::JoinHandle<()>,
    }

    impl Relay {
        fn blackhole(&self, on: bool) {
            self.blackhole
                .store(on, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Sit between the client and `host_addr`, forwarding both ways until
    /// told to stop.
    async fn relay_to(host_addr: std::net::SocketAddr) -> Relay {
        let sock = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = sock.local_addr().unwrap();
        let blackhole = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&blackhole);
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            // Learned from the first packet through, and kept across a
            // blackhole: the client's address does not change just because
            // the path did.
            let mut client_addr: Option<std::net::SocketAddr> = None;
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    continue;
                };
                if from != host_addr {
                    client_addr = Some(from);
                }
                // Dropped AFTER learning the address, so a blackhole that
                // starts before the first packet still ends up able to
                // forward once it lifts.
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                let to = if from == host_addr {
                    match client_addr {
                        Some(a) => a,
                        None => continue,
                    }
                } else {
                    host_addr
                };
                let _ = sock.send_to(&buf[..n], to).await;
            }
        });
        Relay {
            addr,
            blackhole,
            _task: task,
        }
    }

    /// `pair`, with a blackholeable relay in the middle.
    async fn pair_through_relay(shell: &str) -> (HostSession, ClientSession, Relay) {
        pair_through_relay_sized(shell, size()).await
    }

    /// Big enough for the popup's log to have rows: at the fixtures' 40x10 the
    /// status block fills the whole box.
    const BIG: TermSize = TermSize { cols: 80, rows: 24 };

    /// `pair_through_relay` at `size`, with host and client both at `size`
    /// from the start, so no resize is in flight when a test begins.
    async fn pair_through_relay_sized(
        shell: &str,
        size: TermSize,
    ) -> (HostSession, ClientSession, Relay) {
        let listening = crate::link::fixtures::listening().await;
        let relay = relay_to(listening.addr).await;
        // The client's peer is the RELAY, which is the whole point.
        let (host_link, client_link) = listening.dial("127.0.0.1:0", relay.addr).await;

        let mut host = HostSession::spawn("/bin/sh", size, 200, host_link).unwrap();
        let client = ClientSession::new(size, caps(), client_link, None).unwrap();
        host.term.write_input(shell.as_bytes()).unwrap();
        (host, client, relay)
    }

    /// `rebuild` is what the client would rebuild a lost link through.
    /// `None` for every fixture that is not testing the rebuild loop: these
    /// links are built directly rather than over ssh, so there is no target to
    /// rebuild to and nothing that could stand in for one.
    async fn pair_on(
        client_bind: &str,
        shell: &str,
        rebuild: Option<Rebuild>,
    ) -> (HostSession, ClientSession) {
        pair_on_sized(client_bind, shell, rebuild, size()).await
    }

    /// `pair`, with host and client both at `size` from the start, so no
    /// resize is in flight when a test begins.
    async fn pair_sized(shell: &str, size: TermSize) -> (HostSession, ClientSession) {
        pair_on_sized("127.0.0.1:0", shell, None, size).await
    }

    /// `pair_on`, at `size`.
    async fn pair_on_sized(
        client_bind: &str,
        shell: &str,
        rebuild: Option<Rebuild>,
        size: TermSize,
    ) -> (HostSession, ClientSession) {
        let listening = crate::link::fixtures::listening().await;
        let addr = listening.addr;
        let (host_link, client_link) = listening.dial(client_bind, addr).await;

        let host = HostSession::spawn("/bin/sh", size, 200, host_link).unwrap();
        let client = ClientSession::new(size, caps(), client_link, rebuild).unwrap();

        // The caller decides what the shell runs; `spawn` above starts one, so
        // the script is fed as input instead, which is also how a real session
        // works.
        let mut host = host;
        host.term.write_input(shell.as_bytes()).unwrap();
        (host, client)
    }

    /// The displaced connection is closed, and closed with a reason that says
    /// what happened.
    ///
    /// Without the reason the displaced client reports "the host has stopped
    /// answering", which is false: the host is answering, to somebody else.
    #[tokio::test]
    async fn adopting_a_link_closes_the_old_one_saying_it_was_taken_over() {
        let (mut host, client) = pair("/bin/sh").await;
        let (_host2, client2) = pair("/bin/sh").await;

        let old = client.link.sink.connection().clone();
        host.adopt(client2.link, TermSize { cols: 80, rows: 24 })
            .expect("adopting a second attach");

        let reason = old.closed().await;
        let quinn::ConnectionError::ApplicationClosed(closed) = reason else {
            panic!(
                "the displaced connection ended with {reason:?}, not an \
                    application close naming the takeover"
            );
        };
        assert_eq!(
            closed.reason.as_ref(),
            TAKEN_OVER,
            "displaced with the wrong reason; the client cannot tell a takeover \
             from silence"
        );
    }

    /// The newcomer's terminal is usually a different size, and the shell has
    /// to land at ITS geometry.
    ///
    /// `adopt` used to write `self.size = size` directly. `self.size` means
    /// "the size the terminal currently IS", and the only other writer,
    /// [`HostSession::resize`], moves the emulator and the pty first. Writing
    /// the field alone left both at the FIRST client's geometry — and nothing
    /// downstream healed it, because `adopt` seeds `input_rx` with the very
    /// value it wrote, so `turn_at`'s `wanted != self.size` self-heal is false
    /// on that turn and on every turn after it. The client only re-sends a
    /// size on a window change, so the shell stayed wrong until the user
    /// resized their window by hand.
    ///
    /// Asserted on the screen the host actually SHIPS, not on `self.size`.
    /// The blank placeholder `adopt` installs is built from a size too, so a
    /// field assertion — or one taken before a turn — passes happily with the
    /// emulator still at the old geometry. One turn later the sender holds
    /// `term.snapshot()`, which can only be the emulator's own dimensions.
    #[tokio::test]
    async fn adopting_a_link_resizes_the_shell_to_the_newcomers_terminal() {
        let (mut host, client) = pair("/bin/sh").await;
        assert_eq!(
            (host.screen().rows, host.screen().cols),
            (size().rows, size().cols),
            "the fixture did not start where this test thinks it did"
        );
        drop(client);

        let newcomer = TermSize {
            cols: 132,
            rows: 43,
        };
        let (_host2, client2) = pair("/bin/sh").await;
        host.adopt(client2.link_take(), newcomer)
            .expect("adopting a second attach");

        // `screen_stale` is set by `adopt`, so this turn snapshots the
        // emulator whether or not the shell happened to write anything.
        host.turn_at(Instant::now(), None)
            .expect("turn after adopting");

        assert_eq!(
            (host.screen().rows, host.screen().cols),
            (newcomer.rows, newcomer.cols),
            "the shell is still at the previous client's geometry: the \
             newcomer asked for {}x{} and the authoritative screen is {}x{}",
            newcomer.cols,
            newcomer.rows,
            host.screen().cols,
            host.screen().rows
        );
    }

    /// §8.5: the first frame of a new attach is a FULL state, not a diff against
    /// a base the new client has never seen — and it has to carry the shell's
    /// ACTUAL screen, not the blank one `adopt` installs as a placeholder.
    ///
    /// `Turn.sent` is a [`SendOutcome`], which reports how a frame went out but
    /// not what was in it, so the frame is read back off the wire instead: the
    /// newcomer's link is the client-side handle of the connection `pair`
    /// built for `host2`, so whatever the host sends after adopting it arrives
    /// at `host2`'s own [`crate::link::Link::source`], which has been
    /// listening in the background since `pair` returned. That is "the frame
    /// that actually arrives" — a side effect, not a return value.
    #[tokio::test]
    async fn the_first_frame_after_adopting_is_a_full_state() {
        let (mut host, client) = pair("/bin/sh").await;
        // Let the first client get a real screen, so the emulator is NOT blank
        // and a diff-from-current would be visibly different from a full state.
        host.turn_at(Instant::now(), None).expect("first turn");

        // Settle the pty completely before adopting, polling it DIRECTLY
        // rather than through `turn()` -- nothing is driving the first
        // client's own loop here, so its ack never arrives and `turn()`
        // would find something owed forever, which is a different kind of
        // "not quiet" than the one this guards against. Two consecutive
        // quiet polls, 50 ms apart. Without this, an async straggler from the
        // shell's own startup could still land on the turn taken after
        // adopting and make `moved` true on its own -- which would carry the
        // real screen across whether or not `screen_stale` did its job, and
        // the assertion below would pass for the wrong reason.
        let mut quiet = 0;
        let settled_by = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < settled_by && quiet < 2 {
            if host.term.poll().expect("polling the pty") {
                quiet = 0;
            } else {
                quiet += 1;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            quiet, 2,
            "the shell never went quiet; this test cannot tell \
                    `screen_stale` apart from ordinary `moved` freshness"
        );
        drop(client);

        let (mut host2, client2) = pair("/bin/sh").await;
        let newcomer = client2;
        host.adopt(newcomer.link_take(), TermSize { cols: 80, rows: 24 })
            .expect("adopting");

        let turn = host
            .turn_at(Instant::now(), None)
            .expect("turn after adopting");
        assert!(
            turn.sent.is_some(),
            "a frame is owed to a client that just arrived"
        );

        let frame = host2.link.source.recv().await.expect(
            "the frame the host just sent, arriving at the other end \
                     of the connection adopt() installed",
        );
        assert_eq!(
            frame.from_state, 0,
            "the newcomer was sent a diff against state {} it has never seen; \
             §8.5 requires a full state on the first datagram of an attach",
            frame.from_state
        );

        // Not just full-state-SHAPED: a full state built from the still-blank
        // placeholder `adopt` installs would satisfy the assertion above while
        // shipping nothing real. This is exactly what `screen_stale` exists to
        // prevent — see its doc comment.
        let mut check = Receiver::new(ScreenState::blank(10, 40).expect("blank"));
        check
            .on_frame(&frame)
            .expect("applying the newcomer's first frame");
        assert_ne!(
            check.state().cells,
            ScreenState::blank(check.state().rows, check.state().cols)
                .expect("blank")
                .cells,
            "the newcomer's first screen was blank; screen_stale should have \
             forced a snapshot of the shell that kept running while nobody \
             was attached"
        );
    }

    /// EXPERIMENT, not a guard: how long does recovery take after a real
    /// blackout, as a function of how long the blackout lasted?
    ///
    /// Prints a table. Run it with
    /// `cargo test -j4 --bin oxutrm blackout_recovery_curve -- --nocapture
    /// --ignored --test-threads=1`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "experiment; minutes of real wall clock"]
    async fn blackout_recovery_curve() {
        for secs in [15u64, 60, 150, 300] {
            let (mut host, mut client, relay) = pair_through_relay("/bin/sh").await;
            let mut out = Vec::new();

            // Get to a genuinely synced session first, or the outage starts
            // from a state that was never healthy.
            let warm = tokio::time::Instant::now() + Duration::from_secs(2);
            while tokio::time::Instant::now() < warm {
                let _ = host.turn();
                let _ = client.heartbeat(Instant::now());
                let _ = client.turn(&[], &mut out);
                let _ = client.outage_at(Instant::now());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let before = client.link.sink.connection().stats().path.sent_packets;
            // The session must be HEALTHY before the outage, or the rung
            // measures a session that was never up.
            let healthy_first = !client.outage_at(Instant::now());

            relay.blackhole(true);
            let dark = tokio::time::Instant::now() + Duration::from_secs(secs);
            while tokio::time::Instant::now() < dark {
                let _ = host.turn();
                // `run_on` beats every lap; without this the client is never
                // OWED an answer, never goes `Silent`, and the recovery check
                // below passes against a popup that never appeared. That is
                // exactly the "guard that cannot fail" this project keeps
                // producing, and it produced one here.
                let _ = client.heartbeat(Instant::now());
                let _ = client.turn(&[], &mut out);
                let _ = client.outage_at(Instant::now());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let during = client.link.sink.connection().stats().path.sent_packets;
            // The discriminating observable: was the box actually UP when the
            // path came back? If not, this rung measured nothing.
            let went_silent = client.outage_at(Instant::now());

            // The path is perfect again from this instant.
            relay.blackhole(false);
            let restored = tokio::time::Instant::now();
            let give_up = restored + Duration::from_secs(400);
            let mut recovered: Option<Duration> = None;
            while tokio::time::Instant::now() < give_up {
                let _ = host.turn();
                let _ = client.heartbeat(Instant::now());
                let _ = client.turn(&[], &mut out);
                if !client.outage_at(Instant::now()) {
                    recovered = Some(restored.elapsed());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let after = client.link.sink.connection().stats().path.sent_packets;

            let verdict = match recovered {
                Some(d) => format!("recovered in {:>7.2}s", d.as_secs_f64()),
                None => "NO RECOVERY in 400s".to_string(),
            };
            println!(
                "blackout {secs:>4}s -> {verdict} | healthy_first={healthy_first} \
                 went_silent={went_silent} | quinn packets: {before} -> {during} \
                 (dark) -> {after}"
            );
            assert!(
                healthy_first && went_silent,
                "rung {secs}s measured nothing: healthy_first={healthy_first} \
                 went_silent={went_silent} -- the popup must be UP when the path \
                 returns or the recovery check cannot fail"
            );
        }
    }

    /// Does a rebind rescue a backed-off connection?
    ///
    /// The candidate cheap fix for the blackout bug is "rebind while `Silent`,
    /// not only when the route IP moved" -- the machinery already exists in
    /// `follow_route`. This asks whether it would actually help, by rebinding
    /// right when the path comes back and comparing against
    /// `blackout_recovery_curve`'s number for the same blackout length.
    ///
    /// `cargo test -j4 --bin oxutrm does_a_rebind_rescue -- --nocapture
    /// --ignored --test-threads=1`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "experiment; minutes of real wall clock"]
    async fn does_a_rebind_rescue_a_backed_off_connection() {
        for secs in [60u64, 150] {
            let (mut host, mut client, relay) = pair_through_relay("/bin/sh").await;
            let mut out = Vec::new();

            let warm = tokio::time::Instant::now() + Duration::from_secs(2);
            while tokio::time::Instant::now() < warm {
                let _ = host.turn();
                let _ = client.heartbeat(Instant::now());
                let _ = client.turn(&[], &mut out);
                let _ = client.outage_at(Instant::now());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let healthy_first = !client.outage_at(Instant::now());

            relay.blackhole(true);
            let dark = tokio::time::Instant::now() + Duration::from_secs(secs);
            while tokio::time::Instant::now() < dark {
                let _ = host.turn();
                let _ = client.heartbeat(Instant::now());
                let _ = client.turn(&[], &mut out);
                let _ = client.outage_at(Instant::now());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let went_silent = client.outage_at(Instant::now());

            relay.blackhole(false);
            // THE INTERVENTION: a fresh socket, exactly as `follow_route`
            // builds one, the moment the path is back.
            let cfg = oxutrm_net::NetConfig::default();
            let bound = oxutrm_net::bind_socket(&cfg).expect("a fresh socket");
            let socket = crate::ladder::adopt(bound).expect("adopting it");
            let rebound = client.rebind(socket).is_ok();

            let restored = tokio::time::Instant::now();
            let give_up = restored + Duration::from_secs(400);
            let mut recovered: Option<Duration> = None;
            while tokio::time::Instant::now() < give_up {
                let _ = host.turn();
                let _ = client.heartbeat(Instant::now());
                let _ = client.turn(&[], &mut out);
                if !client.outage_at(Instant::now()) {
                    recovered = Some(restored.elapsed());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let verdict = match recovered {
                Some(d) => format!("recovered in {:>7.2}s", d.as_secs_f64()),
                None => "NO RECOVERY in 400s".to_string(),
            };
            println!(
                "REBIND blackout {secs:>4}s -> {verdict} | rebound={rebound} \
                 healthy_first={healthy_first} went_silent={went_silent}"
            );
            assert!(
                healthy_first && went_silent && rebound,
                "rung {secs}s measured nothing"
            );
        }
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
        let (mut host, mut client) = pair_on("127.0.0.1:0", "", None).await;
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
        let (host_link, client_link) = crate::link::fixtures::link_pair().await;

        let mut host = HostSession::spawn("/bin/sh", big, 200, host_link).unwrap();
        let mut client = ClientSession::new(big, caps(), client_link, None).unwrap();

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
        let (mut host, mut client) =
            pair_on("127.0.0.1:0", "printf 'before-roam\r\n'\n", None).await;
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
    ///
    /// The spin is bounded by the CPU clock and NOT by the wall clock, which
    /// is the whole difficulty here. A thread only accumulates 300 ms of CPU
    /// in 300 ms of wall time when it has a core to itself; under a loaded
    /// `make check` -- four test threads, on a box that is shared anyway --
    /// it is descheduled, the wall clock runs on regardless, and the CPU
    /// figure a wall-bounded spin produced fell straight through any floor
    /// worth asserting. Measured on a ten-core machine: with 20 spinners this
    /// failed a third of the time, with 40 it failed every single run, while
    /// the instrument it exists to guard was perfectly healthy throughout.
    /// Spinning until the CPU clock ITSELF reads `SPUN_TARGET` costs more
    /// wall time under load and the same CPU time, which is precisely the
    /// property being measured.
    ///
    /// `WALL_LIMIT` is an escape hatch and not a deadline: a clock stuck at
    /// zero is the very failure this test exists to catch, and without a way
    /// out the loop would hang for ever rather than report it. It sits far
    /// past any real scheduling delay, so reaching it means the instrument is
    /// broken -- which is then what the message says.
    #[test]
    fn the_cpu_clock_measures_work_and_not_wall_time() {
        /// Enough CPU to be unambiguous, and reached in well under a second
        /// of wall time on an idle core.
        const SPUN_TARGET: u64 = 200;
        /// Only a broken clock gets here.
        const WALL_LIMIT: Duration = Duration::from_secs(60);
        /// Work per clock reading, so the loop stays a spin rather than a
        /// benchmark of `clock_gettime`.
        const BATCH: u32 = 100_000;

        let a = thread_cpu_millis();
        std::thread::sleep(Duration::from_millis(300));
        let slept = thread_cpu_millis() - a;

        let b = thread_cpu_millis();
        let start = Instant::now();
        let mut x: u64 = 0;
        let mut spun = 0;
        while spun < SPUN_TARGET && start.elapsed() < WALL_LIMIT {
            for _ in 0..BATCH {
                x = x.wrapping_add(1);
            }
            spun = thread_cpu_millis() - b;
        }

        assert!(x > 0, "the spin was optimised away");
        // Unchanged, and still the assertion that catches a clock reading
        // WALL time: 300 ms of sleeping costs a few microseconds of CPU and
        // 300 ms of wall.
        assert!(slept < 50, "sleeping cost {slept} ms of CPU");
        assert!(
            spun >= SPUN_TARGET,
            "spinning gave up after {:?} of wall time with only {spun} ms of \
             CPU measured, so the clock does not advance with work",
            start.elapsed()
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
    /// the link, so they belong in the popup.
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
            "the count did not reach the popup"
        );
    }

    #[tokio::test]
    async fn silence_raises_the_popup_and_a_frame_ends_the_outage() {
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
        assert!(session.popup_at(t + Duration::from_secs(1)).is_none());

        let v = session
            .popup_at(t + Duration::from_secs(3))
            .expect("no popup after three seconds of silence");
        assert_eq!(v.marker, Marker::Silent);

        session.note_heard(t + Duration::from_secs(4));
        assert!(!session.outage_at(t + Duration::from_secs(4)));
    }

    #[tokio::test]
    async fn the_popup_says_how_long_the_host_has_been_silent() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        // The clock's origin, pinned to the test's: see the test above.
        session.note_heard(t);
        session.note_sent(t);
        // And the lap the owing begins on, which the grace period runs from.
        assert!(session.popup_at(t).is_none());

        let v = session.popup_at(t + Duration::from_secs(6)).unwrap();
        let shown = words(&v);

        assert!(
            shown.contains("silent for 6s"),
            "no silence duration: {shown}"
        );
        assert_claims_nothing_it_cannot_see(&shown);
    }

    /// Someone who closed the outage popup to type blind cannot tell "kept"
    /// from "discarded" until the question appears -- and `q`, which the
    /// popup offers, ends the session before it ever does. Somebody who
    /// opened it again and quit would leave believing their typing had been
    /// thrown away, unless it says otherwise.
    #[tokio::test]
    async fn the_reopened_popup_says_that_blind_typing_is_being_kept() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        assert!(session.popup_at(t).is_none());

        let bare = session
            .popup_at(t + Duration::from_secs(3))
            .expect("no popup after three seconds of silence");
        assert!(
            !words(&bare).contains("kept"),
            "the popup talks about a buffer before anything was typed: {}",
            words(&bare)
        );

        let mut out = Vec::new();
        session.route_keys(&[ESCAPE], &mut out).unwrap();
        session.route_keys(b"make test\r", &mut out).unwrap();
        session.route_keys(&[CTRL_BACKSLASH], &mut out).unwrap();

        let shown = words(
            &session
                .popup_at(t + Duration::from_secs(5))
                .expect("Ctrl-\\ did not open the popup again"),
        );
        assert!(
            shown.contains("10 bytes"),
            "the popup does not say how much is being kept: {shown}"
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
        assert!(session.popup_at(t).is_none());
        assert!(session.popup_at(t + Duration::from_secs(3)).is_some());
        let mut out = Vec::new();
        session.route_keys(&[ESCAPE], &mut out).unwrap();

        session
            .route_keys(&vec![b'x'; crate::linkstate::MAX_HELD], &mut out)
            .unwrap();
        session.route_keys(&[CTRL_BACKSLASH], &mut out).unwrap();

        let shown = words(
            &session
                .popup_at(t + Duration::from_secs(5))
                .expect("Ctrl-\\ did not open the popup again"),
        );
        assert!(
            shown.contains("full"),
            "the buffer stopped accepting keystrokes and the popup did not \
             say so: {shown}"
        );
        assert_claims_nothing_it_cannot_see(&shown);
    }

    /// Every number in the popup that moves by itself is in whole seconds,
    /// so the view changes only when a shown whole-second value (or another
    /// shown fact) changes, and the loop's comparison spares every other
    /// repaint. Ages and countdowns can turn over on different sub-second
    /// phases; here they are pinned to one, so the view holds for a second.
    /// A view carrying quinn's raw packet counters would change on every lap
    /// of an outage -- up to 125 times a second -- in a box whose whole job
    /// is to be read.
    #[tokio::test]
    async fn an_open_popup_is_the_same_view_for_a_second_at_a_time() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        // The link's age ("up Ns") counts from when the session was built,
        // which is a handshake after `t`; pinned onto `t` like `last_heard`,
        // so the whole view turns over on the same second boundary.
        session.quality = Quality::new(t);
        assert!(session.popup_at(t).is_none());

        let first = session
            .popup_at(t + Duration::from_secs(3))
            .expect("no popup after three seconds of silence");

        // Something real moves between the two laps, as it does in an
        // outage: the client sends, and quinn's counters climb.
        let sent = |s: &ClientSession| s.link.sink.connection().stats().path.sent_packets;
        let before = sent(&session);
        let mut out = Vec::new();
        session.turn(b"x", &mut out).expect("a turn that sends");
        // Bounded well inside a second: the log's ages are read off the wall
        // clock, and a wait that crossed a real second could move one and
        // fail the comparison below for a reason it is not about.
        let deadline = Instant::now() + Duration::from_millis(500);
        while sent(&session) == before {
            assert!(
                Instant::now() < deadline,
                "nothing was sent between the laps"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
            session.popup_at(t + Duration::from_millis(3_900)),
            Some(first.clone()),
            "the view changed within the same second"
        );
        let later = session
            .popup_at(t + Duration::from_secs(4))
            .expect("the popup vanished");
        assert_ne!(later, first, "the silence counter never moved");
        assert!(words(&later).contains("silent for 4s"), "{}", words(&later));
    }

    /// A change of phase is the thing the popup exists to report, and
    /// waiting to report it would leave the user looking at a popup that is
    /// out of date.
    #[tokio::test]
    async fn a_change_of_phase_is_shown_the_lap_it_happens() {
        let (_host, mut session) = with_popup().await;
        let mut out = Vec::new();
        session.route_keys(&[ESCAPE], &mut out).unwrap();
        session.route_keys(b"make test\r", &mut out).unwrap();

        // Moments after the outage's popup was last built, the host answers.
        let now = std::time::Instant::now();
        session.note_heard(now);
        let v = session
            .popup_at(now)
            .expect("the question about the held input never appeared");

        assert!(
            v.held
                .first()
                .is_some_and(|l| l.contains("answering again")),
            "the popup still reports the outage the host has already ended: {:?}",
            v.held
        );
    }

    /// The other popup, and the one that went unguarded: the check above
    /// reads only the `Silent` popup, which is how "reconnected" survived in
    /// a phase where nothing reconnects.
    ///
    /// Reached without sleeping, along the path the loop actually takes: the
    /// popup is up, the user closes it and types blind, and then the host
    /// answers.
    #[tokio::test]
    async fn the_confirming_popup_states_only_what_the_client_can_observe() {
        let (_host, mut session) = with_popup().await;
        let mut out = Vec::new();
        session.route_keys(&[ESCAPE], &mut out).unwrap();
        session.route_keys(b"make test\r", &mut out).unwrap();

        // A frame arrives. That the host is answering again is the whole of
        // what the client learns from it -- not that anything reconnected,
        // because the connection never dropped, and not that the shell is
        // well, which no frame can say.
        let now = std::time::Instant::now();
        session.note_heard(now);

        let v = session
            .popup_at(now)
            .expect("the host answered with input held, and nothing was asked");
        let shown = words(&v);

        assert!(
            shown.contains("10 bytes"),
            "this is not the popup that asks about the held input: {shown}"
        );
        assert_claims_nothing_it_cannot_see(&shown);
    }

    /// The only path by which a user ever sees `Phase::Recovering`.
    /// `view.rs`'s test for the recovering rows calls `build` with numbers
    /// already computed; nothing there exercises `popup_at`'s own derivation
    /// of them -- `now.duration_since(self.link_state.last_heard())`
    /// for the quiet count, `next_try.saturating_duration_since(now)` for the
    /// countdown -- so this drives the real wiring through `evaluate`
    /// instead.
    ///
    /// Not run through `assert_claims_nothing_it_cannot_see`: that guard
    /// forbids "reconnect"/"retry" because nothing reconnects while `Silent`.
    /// `Recovering` is exactly the mechanism phase 2 adds, so the word
    /// belongs here and the guard does not apply.
    #[tokio::test]
    async fn the_recovering_section_reports_the_wired_numbers() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        assert!(session.popup_at(t).is_none());
        let _ = session.popup_at(t + Duration::from_secs(3));
        assert!(
            matches!(session.link_state.phase_now(), Phase::Silent { .. }),
            "fixture did not reach Silent: {:?}",
            session.link_state.phase_now()
        );

        let n = session
            .popup_at(t + crate::linkstate::REBUILD_AFTER)
            .expect("no popup while Recovering");
        assert!(
            matches!(session.link_state.phase_now(), Phase::Recovering { .. }),
            "fixture did not reach Recovering: {:?}",
            session.link_state.phase_now()
        );

        assert_eq!(n.marker, Marker::Recovering);
        let shown = n.recovering.join(" | ");
        // `last_heard` is `t`; this call lands exactly `REBUILD_AFTER` later,
        // so a quiet count read from anywhere other than `last_heard` (say,
        // `owed_since`, which this session never set to `t`) would not say
        // 20s here.
        assert!(shown.contains("host quiet for 20s"), "{shown}");
        // Attempt 0 internally (the first attempt), rendered as 1.
        assert!(shown.contains("reconnect attempt 1"), "{shown}");
        // `next_try` was set to exactly `now` on entering `Recovering`, and
        // this call is that same `now` -- so the countdown, read through
        // `saturating_duration_since`, must be zero rather than negative or
        // panicking.
        assert!(shown.contains("next try in 0s"), "{shown}");
    }

    // ---- the rebuild loop --------------------------------------------------

    /// STUN-free, like every other test in this repo: a test that reaches STUN
    /// makes every timing in it non-deterministic.
    fn stunless() -> oxutrm_net::NetConfig {
        oxutrm_net::NetConfig {
            stun_servers: vec![],
            enable_port_mapping: false,
            enable_birthday: false,
            ..Default::default()
        }
    }

    /// An `ssh` that records its own pid and then never says anything.
    ///
    /// An attempt against this is permanently in flight, which is what the
    /// cancellation test needs to have something to cancel. `exec` matters:
    /// without it the pid recorded is the shell's and `sleep` is a child of it
    /// that would outlive the kill.
    fn hanging_ssh(dir: &std::path::Path, pidfile: &std::path::Path) -> SshLauncher {
        use std::os::unix::fs::PermissionsExt as _;

        let script = dir.join("hanging-ssh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 300\n",
                pidfile.display()
            ),
        )
        .expect("writing the fake ssh");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("making the fake ssh executable");
        SshLauncher::command(&script)
    }

    /// Wait until the fake ssh has recorded its pid, and answer with it.
    ///
    /// A completed observation rather than a sleep: the file existing IS the
    /// attempt having reached the point of spawning ssh.
    async fn wait_for_pid(pidfile: &std::path::Path) -> u32 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(text) = std::fs::read_to_string(pidfile)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                return pid;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the attempt never started an ssh"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Is `pid` a process that is still running?
    ///
    /// A zombie counts as gone, and that is the whole reason this asks `ps`
    /// rather than `kill -0`: tokio kills the child on drop and reaps it from
    /// its orphan queue afterwards, so between those two moments the pid still
    /// exists and `kill -0` still succeeds.
    fn process_is_alive(pid: u32) -> bool {
        let Ok(out) = std::process::Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
        else {
            return false;
        };
        let state = String::from_utf8_lossy(&out.stdout);
        let state = state.trim();
        !state.is_empty() && !state.starts_with('Z')
    }

    async fn assert_gone(pid: u32, what: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while process_is_alive(pid) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{what} (pid {pid}) is still running"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Drive a fresh session into `Recovering` on a clock of our own.
    ///
    /// The same build-up `the_recovering_section_reports_the_wired_numbers`
    /// uses, and it has to be a build-up: `evaluate` escalates one stage per
    /// call, so a single jump to `REBUILD_AFTER` reaches `Silent` and stops
    /// there. Returns the instant the phase was entered at.
    fn drive_to_recovering(session: &mut ClientSession) -> Instant {
        let t = Instant::now();
        session.note_heard(t);
        session.note_sent(t);
        assert!(!session.outage_at(t));
        let _ = session.outage_at(t + Duration::from_secs(3));
        let entered = t + crate::linkstate::REBUILD_AFTER;
        let _ = session.outage_at(entered);
        assert!(
            matches!(session.link_state.phase_now(), Phase::Recovering { .. }),
            "the fixture did not reach Recovering: {:?}",
            session.link_state.phase_now()
        );
        entered
    }

    /// Whichever path revives first wins, and the loser is stopped.
    ///
    /// The old link is held throughout a rebuild precisely so that it may come
    /// back on its own (spec §5.3). When it does, the attempt in flight is not
    /// merely redundant: left running it can still land, and then it swaps the
    /// transport out from under a session that had already recovered.
    ///
    /// The assertion that matters is **the ssh is dead**, not that the handle
    /// was let go of. Dropping a `JoinHandle` detaches its task rather than
    /// stopping it, so a `cancel` written as `self.in_flight.take()` — no
    /// `abort` — would leave `is_running()` false and the attempt, its ssh and
    /// its eventual swap all very much alive. Only the process check tells
    /// those two apart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_frame_on_the_old_link_stops_the_attempt_in_flight() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let pidfile = dir.path().join("ssh.pid");
        let rebuild = Rebuild::new("bastion.example.net".to_owned(), "f0".repeat(16))
            .via(hanging_ssh(dir.path(), &pidfile), stunless());
        let (mut host, mut session) = pair_on("127.0.0.1:0", "/bin/sh", Some(rebuild)).await;

        let entered = drive_to_recovering(&mut session);

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        session.rebuild_step(entered, &tx);
        assert!(
            session
                .rebuild
                .as_ref()
                .expect("the fixture built a rebuild")
                .is_running(),
            "no attempt was started, so there is nothing for this test to \
             cancel and every assertion below would pass for free"
        );
        let ssh = wait_for_pid(&pidfile).await;

        // The old link answers. This is the ordinary scavenging path: the
        // frame lands in the channel and `turn` picks it up.
        let mut out = Vec::new();
        host.turn().expect("the host takes a turn");
        wait_for_frame(&mut session).await;
        session.turn(&[], &mut out).expect("a pacing lap");
        assert_eq!(
            session.link_state.phase_now(),
            Phase::Live,
            "the frame did not revive the link, so what follows would be \
             testing nothing"
        );

        // Set by `begin`, and the thing this test's sibling assertion is
        // about: without it being true HERE, the assertion after the step
        // would be reading a latch that was never raised.
        assert!(
            session
                .rebuild
                .as_ref()
                .expect("the rebuild is still there")
                .may_have_displaced_us(),
            "the attempt in flight never raised the displacing latch, so \
             clearing it below would prove nothing"
        );

        // The rest of that same lap of `run_on`.
        session.rebuild_step(Instant::now(), &tx);

        assert!(
            !session
                .rebuild
                .as_ref()
                .expect("the rebuild is still there")
                .is_running(),
            "the client is still rebuilding a link that came back by itself"
        );
        // The outage is over, so the rebuild's claim on the next `TAKEN_OVER`
        // is over with it. Left standing, a third-party takeover any time
        // later in this session is attributed to a rebuild that ended here:
        // the close is swallowed, `conn.closed()` is disabled for good, and
        // the client sits on a dead connection showing a screen that will
        // never change -- then goes `Silent`, `Recovering`, and silently takes
        // the session back off whoever attached.
        assert!(
            !session
                .rebuild
                .as_ref()
                .expect("the rebuild is still there")
                .may_have_displaced_us(),
            "the displacing latch outlived the outage that raised it"
        );
        assert_gone(ssh, "the abandoned attempt's ssh").await;
    }

    /// A rebuilt link is not immediately rebuilt again.
    ///
    /// The swap deliberately leaves the phase at `Recovering`, because only a
    /// frame may say the host is answering. The trap that follows from it: the
    /// phase still carries the `next_try` set when the attempt that just
    /// landed STARTED, and any real attempt -- ssh, ICE, a QUIC handshake --
    /// outlives that by seconds. So the very same lap of `run_on` that swapped
    /// the link would go on to build another one, that one can land too and
    /// displace the first, and the phase is STILL `Recovering`: a loop that
    /// displaces itself for as long as no frame gets through, which is exactly
    /// the condition it was built to survive.
    ///
    /// The third assertion is what stops the fix from being "never rebuild
    /// again". A guard that simply latched off would satisfy the first two.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_landed_swap_does_not_immediately_start_another_attempt() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let pidfile = dir.path().join("ssh.pid");
        let rebuild = Rebuild::new("bastion.example.net".to_owned(), "f0".repeat(16))
            .via(hanging_ssh(dir.path(), &pidfile), stunless());
        let (_host, mut session) = pair_on("127.0.0.1:0", "/bin/sh", Some(rebuild)).await;
        let (_host2, newcomer) = pair("/bin/sh").await;

        let entered = drive_to_recovering(&mut session);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);

        // One real attempt, so the phase reaches the swap carrying the
        // schedule an attempt leaves behind rather than a fresh one.
        session.rebuild_step(entered, &tx);
        assert!(
            session.rebuild.as_ref().expect("a rebuild").is_running(),
            "the fixture started no attempt"
        );
        let ssh = wait_for_pid(&pidfile).await;

        // It lands, five seconds later -- comfortably past the one second
        // `begin_attempt` scheduled, as any real ssh handshake is. `cancel`
        // rather than the loop's own `finished`, because a real attempt's task
        // has ENDED by the time its outcome is read and this fake's never
        // would; the phase state either one leaves behind is identical.
        let landed = entered + Duration::from_secs(5);
        session.rebuild.as_mut().expect("a rebuild").cancel();
        session
            .swap_in(newcomer.link_take(), landed)
            .expect("swapping in the rebuilt link");
        assert_gone(ssh, "the landed attempt's ssh").await;

        // The rest of that same lap.
        session.rebuild_step(landed, &tx);
        assert!(
            !session.rebuild.as_ref().expect("a rebuild").is_running(),
            "the lap that swapped a rebuilt link in went straight on to build \
             another one, which can land and displace it"
        );

        match session.link_state.phase_now() {
            Phase::Recovering { attempt, next_try } => {
                assert_eq!(
                    attempt, 0,
                    "the popup will keep counting attempts up over a link \
                     that has already been rebuilt"
                );
                assert_eq!(next_try, landed + crate::linkstate::backoff(0));
            }
            other => {
                panic!("the swap left Recovering by itself; only a frame may do that: {other:?}")
            }
        }

        // Delayed, not disabled: a rebuilt link that never produces a frame
        // has to be rebuilt again in its turn.
        session.rebuild_step(
            landed + crate::linkstate::backoff(0) + Duration::from_millis(1),
            &tx,
        );
        assert!(
            session.rebuild.as_ref().expect("a rebuild").is_running(),
            "the rebuild loop stopped for good at the first swap, so a link \
             that landed and then said nothing is never rebuilt"
        );
    }

    /// A landed attempt replaces the transport and starts the sync state over.
    ///
    /// Design spec §8.5: both ends reset their sequence counters at every
    /// attach and the host's first datagram is a full state. The host has
    /// already done its half — the attempt reached it as an ordinary attach,
    /// so `HostSession::adopt` ran — and a client that kept its old counters
    /// would reject that first frame as a base mismatch and freeze on the
    /// screen it had.
    ///
    /// Driven with a link built by the fixture rather than by a real rebuild:
    /// what is under test is the swap, and `establish` reaching a real host is
    /// `connect.rs`'s duplex-paired test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_landed_rebuild_swaps_the_link_and_starts_the_sync_state_over() {
        let (mut host, mut session) = pair("/bin/sh").await;
        let (_host2, newcomer) = pair("/bin/sh").await;

        // Get both counters off their starting values, so "back to 1" is a
        // claim about the reset rather than about a session that never moved.
        let mut out = Vec::new();
        session.turn(b"hello", &mut out).expect("a keystroke");
        host.turn().expect("the host takes a turn");
        wait_for_frame(&mut session).await;
        session.turn(&[], &mut out).expect("a pacing lap");
        assert!(
            session.input_tx.current().seq() > 1,
            "the fixture never moved the input counter"
        );
        assert!(
            session.screen_rx.state().rows > 0,
            "the fixture has no screen to lose"
        );

        // The HOST's handle of the link about to be replaced. Watched from
        // there and not from this side: `closed()` on the connection we
        // ourselves closed answers `LocallyClosed` and says nothing about what
        // went out, so a close whose reason never reached the peer would look
        // identical.
        let displaced = host.link.sink.connection().clone();
        session
            .swap_in(newcomer.link_take(), Instant::now())
            .expect("swapping in the rebuilt link");

        assert_eq!(
            session.input_tx.current().seq(),
            1,
            "the input counter carried across the swap, so the host -- which \
             reset its receiver to 1 when it adopted -- will reject \
             everything typed from here"
        );
        assert_eq!(
            session.screen_rx.ack(),
            0,
            "the client still acknowledges a screen state the rebuilt link's \
             host has never sent, so the host's first full state looks like \
             an old one"
        );

        // The connection this replaced is closed, and closed with a reason:
        // the same discipline `adopt` follows on the host's side.
        let reason = tokio::time::timeout(Duration::from_secs(5), displaced.closed())
            .await
            .expect("the replaced connection was never closed");
        let quinn::ConnectionError::ApplicationClosed(closed) = reason else {
            panic!("the replaced connection ended with {reason:?}, not an application close");
        };
        assert_eq!(closed.reason.as_ref(), REBUILT);
    }

    /// A completed standby attach, as the listener hands it to the loop.
    fn standby_attached(link: Link) -> crate::attach_exchange::Attached {
        crate::attach_exchange::Attached {
            link,
            path: path_of(Rung::StunPunch, 30, 1400, 4, NatType::Unknown),
            client_size: size(),
            role: crate::control::Role::Standby,
        }
    }

    /// The application close a connection ended with, as its peer saw it,
    /// within five seconds.
    async fn closed_as(conn: &quinn::Connection) -> quinn::ConnectionError {
        tokio::time::timeout(Duration::from_secs(5), conn.closed())
            .await
            .expect("the connection was never closed")
    }

    fn closed_with(reason: &quinn::ConnectionError, phrase: &[u8]) -> bool {
        matches!(
            reason,
            quinn::ConnectionError::ApplicationClosed(c) if c.reason.as_ref() == phrase
        )
    }

    /// A standby is parked, not adopted: the client did not ask to switch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_standby_is_parked_and_the_primary_is_left_alone() {
        let (mut host, _client) = pair("").await;
        let (standby_host, _standby_client) = crate::link::fixtures::link_pair().await;
        let primary = host.link.sink.connection().stable_id();
        let parked = standby_host.sink.connection().stable_id();
        let mut slot = None;

        host.on_attached(standby_attached(standby_host), &mut slot)
            .unwrap();

        assert_eq!(
            slot.as_ref()
                .map(|l: &Link| l.sink.connection().stable_id()),
            Some(parked),
            "the standby was not parked"
        );
        assert_eq!(
            host.link.sink.connection().stable_id(),
            primary,
            "parking a standby swapped the primary"
        );
        assert!(
            host.link.sink.connection().close_reason().is_none(),
            "parking a standby closed the primary"
        );
    }

    /// One slot: a newer standby supersedes the parked one, and the one it
    /// supersedes is closed rather than merely dropped (its control server
    /// holds the connection open otherwise).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_newer_standby_replaces_the_parked_one_and_closes_it() {
        let (mut host, _client) = pair("").await;
        let (older_host, older_client) = crate::link::fixtures::link_pair().await;
        let (newer_host, _newer_client) = crate::link::fixtures::link_pair().await;
        let newer = newer_host.sink.connection().stable_id();
        let mut slot = None;
        host.on_attached(standby_attached(older_host), &mut slot)
            .unwrap();
        assert!(
            older_client.sink.connection().close_reason().is_none(),
            "the older standby was closed before anything replaced it"
        );

        host.on_attached(standby_attached(newer_host), &mut slot)
            .unwrap();

        assert_eq!(
            slot.as_ref()
                .map(|l: &Link| l.sink.connection().stable_id()),
            Some(newer),
            "the slot does not hold the newer standby"
        );
        let reason = closed_as(older_client.sink.connection()).await;
        assert!(closed_with(&reason, SUPERSEDED), "closed as {reason:?}");
    }

    /// A takeover drops the displaced client's standby. Left parked, it would
    /// let that client take the session back by failing over onto it,
    /// without going through ssh.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_primary_attach_drops_the_parked_standby() {
        let (mut host, _client) = pair("").await;
        let (standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        let (newcomer_host, _newcomer_client) = crate::link::fixtures::link_pair().await;
        let newcomer_id = newcomer_host.sink.connection().stable_id();
        let mut slot = None;
        host.on_attached(standby_attached(standby_host), &mut slot)
            .unwrap();
        assert!(slot.is_some(), "the fixture parked nothing");

        let mut newcomer = standby_attached(newcomer_host);
        newcomer.role = crate::control::Role::Primary;
        host.on_attached(newcomer, &mut slot).unwrap();

        assert!(
            slot.is_none(),
            "a takeover kept the displaced client's standby"
        );
        assert_eq!(
            host.link.sink.connection().stable_id(),
            newcomer_id,
            "the primary attach was not adopted"
        );
        let reason = closed_as(standby_client.sink.connection()).await;
        assert!(is_takeover(&reason), "closed as {reason:?}");
    }

    /// The old primary is told it was switched away from, not taken over:
    /// nobody else took anything, and the client must not read its own
    /// failover as a displacement.
    ///
    /// Driven through `promote_standby`, which is what the loop calls. The
    /// client keeps its old link open (no `swap_in`), so the close observed
    /// on it can only be the host's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn promoting_the_standby_closes_the_primary_as_switched() {
        let (mut host, client) = pair("").await;
        let (mut standby_host, mut standby_client) = crate::link::fixtures::link_pair().await;
        // Any frame: only its arrival matters here, not whether it applies.
        standby_client.sink.send(&Frame {
            my_state: 1,
            from_state: 0,
            ack_state: 0,
            flags: 0,
            payload: Vec::new(),
        });
        let first = tokio::time::timeout(Duration::from_secs(5), standby_host.source.recv())
            .await
            .expect("no frame on the standby")
            .expect("the standby closed");
        assert!(
            client.link.sink.connection().close_reason().is_none(),
            "the old primary was closed before the promotion"
        );

        host.promote_standby(standby_host, first)
            .expect("promoting the standby");

        let reason = closed_as(client.link.sink.connection()).await;
        assert!(closed_with(&reason, SWITCHED), "closed as {reason:?}");
    }

    /// The first frame on the standby is fed AFTER the reset, and so applies
    /// on the very turn that promotes it.
    ///
    /// The old generation is driven past the frame's sequence number first:
    /// against it, the client's first post-reset frame is stale and would be
    /// ignored. So a promotion that fed the frame before resetting would take
    /// it in as nothing, and this is the test that notices.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn promoting_the_standby_resets_before_it_feeds_the_first_frame() {
        let (mut host, mut client) = pair("").await;
        let (mut standby_host, standby_client) = crate::link::fixtures::link_pair().await;

        // Move the old generation on, a keystroke at a time.
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while host.input_rx.state().seq() < 4 {
            assert!(Instant::now() < deadline, "the old generation never moved");
            client.turn(b" ", &mut out).expect("a keystroke");
            tokio::time::sleep(Duration::from_millis(10)).await;
            host.turn().expect("a host turn");
        }

        // The client fails over and types: its first frame, new generation.
        client
            .swap_in(standby_client, Instant::now())
            .expect("swapping onto the standby");
        client.turn(b"x", &mut out).expect("typing on the standby");
        let first = tokio::time::timeout(Duration::from_secs(5), standby_host.source.recv())
            .await
            .expect("no frame on the standby")
            .expect("the standby closed");
        assert!(
            first.my_state < host.input_rx.state().seq(),
            "the fixture cannot tell the orders apart: frame {} is not stale \
             against the old generation at {}",
            first.my_state,
            host.input_rx.state().seq()
        );

        let turn = host
            .promote_standby(standby_host, first.clone())
            .expect("promoting the standby");

        assert_eq!(
            turn.applied, 1,
            "the first frame on the standby was not applied on the turn that promoted it"
        );
        assert_eq!(
            host.input_rx.ack(),
            first.my_state,
            "the host does not acknowledge the standby's first frame"
        );
    }

    /// The whole loop: a standby parked through the attach channel is adopted
    /// when the client's first frame arrives on it, and that frame's typing
    /// reaches the shell.
    ///
    /// The screen text is the evidence. The client's screen restarts blank
    /// when it swaps, it reads only the standby from then on, and the host
    /// only sends on a link it has adopted -- so output of the typed command
    /// on that screen can only have come over the promoted standby. The
    /// printf's own argument does not contain the marker; only its output
    /// does.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_first_frame_on_a_standby_adopts_it_and_is_applied() {
        let (mut host, mut client) = pair("").await;
        let (standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        let (attach_tx, mut attach_rx) = tokio::sync::mpsc::channel(1);
        let host_loop = tokio::spawn(async move { host.run_with_attaches(&mut attach_rx).await });

        attach_tx
            .send(standby_attached(standby_host))
            .await
            .expect("the host loop is gone");

        client
            .swap_in(standby_client, Instant::now())
            .expect("swapping onto the standby");
        assert!(!text(client.screen()).contains("standby-ok"));

        let mut out = Vec::new();
        client
            .turn(b"printf 'standby-%s\\n' ok\n", &mut out)
            .expect("typing on the standby");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !text(client.screen()).contains("standby-ok") {
            assert!(
                Instant::now() < deadline,
                "the typing never came back over the standby; screen:\n{}",
                text(client.screen())
            );
            client.turn(&[], &mut out).expect("a client turn");
            tokio::time::sleep(Duration::from_millis(3)).await;
        }

        // And the session goes on over it, to the shell's own end.
        client
            .turn(b"exit 7\n", &mut out)
            .expect("typing on the standby");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !host_loop.is_finished() {
            assert!(Instant::now() < deadline, "the shell never exited");
            // The close ends the client's turns with an error; the host's
            // status is the thing asserted.
            let _ = client.turn(&[], &mut out);
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        assert_eq!(
            host_loop.await.expect("host task").expect("host loop"),
            7,
            "the shell did not exit through the standby"
        );
    }

    /// A completed client-side standby, as a search hands it to the session.
    /// Also the shape a rebuild's `Established` has. The attach id is not
    /// the fixtures' zero, so a test can see it carried over.
    fn standby_established(link: Link) -> crate::connect::Established {
        crate::connect::Established {
            link,
            path: path_of(Rung::StunPunch, 38, 1400, 0, NatType::Unknown),
            session_id: String::new(),
            attach_id: 2,
            host_features: vec![],
        }
    }

    /// A standby, with the search it started at `now`. Created a settling
    /// delay before `now`, so that search is due at once.
    fn standby_searching(now: Instant) -> (crate::standby::Standby, u64) {
        let mut s = crate::standby::Standby::new(
            crate::attach_exchange::fixtures::stun_free(),
            now.checked_sub(crate::linkstate::STANDBY_DELAY)
                .expect("a clock this young"),
        );
        let crate::standby::StandbyAction::Search { search } = s.step(Phase::Live, now, false)
        else {
            panic!("the fixture's standby started no search");
        };
        (s, search)
    }

    /// A client-side standby holding `link`, as the search it started found
    /// it.
    fn standby_holding(link: Link) -> crate::standby::Standby {
        let (mut standby, search) = standby_searching(Instant::now());
        assert!(
            standby.found(search, standby_established(link)),
            "the fixture's search was stale"
        );
        standby
    }

    /// The terminal the client paints, shared with the test that watches it
    /// while the loop runs in a task of its own.
    #[derive(Clone, Default)]
    struct SharedOut(Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedOut {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    /// What a terminal of `size` fed everything the client wrote would be
    /// showing. Painted bytes are a diff, and a diff can split a word
    /// wherever a cell happened to be unchanged, so tests read the screen
    /// and not the bytes.
    fn screen_of(out: &SharedOut, size: TermSize) -> String {
        crate::loopback::fixtures::replay(&out.bytes(), size).join("\n")
    }

    async fn wait_for_screen(out: &SharedOut, size: TermSize, want: &str, budget: Duration) {
        let deadline = Instant::now() + budget;
        while !screen_of(out, size).contains(want) {
            assert!(
                Instant::now() < deadline,
                "{want:?} never reached the screen; it showed:\n{}",
                screen_of(out, size)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_off_screen(out: &SharedOut, size: TermSize, gone: &str, budget: Duration) {
        let deadline = Instant::now() + budget;
        while screen_of(out, size).contains(gone) {
            assert!(
                Instant::now() < deadline,
                "{gone:?} never left the screen; it showed:\n{}",
                screen_of(out, size)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Close the popup the way a user does, before typing to the shell:
    /// while it is shown it takes every key. The Esc goes alone and the
    /// next write waits for the popup to be gone, so the client reads it as
    /// a lone Esc and not as the start of an escape sequence.
    async fn close_the_popup(typing: &mut impl Write, out: &SharedOut, size: TermSize) {
        typing.write_all(&[ESCAPE]).expect("type");
        wait_off_screen(out, size, "q quit", Duration::from_secs(10)).await;
    }

    /// What reached the terminal outside the renderer. `Renderer::render`
    /// wraps every non-empty write in synchronized output
    /// (`ESC[?2026h` ... `ESC[?2026l`), so anything between those blocks
    /// was written by something else.
    fn outside_renderer(out: &[u8]) -> Vec<u8> {
        const BEGIN: &[u8] = b"\x1b[?2026h";
        const END: &[u8] = b"\x1b[?2026l";
        let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
        let mut rest = out;
        let mut stray = Vec::new();
        while !rest.is_empty() {
            let Some(begin) = find(rest, BEGIN) else {
                stray.extend_from_slice(rest);
                break;
            };
            stray.extend_from_slice(&rest[..begin]);
            let inside = &rest[begin + BEGIN.len()..];
            let Some(end) = find(inside, END) else {
                stray.extend_from_slice(&rest[begin..]);
                break;
            };
            rest = &inside[end + END.len()..];
        }
        stray
    }

    /// The helper has to be able to see a stray, or every assertion made
    /// with it is a guard that cannot fail.
    #[test]
    fn outside_renderer_finds_what_is_written_between_the_renderers_blocks() {
        assert_eq!(
            outside_renderer(b"x\x1b[?2026hA\x1b[?2026ly\r\n"),
            b"xy\r\n"
        );
        assert!(outside_renderer(b"\x1b[?2026hA\x1b[?2026l\x1b[?2026hB\x1b[?2026l").is_empty());
    }

    fn last_entry(c: &ClientSession) -> Option<(Kind, String)> {
        c.activity
            .entries()
            .next_back()
            .map(|e| (e.kind, e.text.clone()))
    }

    impl Write for SharedOut {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Spec §5.2: what used to be a line written over the session is an
    /// entry in the activity log. The handler has no terminal to write to.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_found_standby_is_recorded() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let now = Instant::now();
        let (standby, search) = standby_searching(now);
        client.standby = Some(standby);
        let (_standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        assert_eq!(
            client.activity.entries().len(),
            0,
            "the fixture recorded something"
        );

        let watch = client.on_standby_event(
            crate::standby::StandbyEvent::Found {
                search,
                e: Box::new(standby_established(standby_client)),
            },
            now,
        );

        assert!(
            watch.is_some(),
            "the loop was not handed the standby to watch"
        );
        assert_eq!(
            last_entry(&client),
            Some((Kind::Standby, "found IPv4 punched, 38 ms".to_string()))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_search_is_recorded_with_its_reason_and_a_stale_one_is_not() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let now = Instant::now();
        let (standby, search) = standby_searching(now);
        client.standby = Some(standby);

        client.on_standby_event(
            crate::standby::StandbyEvent::NotFound {
                search: search + 7,
                reason: "stale".to_string(),
            },
            now,
        );
        assert_eq!(
            client.activity.entries().len(),
            0,
            "a stale search's failure was recorded"
        );

        client.on_standby_event(
            crate::standby::StandbyEvent::NotFound {
                search,
                reason: "no path".to_string(),
            },
            now,
        );
        assert_eq!(
            last_entry(&client),
            Some((Kind::Standby, "not found: no path".to_string()))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_standby_is_recorded() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let (_standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        client.standby = Some(standby_holding(standby_client));
        assert_eq!(
            client.activity.entries().len(),
            0,
            "the fixture recorded something"
        );

        client.on_standby_closed(&quinn::ConnectionError::TimedOut, Instant::now());

        assert_eq!(
            last_entry(&client),
            Some((Kind::Standby, "lost: timed out".to_string()))
        );
        assert!(!client.standby.as_ref().is_some_and(|s| s.has_link()));
    }

    /// A search the loop starts is recorded as it starts.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_search_started_is_recorded() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let now = Instant::now();
        client.standby = Some(crate::standby::Standby::new(
            crate::attach_exchange::fixtures::stun_free(),
            now.checked_sub(crate::linkstate::STANDBY_DELAY)
                .expect("a clock this young"),
        ));
        assert_eq!(
            client.activity.entries().len(),
            0,
            "the fixture recorded something"
        );

        let action = client.standby_step(Phase::Live, now, false);

        assert!(
            matches!(action, crate::standby::StandbyAction::Search { .. }),
            "no search started: {action:?}"
        );
        assert_eq!(
            last_entry(&client),
            Some((Kind::Standby, "search started".to_string()))
        );
    }

    /// A probe failing every `PROBE_RETRY` through a long outage is one
    /// "probing standby" and one folded failure, not two alternating
    /// entries every five seconds that push everything useful out of the
    /// ring.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_long_outage_records_its_probing_once_and_folds_its_failures() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let (_standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        client.standby = Some(standby_holding(standby_client));
        let t0 = Instant::now();
        let silent = Phase::Silent { since: t0 };

        for at in [t0, t0 + crate::linkstate::PROBE_RETRY] {
            let action = client.standby_step(silent, at, false);
            assert!(
                matches!(action, crate::standby::StandbyAction::Probe { .. }),
                "no probe at {:?}: {action:?}",
                at - t0
            );
            client.on_standby_event(crate::standby::StandbyEvent::Probed { answered: false }, at);
        }

        let log: Vec<(Kind, String, u32)> = client
            .activity
            .entries()
            .map(|e| (e.kind, e.text.clone(), e.repeats))
            .collect();
        assert_eq!(
            log,
            vec![
                (Kind::Failover, "probing standby".to_string(), 0),
                (Kind::Failover, "probe failed".to_string(), 1),
            ]
        );
    }

    /// The failover is recorded, writes nothing to the terminal but the
    /// renderer's output, and the path it went to becomes the session's.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failover_is_recorded_and_the_path_follows_it() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let mut out = Vec::new();
        client
            .announce(
                &path_of(Rung::Ipv6Direct, 11, 1452, 0, NatType::None),
                &mut out,
            )
            .expect("announce");
        out.clear();
        client.identity = Some(Identity {
            target: "bastion".into(),
            session_id: "f0".repeat(16),
            attach_id: 5,
        });
        let (_standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        client.standby = Some(standby_holding(standby_client));

        assert!(
            client
                .fail_over(Instant::now(), &mut out)
                .expect("failing over")
        );

        assert_eq!(
            last_entry(&client),
            Some((
                Kind::Failover,
                "switched to standby (IPv4 punched)".to_string()
            ))
        );
        assert!(
            outside_renderer(&out).is_empty(),
            "{:?}",
            String::from_utf8_lossy(&outside_renderer(&out))
        );
        assert_eq!(
            client.identity.as_ref().map(|i| i.attach_id),
            Some(2),
            "the attach id stayed the old link's"
        );
        let standby = path_of(Rung::StunPunch, 38, 1400, 0, NatType::Unknown);
        assert!(
            !client.announce(&standby, &mut out).expect("announce"),
            "the failover did not record the path it switched to"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failover_with_no_standby_does_nothing() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let mut out = Vec::new();
        assert!(
            !client
                .fail_over(Instant::now(), &mut out)
                .expect("failing over")
        );
        assert_eq!(client.activity.entries().len(), 0);
        assert!(out.is_empty(), "{:?}", String::from_utf8_lossy(&out));
    }

    /// The migration branch of `announce` used to print; the session owns the
    /// screen by then.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_path_migration_is_recorded_not_printed() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let mut out = Vec::new();
        client
            .announce(
                &path_of(Rung::StunPunch, 38, 1392, 0, NatType::EndpointIndependent),
                &mut out,
            )
            .expect("first");
        assert!(!out.is_empty(), "the connect line was not printed");
        assert_eq!(
            client.activity.entries().len(),
            0,
            "the connect line was recorded"
        );
        out.clear();

        let better = path_of(Rung::Ipv6Direct, 11, 1452, 0, NatType::None);
        assert!(client.announce(&better, &mut out).expect("second"));

        assert!(
            out.is_empty(),
            "the migration was written over the session: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(
            last_entry(&client),
            Some((Kind::Link, "path migrated \u{2192} IPv6 direct".to_string()))
        );
    }

    /// Spec §5.2, synchronously: once the session owns the screen, the
    /// events that used to write a line over it -- held input sent, a path
    /// migration, a failover -- reach the terminal only as the renderer's
    /// output. The connect banner, written before, is the one thing outside
    /// it, and finding exactly the banner shows the check can see a stray.
    #[tokio::test(flavor = "multi_thread")]
    async fn after_the_banner_only_the_renderer_writes_to_the_terminal() {
        let (_host, mut client) = with_confirming_popup(b"make test\r").await;
        let mut out = Vec::new();
        let first = path_of(Rung::Ipv6Direct, 11, 1452, 0, NatType::None);
        client.announce(&first, &mut out).expect("the banner");
        let banner = format!("{}\n", status_line(&first));
        assert_eq!(String::from_utf8_lossy(&outside_renderer(&out)), banner);

        assert_eq!(answer(&mut client, b"s", &mut out), None);
        let migrated = path_of(Rung::StunPunch, 38, 1392, 0, NatType::EndpointIndependent);
        assert!(client.announce(&migrated, &mut out).expect("a migration"));
        let (_standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        client.standby = Some(standby_holding(standby_client));
        assert!(
            client
                .fail_over(Instant::now(), &mut out)
                .expect("failing over")
        );

        let texts: Vec<String> = client.activity.entries().map(|e| e.text.clone()).collect();
        for event in ["held input sent", "path migrated", "switched to standby"] {
            assert!(
                texts.iter().any(|t| t.starts_with(event)),
                "{event:?} never happened, so it proves nothing: {texts:?}"
            );
        }
        assert_eq!(
            String::from_utf8_lossy(&outside_renderer(&out)),
            banner,
            "written outside the renderer after the banner"
        );
    }

    #[tokio::test]
    async fn sending_or_dropping_held_input_is_recorded() {
        let (_host, mut session) = with_confirming_popup(b"make test\r").await;
        let mut out = Vec::new();
        assert!(
            !session.activity.entries().any(|e| e.kind == Kind::Input),
            "the fixture recorded input"
        );
        answer(&mut session, b"s", &mut out);
        assert_eq!(
            last_entry(&session),
            Some((Kind::Input, "held input sent (10 bytes)".to_string()))
        );

        let (_host, mut session) = with_confirming_popup(b"rm -rf /tmp/x\r").await;
        answer(&mut session, b"d", &mut out);
        assert_eq!(
            last_entry(&session),
            Some((Kind::Input, "held input dropped (14 bytes)".to_string()))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_rebuild_attempt_and_its_failure_are_recorded() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let pidfile = dir.path().join("ssh.pid");
        let rebuild = Rebuild::new("bastion.example.net".to_owned(), "f0".repeat(16))
            .via(hanging_ssh(dir.path(), &pidfile), stunless());
        let (_host, mut session) = pair_on("127.0.0.1:0", "/bin/sh", Some(rebuild)).await;
        let entered = drive_to_recovering(&mut session);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        assert!(
            !session.activity.entries().any(|e| e.kind == Kind::Rebuild),
            "the fixture recorded a rebuild"
        );

        session.rebuild_step(entered, &tx);
        assert_eq!(
            last_entry(&session),
            Some((Kind::Rebuild, "attempt 1 started".to_string()))
        );

        session.rebuild_failed(
            "ssh exited with status 255".to_string(),
            entered + Duration::from_secs(1),
        );
        assert_eq!(
            last_entry(&session),
            Some((
                Kind::Rebuild,
                "attempt 1 failed: ssh exited with status 255".to_string()
            ))
        );
        assert!(matches!(
            session.link_state.phase_now(),
            Phase::Recovering { attempt: 1, .. }
        ));
        assert_eq!(
            session.last_failure.as_deref(),
            Some("ssh exited with status 255")
        );
        if let Some(r) = session.rebuild.as_mut() {
            r.cancel();
        }
    }

    /// The attempt that ends the session is the last line of the file, so
    /// it must say how it ended (spec §5.1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_definite_rebuild_failure_is_recorded_before_the_session_ends() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let pidfile = dir.path().join("ssh.pid");
        let rebuild = Rebuild::new("bastion.example.net".to_owned(), "f0".repeat(16))
            .via(hanging_ssh(dir.path(), &pidfile), stunless());
        let (_host, mut session) = pair_on("127.0.0.1:0", "/bin/sh", Some(rebuild)).await;
        let entered = drive_to_recovering(&mut session);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        session.rebuild_step(entered, &tx);
        assert_eq!(
            last_entry(&session),
            Some((Kind::Rebuild, "attempt 1 started".to_string()))
        );

        let err = session.rebuild_refused("the host no longer knows this session");

        assert_eq!(
            last_entry(&session),
            Some((
                Kind::Rebuild,
                "attempt 1 failed: the host no longer knows this session".to_string()
            ))
        );
        assert_eq!(
            err.to_string(),
            "this session cannot be resumed: the host no longer knows this session"
        );
        assert_eq!(
            session.last_failure, None,
            "a definite answer is not a retry"
        );
        if let Some(r) = session.rebuild.as_mut() {
            r.cancel();
        }
    }

    /// A probe's answer is recorded; a stale one (the probe it answers is no
    /// longer pending) is not.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_probe_answer_is_recorded_and_a_stale_one_is_not() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let (_standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        client.standby = Some(standby_holding(standby_client));
        let t0 = Instant::now();
        let silent = Phase::Silent { since: t0 };

        client.on_standby_event(crate::standby::StandbyEvent::Probed { answered: true }, t0);
        assert_eq!(
            client.activity.entries().len(),
            0,
            "a probe nobody sent was recorded"
        );

        let action = client.standby_step(silent, t0, false);
        assert!(
            matches!(action, crate::standby::StandbyAction::Probe { .. }),
            "no probe started: {action:?}"
        );
        assert_eq!(
            last_entry(&client),
            Some((Kind::Failover, "probing standby".to_string()))
        );
        client.on_standby_event(crate::standby::StandbyEvent::Probed { answered: true }, t0);
        assert_eq!(
            last_entry(&client),
            Some((Kind::Failover, "probe answered".to_string()))
        );
        let before = client.activity.entries().len();
        client.on_standby_event(crate::standby::StandbyEvent::Probed { answered: true }, t0);
        assert_eq!(
            client.activity.entries().len(),
            before,
            "a stale answer was recorded"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_landed_rebuild_is_recorded_and_its_path_and_attach_become_the_sessions() {
        let (_host, mut client) = pair("").await;
        client.identity = Some(Identity {
            target: "bastion".into(),
            session_id: "f0".repeat(16),
            attach_id: 5,
        });
        assert!(client.path.is_none(), "the fixture already had a path");
        let (_rebuilt_host, rebuilt) = crate::link::fixtures::link_pair().await;

        client
            .rebuild_landed(standby_established(rebuilt), Instant::now())
            .expect("landing");

        assert_eq!(
            last_entry(&client),
            Some((Kind::Rebuild, "landed via IPv4 punched".to_string()))
        );
        assert_eq!(client.path.as_ref().map(|p| p.rtt_ms), Some(38));
        assert_eq!(client.identity.as_ref().map(|i| i.attach_id), Some(2));
    }

    /// Ruling B1. A failover is not a rebuild landing: an ssh attempt that
    /// started before it may still reach the host and close this link as
    /// taken over, and that close must still be read as our own doing. So the
    /// latch survives a failover, while the old link is closed as switched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failover_keeps_the_rebuild_latch_and_closes_the_old_link_as_switched() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let pidfile = dir.path().join("ssh.pid");
        let rebuild = Rebuild::new("bastion.example.net".to_owned(), "f0".repeat(16))
            .via(hanging_ssh(dir.path(), &pidfile), stunless());
        let (host, mut session) = pair_on("127.0.0.1:0", "/bin/sh", Some(rebuild)).await;
        let entered = drive_to_recovering(&mut session);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        session.rebuild_step(entered, &tx);
        assert!(
            session
                .rebuild
                .as_ref()
                .is_some_and(Rebuild::may_have_displaced_us),
            "the fixture never latched"
        );
        let (_standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        let displaced = host.link.sink.connection().clone();

        session
            .swap_in_as(standby_client, Instant::now(), SWITCHED)
            .expect("failing over");

        assert!(
            session
                .rebuild
                .as_ref()
                .is_some_and(Rebuild::may_have_displaced_us),
            "the failover cleared the latch: the attempt still in flight would \
             now end the session when it lands"
        );
        let reason = closed_as(&displaced).await;
        assert!(closed_with(&reason, SWITCHED), "closed as {reason:?}");
        if let Some(r) = session.rebuild.as_mut() {
            r.cancel();
        }
    }

    /// A rebuild that lands drops the standby: the host closed it as taken
    /// over when it adopted the rebuild. Ours is closed too, because a
    /// standby whose path is dead never hears the host's close, and with no
    /// idle timeout it would otherwise never end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_rebuild_that_lands_forgets_and_closes_the_standby() {
        let (_host, mut client) = pair("").await;
        let (standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        client.standby = Some(standby_holding(standby_client));
        let (_rebuilt_host, rebuilt) = crate::link::fixtures::link_pair().await;
        assert!(
            client.standby.as_ref().is_some_and(|s| s.has_link()),
            "the fixture holds no standby"
        );

        client
            .swap_in(rebuilt, Instant::now())
            .expect("swapping in the rebuilt link");

        assert!(
            !client.standby.as_ref().is_some_and(|s| s.has_link()),
            "a landed rebuild left the standby the host had dropped in place"
        );
        let reason = closed_as(standby_host.sink.connection()).await;
        assert!(closed_with(&reason, REBUILT), "closed as {reason:?}");
    }

    /// The loop watches the standby's connection: when it closes, the
    /// standby is forgotten and its loss is recorded (spec §5.1), which the
    /// popup's log shows. The close is the host's `SUPERSEDED`, a reason that
    /// ends nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_standby_closed_under_a_running_session_is_recorded_and_forgotten() {
        let (mut host, mut client) = pair_sized("", BIG).await;
        let (standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        client.standby = Some(standby_holding(standby_client));

        let (_attach_tx, mut attach_rx) = tokio::sync::mpsc::channel(1);
        let host_loop = tokio::spawn(async move { host.run_with_attaches(&mut attach_rx).await });
        let (keys, mut typing) = keyboard();
        let out = SharedOut::default();
        let client_loop = tokio::spawn({
            let mut out = out.clone();
            async move {
                let code = client.run_on(keys, &mut out).await;
                (code, client)
            }
        });

        // A round trip through the shell: the loop is running, laps and all.
        typing
            .write_all(b"printf 'ready-%s\\n' ok\n")
            .expect("type");
        wait_for_screen(&out, BIG, "ready-ok", Duration::from_secs(10)).await;

        // Losing a standby is not an outage, so nothing opens the popup by
        // itself: open it by hand, as a curious user would.
        typing.write_all(&[CTRL_BACKSLASH]).expect("type");
        wait_for_screen(&out, BIG, "Esc close", Duration::from_secs(10)).await;
        assert!(
            !screen_of(&out, BIG).contains("lost:"),
            "the standby was reported lost before anything closed it"
        );

        standby_host
            .sink
            .connection()
            .close(quinn::VarInt::from_u32(0), SUPERSEDED);
        wait_for_screen(&out, BIG, "lost:", Duration::from_secs(10)).await;

        // The open popup takes every key: close it, then talk to the shell.
        close_the_popup(&mut typing, &out, BIG).await;
        typing.write_all(b"exit 7\n").expect("type");
        let (code, client) = tokio::time::timeout(Duration::from_secs(15), client_loop)
            .await
            .expect("the client never finished")
            .expect("client task");
        assert_eq!(code.expect("the client loop failed"), 7);
        assert!(
            !client.standby.as_ref().is_some_and(|s| s.has_link()),
            "the closed standby is still held"
        );
        let stray = outside_renderer(&out.bytes());
        assert!(
            stray.is_empty(),
            "written outside the renderer: {:?}",
            String::from_utf8_lossy(&stray)
        );
        assert_eq!(host_loop.await.expect("host task").expect("host loop"), 7);
    }

    /// Spec §3.5 end to end: the primary goes dark, the standby answers its
    /// probe, and the client carries on over the standby with no ssh anywhere
    /// in the test.
    ///
    /// The exit status is the evidence: `exit 7` is typed while the primary
    /// is blackholed, so only the standby can carry it to the shell and the
    /// status back. The final link's identity says which one did.
    ///
    /// The popup opens for the outage and shows the failover in its log;
    /// the test reads that off the replayed screen, then waits for the popup
    /// to say the link is back and to close by itself, and only then types
    /// `exit 7`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_dead_primary_fails_over_onto_an_answering_standby() {
        let (mut host, mut client, relay) = pair_through_relay_sized("", BIG).await;
        let primary_id = client.link.sink.connection().stable_id();
        let (standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        // The probe is answered by the standby's own control server. Its door
        // is never knocked on.
        let (door_tx, _door_rx) = tokio::sync::mpsc::channel(1);
        crate::control::serve_control(standby_host.sink.connection().clone(), door_tx);
        let standby_id = standby_client.sink.connection().stable_id();
        assert_ne!(standby_id, primary_id);

        client.standby = Some(standby_holding(standby_client));

        let (attach_tx, mut attach_rx) = tokio::sync::mpsc::channel(1);
        let host_loop = tokio::spawn(async move { host.run_with_attaches(&mut attach_rx).await });
        attach_tx
            .send(standby_attached(standby_host))
            .await
            .expect("the host loop is gone");

        let (keys, mut typing) = keyboard();
        let out = SharedOut::default();
        let client_loop = tokio::spawn({
            let mut out = out.clone();
            async move {
                let code = client.run_on(keys, &mut out).await;
                (code, client)
            }
        });

        relay.blackhole(true);
        // Sent while the phase is still `Live`, so a reply is owed and the
        // silence is noticed. The bytes themselves are lost with the swap.
        typing.write_all(b"true\n").expect("type");
        wait_for_screen(&out, BIG, "switched to standby", Duration::from_secs(15)).await;
        // And answering on it. Typed before the first frame arrives on
        // it, `exit 7` would be held for the `Confirming` question and never
        // reach the shell.
        wait_for_screen(&out, BIG, "LIVE again", Duration::from_secs(15)).await;
        // The lingering popup takes every key. Let it close by itself
        // rather than pressing Esc: an Esc racing the linger's end would
        // reach the shell, and `ESC e` is a readline command.
        wait_off_screen(&out, BIG, "LIVE again", Duration::from_secs(10)).await;
        typing.write_all(b"exit 7\n").expect("type");

        let (code, client) = tokio::time::timeout(Duration::from_secs(15), client_loop)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the client never finished; it showed:\n{}",
                    screen_of(&out, BIG)
                )
            })
            .expect("client task");
        assert_eq!(
            code.expect("the client loop failed"),
            7,
            "the shell's exit did not come back while the primary was dark"
        );
        assert_eq!(
            client.link.sink.connection().stable_id(),
            standby_id,
            "the session did not end on the standby"
        );
        let stray = outside_renderer(&out.bytes());
        assert!(
            stray.is_empty(),
            "written outside the renderer: {:?}",
            String::from_utf8_lossy(&stray)
        );
        assert_eq!(host_loop.await.expect("host task").expect("host loop"), 7);
    }

    /// The search, end to end through the doors `serve` opens: the request
    /// goes out on the FIRST link's control stream, runs the real exchange
    /// through the listener, is parked by the host, and is recorded by the
    /// client, whose popup shows it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_standby_is_found_over_the_first_links_control_stream() {
        let (mut host, mut client) = pair_sized("", BIG).await;
        let primary_id = client.link.sink.connection().stable_id();
        let dir = tempfile::tempdir().expect("a scratch directory");
        let listener =
            tokio::net::UnixListener::bind(dir.path().join("sock")).expect("binding the socket");
        let start =
            crate::attach_exchange::fixtures::fresh_meta("00112233445566778899aabbccddeeff");
        let guard = Arc::new(
            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
        );
        let meta = Arc::new(tokio::sync::Mutex::new(start));
        let (door_task, mut attach_rx) = crate::serve::open_doors(
            listener,
            guard,
            Arc::clone(&meta),
            crate::attach_exchange::fixtures::stun_free(),
            host.link.sink.connection().clone(),
        );
        let host_loop = tokio::spawn(async move { host.run_with_attaches(&mut attach_rx).await });

        // Due at once rather than after the settling delay. Loopback is the
        // primary's own path, so the real filter would rightly refuse every
        // candidate this test has: this one admits them.
        let mut standby = crate::standby::Standby::new(
            crate::attach_exchange::fixtures::stun_free(),
            Instant::now()
                .checked_sub(crate::linkstate::STANDBY_DELAY)
                .expect("a clock this young"),
        );
        standby.admit_for = |_| Some(Arc::new(|_| true));
        client.standby = Some(standby);

        let (keys, mut typing) = keyboard();
        let out = SharedOut::default();
        let client_loop = tokio::spawn({
            let mut out = out.clone();
            async move {
                let code = client.run_on(keys, &mut out).await;
                (code, client)
            }
        });

        // A found standby is not an outage, so nothing opens the popup by
        // itself: open it by hand and watch its log.
        typing.write_all(&[CTRL_BACKSLASH]).expect("type");
        wait_for_screen(&out, BIG, "Esc close", Duration::from_secs(10)).await;
        wait_for_screen(&out, BIG, "found ", Duration::from_secs(20)).await;
        assert_eq!(
            meta.lock().await.attach_id,
            1,
            "the host did not run an exchange for the standby"
        );

        // The open popup takes every key: close it, then talk to the shell.
        close_the_popup(&mut typing, &out, BIG).await;
        typing.write_all(b"exit 7\n").expect("type");
        let (code, client) = tokio::time::timeout(Duration::from_secs(15), client_loop)
            .await
            .expect("the client never finished")
            .expect("client task");
        assert_eq!(code.expect("the client loop failed"), 7);
        assert_eq!(
            client.link.sink.connection().stable_id(),
            primary_id,
            "finding a standby moved the session off the primary"
        );
        assert!(
            client
                .activity
                .entries()
                .any(|e| e.kind == Kind::Standby && e.text.starts_with("found ")),
            "the standby was shown but not recorded"
        );
        assert_eq!(host_loop.await.expect("host task").expect("host loop"), 7);
        crate::listener::close_the_door(door_task).await;
    }

    /// What the popup may not say while `Silent` or `Confirming`, wherever
    /// in it it says it.
    ///
    /// Nothing is reconnecting in those phases -- the rebuild loop starts
    /// only in `Recovering` -- so they may not name it. And from here a dead
    /// network and a crashed host are indistinguishable, so the popup may not
    /// vouch for the far end at all, not even hedged. What a key DOES stays
    /// true either way. The list is `view.rs`'s, shared.
    fn assert_claims_nothing_it_cannot_see(shown: &str) {
        crate::view::assert_claims_nothing_it_cannot_see(shown);
    }

    /// Ctrl-\, the key layer 1 listens for. `ui`'s own copy is the code
    /// under test, and a test that reached for it would be asserting the
    /// constant rather than the keystroke.
    const CTRL_BACKSLASH: u8 = 0x1c;

    /// A lone Esc, the popup's close key; the same reasoning.
    const ESCAPE: u8 = 0x1b;

    /// A client with the popup up for a real `Silent` phase, left exactly as
    /// the loop leaves it: the phase decided by `popup_at`, and `shown`
    /// mirroring what the overlay is.
    async fn with_popup() -> (HostSession, ClientSession) {
        let t = std::time::Instant::now();
        let (host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        // The lap the owing begins on, and it is not decoration: the grace
        // period is measured from when the reply started being owed, so a
        // fixture that jumped straight to three seconds would be asking about
        // an owing three seconds long that had only just started.
        assert!(session.popup_at(t).is_none());
        let view = session.popup_at(t + Duration::from_secs(3));
        assert!(view.is_some(), "the fixture raised no popup");
        session.shown = view;
        (host, session)
    }

    /// A client whose popup asks about `held`, typed blind: the popup went
    /// up, the user closed it and typed, and the host started answering
    /// again. The only phase that offers `s send` and `d drop`.
    async fn with_confirming_popup(held: &[u8]) -> (HostSession, ClientSession) {
        let (host, mut session) = with_popup().await;
        let mut out = Vec::new();
        session
            .route_keys(&[ESCAPE], &mut out)
            .expect("close the popup");
        session.route_keys(held, &mut out).expect("hold the typing");

        let now = std::time::Instant::now();
        session.note_heard(now);
        let view = session.popup_at(now);
        assert!(view.is_some(), "the fixture asked the user nothing");
        session.shown = view;
        (host, session)
    }

    /// Press `keys` once the question has been up for `ANSWER_GUARD`: the
    /// read is stamped that far past now, which is past when any lap or
    /// read before this one first saw `Confirming`.
    fn answer(session: &mut ClientSession, keys: &[u8], out: &mut Vec<u8>) -> Option<i32> {
        session
            .route_keys_at(keys, Instant::now() + crate::ui::ANSWER_GUARD, out)
            .expect("the answer lands")
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
    /// back to life underneath a popup saying nobody is answering.
    #[tokio::test]
    async fn a_scavenged_frame_ends_the_silence() {
        let (mut host, mut session) = with_popup().await;
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
    /// it or the popup overstates the outage for as long as it is up.
    #[tokio::test]
    async fn a_scavenged_frame_ends_the_outage_the_popup_reports() {
        let (mut host, mut session) = with_popup().await;
        let mut out = Vec::new();
        assert!(
            session.outage_at(Instant::now()),
            "the fixture's popup reports no outage"
        );

        host.turn().expect("the host takes a turn");
        wait_for_frame(&mut session).await;
        session.turn(&[], &mut out).expect("a pacing lap");

        let v = session
            .popup_at(Instant::now())
            .expect("the popup closed instead of lingering");
        assert_eq!(
            v.marker,
            Marker::LiveAgain,
            "a frame was applied and the popup still reports silence"
        );
    }

    /// `heard` runs on every frame. One landing while the question is up
    /// must neither answer it nor take it down: it stays until `s` or `d`.
    #[tokio::test]
    async fn a_frame_while_the_question_is_up_does_not_answer_it() {
        let (mut host, mut session) = with_confirming_popup(b"echo hi\r").await;
        let mut out = Vec::new();

        host.turn().expect("the host takes a turn");
        wait_for_frame(&mut session).await;
        session.turn(&[], &mut out).expect("a pacing lap");

        assert_eq!(
            session.link_state.held(),
            b"echo hi\r",
            "a frame resolved the held input"
        );
        assert!(
            session.popup_at(Instant::now()).is_some(),
            "a frame took the question down"
        );
        answer(&mut session, b"d", &mut out);
        assert!(
            session.link_state.held().is_empty(),
            "d did not drop the held buffer after a frame"
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

    /// `q` is the popup's own key, so it must not reach the shell, and it
    /// must end the client with a status of its own
    /// rather than one invented for a shell that never exited.
    #[tokio::test]
    async fn the_quit_key_ends_the_client_with_a_status_of_zero() {
        let (_host, mut session) = with_popup().await;
        let before = spoken(&session);
        let mut out = Vec::new();

        let answer = session.route_keys(b"q", &mut out).unwrap();

        assert_eq!(answer, Some(0), "the quit key did not end the client");
        assert_eq!(spoken(&session), before, "the command reached the shell");
    }

    /// The other half of holding it: giving it back, in one piece and in
    /// order, once the user has looked at the screen and said so.
    #[tokio::test]
    async fn the_send_key_delivers_what_was_typed_blind() {
        let (_host, mut session) = with_confirming_popup(b"make test\r").await;
        let mut out = Vec::new();

        let answered = answer(&mut session, b"s", &mut out);

        assert_eq!(answered, None, "sending the held input ended the session");
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
        let (_host, mut session) = with_confirming_popup(b"make test\r").await;
        let mut out = Vec::new();
        let before = spoken(&session);

        let answered = answer(&mut session, b"d", &mut out);

        assert_eq!(answered, None, "dropping the held input ended the session");
        assert!(session.link_state.held().is_empty(), "the drop kept it");
        assert_eq!(spoken(&session), before, "the drop sent it instead");
    }

    /// The outage popup offers `Esc` and `q`; `s` and `d` answer only the
    /// question. Pressed into it, they must neither deliver nor discard what
    /// was typed blind before it was opened again -- nor be kept as typing.
    #[tokio::test]
    async fn the_outage_popup_does_not_honour_the_keys_it_does_not_offer() {
        let (_host, mut session) = with_popup().await;
        let mut out = Vec::new();
        session.route_keys(&[ESCAPE], &mut out).unwrap();
        session.route_keys(b"make test\r", &mut out).unwrap();
        session.route_keys(&[CTRL_BACKSLASH], &mut out).unwrap();
        assert!(
            session.ui.visible(session.link_state.phase_now()),
            "Ctrl-\\ did not open the popup again"
        );
        let before = spoken(&session);

        assert_eq!(session.route_keys(b"s", &mut out).unwrap(), None);
        assert_eq!(
            spoken(&session),
            before,
            "the held input was delivered to a host the popup says is not answering"
        );
        assert_eq!(session.route_keys(b"d", &mut out).unwrap(), None);
        assert_eq!(
            session.link_state.held(),
            b"make test\r",
            "the held input was discarded, or the keys were kept as typing"
        );
    }

    /// And `q` is the key every popup offers, the question included.
    #[tokio::test]
    async fn the_quit_key_works_under_the_confirming_popup_too() {
        let (_host, mut session) = with_confirming_popup(b"make test\r").await;
        let mut out = Vec::new();

        assert_eq!(answer(&mut session, b"q", &mut out), Some(0));
    }

    /// Final review, Important 1, at the session: a key that was in flight
    /// when the question appeared answers nothing -- the held input is
    /// neither sent nor dropped and the client does not quit -- and the
    /// same keys answer once the question has been up `ANSWER_GUARD`.
    #[tokio::test]
    async fn keys_in_flight_when_the_question_appears_do_not_answer_it() {
        let (_host, mut session) = with_confirming_popup(b"make test\r").await;
        let before = spoken(&session);
        let mut out = Vec::new();

        for key in [&b"s"[..], b"d", b"q"] {
            assert_eq!(
                session.route_keys(key, &mut out).unwrap(),
                None,
                "{key:?} quit"
            );
        }
        assert_eq!(spoken(&session), before, "an in-flight key sent the input");
        assert_eq!(
            session.link_state.held(),
            b"make test\r",
            "an in-flight key dropped the input, or was held with it"
        );
        assert!(
            session.ui.visible(session.link_state.phase_now()),
            "an in-flight key took the question down"
        );

        assert_eq!(answer(&mut session, b"s", &mut out), None);
        assert!(
            spoken(&session).ends_with(b"make test\r"),
            "{:?}",
            spoken(&session)
        );
    }

    /// The whole point of the key on a healthy link: it opens the popup and
    /// is not sent.
    #[tokio::test]
    async fn ctrl_backslash_opens_the_popup_on_a_healthy_session() {
        let (_host, mut session) = pair("/bin/sh").await;
        let now = Instant::now();
        assert!(
            session.popup_at(now).is_none(),
            "the popup was up before the key"
        );
        let before = spoken(&session);
        let mut out = Vec::new();

        assert_eq!(
            session.route_keys(&[CTRL_BACKSLASH], &mut out).unwrap(),
            None
        );

        assert_eq!(spoken(&session), before, "the key reached the host");
        let v = session.popup_at(now).expect("the key opened nothing");
        assert_eq!(v.marker, Marker::Live);
        assert!(words(&v).contains("Esc close"), "{}", words(&v));
    }

    /// On a healthy link the open popup takes every key too: typing into it
    /// reaches neither the host nor the held buffer, and it stays up.
    #[tokio::test]
    async fn typing_into_an_open_popup_on_a_healthy_session_goes_nowhere() {
        let (_host, mut session) = pair("/bin/sh").await;
        let mut out = Vec::new();
        session.route_keys(&[CTRL_BACKSLASH], &mut out).unwrap();
        let before = spoken(&session);

        assert_eq!(session.route_keys(b"ls\r", &mut out).unwrap(), None);

        assert_eq!(
            spoken(&session),
            before,
            "typing went through the popup to the host"
        );
        assert!(
            session.link_state.held().is_empty(),
            "typing into the popup was held"
        );
        assert!(
            session.ui.visible(session.link_state.phase_now()),
            "typing closed the popup"
        );
    }

    /// Review focus 2, at the session: closing the popup an outage opened
    /// makes typing blind a choice, and what is typed then is held -- not
    /// sent at a host that is not answering -- while the popup stays shut
    /// for the rest of the outage.
    #[tokio::test]
    async fn typing_after_closing_the_outage_popup_is_held_and_not_sent() {
        let (_host, mut session) = with_popup().await;
        let before = spoken(&session);
        let mut out = Vec::new();

        assert_eq!(session.route_keys(&[ESCAPE], &mut out).unwrap(), None);
        assert!(
            !session.ui.visible(session.link_state.phase_now()),
            "Esc left the popup up"
        );
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
        // Still `Silent` (the rebuild starts at `REBUILD_AFTER`, 20 s), and
        // still the outage the popup was closed in.
        let later = Instant::now() + Duration::from_secs(5);
        assert!(
            session.popup_at(later).is_none(),
            "the popup opened itself again for the outage it was closed in"
        );
    }

    /// Review focus 3, at the session: typing held behind a popup closed by
    /// hand is asked about when the host answers, and the question takes a
    /// bare `s`.
    #[tokio::test]
    async fn held_input_after_a_closed_outage_opens_the_question() {
        let (_host, mut session) = with_popup().await;
        let mut out = Vec::new();
        session.route_keys(&[ESCAPE], &mut out).unwrap();
        session.route_keys(b"make test\r", &mut out).unwrap();
        assert!(
            session.popup_at(Instant::now()).is_none(),
            "the fixture's popup did not stay closed"
        );

        let now = Instant::now();
        session.note_heard(now);
        let v = session
            .popup_at(now)
            .expect("the host answered with input held, and nothing was asked");
        assert!(
            v.held
                .first()
                .is_some_and(|l| l.contains("deliver what you typed?")),
            "{:?}",
            v.held
        );
        assert!(words(&v).contains("s send"), "{}", words(&v));

        let before = spoken(&session);
        assert!(
            !before.ends_with(b"make test\r"),
            "sent before it was asked about"
        );
        assert_eq!(answer(&mut session, b"s", &mut out), None);
        assert!(
            spoken(&session).ends_with(b"make test\r"),
            "{:?}",
            spoken(&session)
        );
        assert!(session.link_state.held().is_empty(), "sent and kept");
        assert!(
            session.popup_at(Instant::now()).is_none(),
            "the answered question stayed up"
        );
    }

    /// The popup an outage opened stays up for `LINGER` once the link is
    /// back, saying so, and then closes by itself.
    #[tokio::test]
    async fn returning_to_live_lingers_and_then_closes() {
        let (_host, mut session) = with_popup().await;
        // The host keeps answering throughout: a `note_heard` before each
        // lap is a frame arriving. Without it the fixture's unacknowledged
        // input would make the link `Silent` again two seconds in, which is
        // inside `LINGER`.
        let now = Instant::now();
        session.note_heard(now);
        let v = session.popup_at(now).expect("the popup closed at once");
        assert_eq!(v.marker, Marker::LiveAgain);

        let almost = now + crate::ui::LINGER - Duration::from_millis(1);
        session.note_heard(almost);
        assert!(session.popup_at(almost).is_some(), "closed before LINGER");

        let then = now + crate::ui::LINGER;
        session.note_heard(then);
        assert_eq!(session.popup_at(then), None, "it never closed");
    }

    #[tokio::test]
    async fn an_outage_and_its_end_are_recorded() {
        let t = Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        assert!(session.popup_at(t).is_none());
        assert_eq!(
            session.activity.entries().len(),
            0,
            "something was recorded before the outage"
        );

        session.popup_at(t + Duration::from_secs(3));
        session.note_heard(t + Duration::from_millis(4_500));
        session.popup_at(t + Duration::from_millis(4_500));

        let texts: Vec<String> = session.activity.entries().map(|e| e.text.clone()).collect();
        assert_eq!(texts, ["silent", "live again via this link, outage 4.5 s"]);
    }

    /// Two laps within a second take one sample: `Quality::push` does not
    /// pace itself, so the pacing is the session's.
    #[tokio::test]
    async fn quality_is_sampled_once_a_second_and_marks_outage_seconds() {
        let t = Instant::now();
        let (_host, mut session) = with_popup().await;
        assert!(session.quality.sparkline().is_empty());

        session.sample_quality(t);
        session.sample_quality(t + Duration::from_millis(500));
        session.sample_quality(t + Duration::from_secs(1));

        assert_eq!(
            session.quality.sparkline(),
            vec![None, None],
            "with_popup is Silent: both are outage seconds"
        );
    }

    #[tokio::test]
    async fn a_swap_starts_a_new_quality_segment() {
        let (_host, mut client) = pair("").await;
        let (_rebuilt_host, rebuilt) = crate::link::fixtures::link_pair().await;
        assert_eq!(client.quality.segment(), 1);

        client
            .swap_in(rebuilt, Instant::now())
            .expect("swapping in");

        assert_eq!(client.quality.segment(), 2);
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

    /// The caller's half of `popup_at`'s question: have we said something the
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
    async fn a_heartbeating_session_that_is_answered_never_reports_an_outage() {
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
            assert!(
                !client.outage_at(now),
                "cycle {cycle}: a popup was raised on the very lap the heartbeat \
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
            assert!(
                !client.outage_at(now),
                "cycle {cycle}: a popup survived the host answering"
            );
        }
    }

    /// A healthy session, through the REAL loop, for longer than the
    /// heartbeat: the popup may never open.
    ///
    /// Both idle-CPU guards run for two seconds, which is below
    /// `HEARTBEAT_IDLE`, so what the heartbeat does *inside* the loop was
    /// entirely unpinned -- and nothing anywhere asserted the composed
    /// property that a working session shows nothing at all. That is how C1
    /// shipped: every one of its parts passed its own review.
    ///
    /// The assertion reads the activity log, which records every outage the
    /// popup opens for: painted bytes are a diff, and a diff can split a word
    /// wherever a cell happened to be unchanged.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_healthy_session_raises_no_popup_across_several_heartbeats() {
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
        assert!(
            !client.activity.entries().any(|e| e.text == "silent"),
            "a healthy session went silent"
        );
        assert!(
            client.shown.is_none(),
            "the session ended with a popup still up"
        );
    }

    /// The loop rebuilds layer 1 only when the VIEW changes, and a resize
    /// changes not one word of it. So the resize itself has to lay the popup
    /// out again, or a `Confirming` popup -- the one the user sits and reads,
    /// because it is asking them a question -- keeps the geometry of a
    /// screen that is gone until they press a key. Twice: once to a box
    /// that still fits, and once below `MIN_BOX`, where the popup is a
    /// single line.
    #[tokio::test]
    async fn a_resize_lays_the_popup_out_again_for_the_new_screen() {
        for small in [
            TermSize { cols: 24, rows: 8 },
            TermSize { cols: 19, rows: 5 },
        ] {
            let t = std::time::Instant::now();
            let (_host, mut session) = pair("/bin/sh").await;
            session.note_heard(t);
            session.note_sent(t);

            // Raise the popup and paint it, exactly as `run_on` does --
            // including the earlier lap on which the reply started being owed.
            assert!(session.popup_at(t).is_none());
            let view = session.popup_at(t + Duration::from_secs(3)).unwrap();
            session
                .renderer
                .set_overlay(Some(layout_popup(&view, session.size)));
            session.shown = Some(view.clone());
            let mut painted = Vec::new();
            session
                .renderer
                .render(&mut painted, session.screen_rx.state())
                .unwrap();

            session.resize(small);
            let mut after = Vec::new();
            session
                .renderer
                .render(&mut after, session.screen_rx.state())
                .unwrap();

            // What a renderer that never saw the old screen paints, for the
            // same view and the same state. Equality is the assertion: the
            // popup is where the NEW screen puts it, and not where the old
            // one did.
            let mut fresh = Renderer::new(small, caps());
            fresh.set_overlay(Some(layout_popup(&view, small)));
            let mut expected = Vec::new();
            fresh
                .render(&mut expected, session.screen_rx.state())
                .unwrap();

            assert_eq!(
                String::from_utf8_lossy(&after),
                String::from_utf8_lossy(&expected),
                "the popup kept the geometry of the screen that went away ({small:?})"
            );
        }
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
        let (_host, mut session) = with_popup().await;
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
        let (_host, mut session) = with_popup().await;
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

    /// The rebind itself, in process: bind, adopt, `rebind_abstract`, settle.
    ///
    /// `Link::rebind` has an end-to-end test of its own, but it self-skips on
    /// macOS -- so on the machine this is developed on, nothing exercised the
    /// branch `follow_route` takes when a route really has moved. Everything
    /// here is real: a real quinn connection, a real socket bound the way the
    /// ladder binds one, and a real `rebind_abstract` under it.
    ///
    /// The move is manufactured the only way loopback allows: the baseline is
    /// set to an address this machine cannot be reached on, so the probe --
    /// which genuinely asks the kernel, and genuinely gets `127.0.0.1` back --
    /// disagrees with it. Nothing about `moved`, the bind or the rebind knows
    /// the difference.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_moved_route_swaps_the_socket_and_the_session_survives_it() {
        let (mut host, mut session) = with_popup().await;
        let before = session.link.socket.local_addr().expect("a bound socket");

        // A baseline the loopback probe cannot agree with.
        session.route =
            crate::roam::RouteWatch::new(Some("10.46.18.101".parse().expect("a literal address")));

        assert!(
            session.follow_route(Instant::now()),
            "a probe that disagreed with the baseline did not rebind"
        );
        let after = session.link.socket.local_addr().expect("a bound socket");
        assert_ne!(
            before, after,
            "`follow_route` reported a rebind without swapping the socket"
        );

        // Settled, so the same reading does not ask again on the next probe.
        assert!(
            !session.follow_route(Instant::now() + ROUTE_PROBE_EVERY * 2),
            "the same route change asked for a second rebind"
        );

        // And the session still works over the new socket. This is the half a
        // socket-address assertion cannot reach: QUIC migration is what makes
        // the swap survivable, so output produced after it has to arrive.
        let mut out = Vec::new();
        host.term
            .write_input(b"printf 'after-the-rebind\\r\\n'\n")
            .expect("write");
        assert!(
            drive(
                &mut host,
                &mut session,
                &mut out,
                Duration::from_secs(20),
                |_, c| text(c.screen()).contains("after-the-rebind")
            )
            .await,
            "nothing arrived after the rebind: the connection did not migrate \
             onto the new socket\n--- client ---\n{}",
            text(session.screen())
        );
    }

    /// The probe pace belongs to one outage, not to the session.
    ///
    /// `probed_at` is what keeps `Silent` from probing on every lap of a loop
    /// that wakes 125 times a second. Left set across a return to `Live` it
    /// would also swallow the FIRST probe of the next outage, whenever that
    /// outage began within `ROUTE_PROBE_EVERY` of the previous probe -- and
    /// the first probe is the one that matters, because it is what notices a
    /// route that has just moved.
    ///
    /// The honest scope of this guard: that skip is not reachable through
    /// today's constants, because a second `Silent` costs another
    /// `SILENT_AFTER` (2 s) and the cooldown is `ROUTE_PROBE_EVERY` (1 s). So
    /// this asserts the mechanism directly -- a lap that is not `Silent`
    /// leaves no pace behind -- rather than staging a timeline that needs one
    /// of those two constants, which live in different modules, to move first.
    #[tokio::test]
    async fn a_healthy_lap_leaves_no_probe_pace_behind_it() {
        let (_host, mut session) = with_popup().await;
        let t = Instant::now();

        session.follow_route(t);
        assert!(
            session.probed_at.is_some(),
            "the outage never probed, so this asserts nothing about clearing"
        );

        // The host answers. The loop calls `follow_route` on healthy laps too,
        // and this is the only thing it does on one.
        session.note_heard(t);
        assert!(
            !session.follow_route(t),
            "probed a healthy link; a rebind on a working path breaks it"
        );
        assert!(
            session.probed_at.is_none(),
            "the ended outage left its cooldown behind: the next outage would \
             skip the first probe -- the one that notices a moved route -- if \
             it began inside ROUTE_PROBE_EVERY"
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
    /// and what failed before `notice_at` (now `popup_at`) was moved inside
    /// the loop below: a silence long enough to have been fatal raises the
    /// outage popup and the outage ends on its own once the host answers.
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
            // `run`'s own loop calls `popup_at` once per lap (see the
            // `Wake::Due` arm above) -- that is what advances `LinkState`'s
            // grace-period clock. `evaluate` is edge-triggered on being
            // asked, not on wall-clock time passing underneath it, so a loop
            // that never asks would still see `Live` on its first question
            // 35s in and this composed test would prove nothing.
            let _ = client.outage_at(Instant::now());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(
            client.outage_at(Instant::now()),
            "no popup after {outage:?} of silence"
        );
        // Sanity, not the regression guard -- see the doc comment above for
        // why this cannot be made to fail by restoring the old timeout here.
        assert!(
            client.link.sink.connection().close_reason().is_none(),
            "the connection died under the popup: {:?}",
            client.link.sink.connection().close_reason()
        );

        // The host comes back. The popup must come down on its own -- through
        // the scavenging path Task 1 fixed, since nothing here wakes a frame arm.
        host.turn().expect("the host answers at last");
        tokio::time::sleep(Duration::from_millis(200)).await;
        client
            .turn(&[], &mut out)
            .expect("a lap that scavenges the frame");

        assert!(
            !client.outage_at(Instant::now()),
            "the host answered and the popup stayed up"
        );
    }

    /// A short, **non-`#[ignore]`d** sibling of the test above, so CI runs
    /// the real-clock half of the composed-test story on every default
    /// `cargo test`, not only when someone remembers `--ignored`.
    ///
    /// Every other popup test in this file injects instants and holds the
    /// clock still. This one and the 35s test above are the only two that
    /// let `LinkState::evaluate`'s edge-triggered `owed_since` and the
    /// scavenging clear path run against a real clock. This one starts from
    /// a genuinely SYNCED session (the host acks the client's first input
    /// before the silence begins), so the popup's later rise is driven by
    /// `heartbeat_due` firing at `HEARTBEAT_IDLE` and then `SILENT_AFTER`
    /// more before that owing is old enough to report -- roughly 7s in the
    /// worst case -- rather than by the from-construction seq mismatch every
    /// fresh `ClientSession` starts with, which is what the 35s test above
    /// relies on instead (silence from the very first lap of the session,
    /// a different and equally real case, but not one that exercises
    /// `heartbeat_due` at all). 9s of real polling budgets both real timers
    /// with margin for a loaded runner.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_short_silence_raises_and_ends_the_outage_on_a_real_clock() {
        let (mut host, mut client) = pair("/bin/sh").await;
        let mut out = Vec::new();

        // Get to a genuinely synced state before the silence starts: the
        // client speaks once, the host applies it and answers, and the
        // client applies that answer -- so `input_tx.current().seq() ==
        // screen_rx.peer_ack()` and nothing is owed, exactly as a healthy
        // attach looks. Without this the popup would rise from the same
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
            !client.outage_at(Instant::now()),
            "not synced before the silence began; this test would prove \
             nothing about HEARTBEAT_IDLE"
        );

        let t = Instant::now();
        client.note_heard(t);
        client.note_sent(t);

        // HEARTBEAT_IDLE (5s) until the client's own heartbeat makes a reply
        // owed, plus SILENT_AFTER (2s) more before that owing is old enough
        // to raise the popup -- 9s of real polling budgets both with
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
            let _ = client.outage_at(Instant::now());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(
            client.outage_at(Instant::now()),
            "no popup after {outage:?}, well past HEARTBEAT_IDLE + SILENT_AFTER"
        );

        // The host answers again; the popup must clear through the same
        // scavenging path Task 1 fixed.
        //
        // BOTH sides turn, and the wait is for the popup to go rather than
        // for a fixed sleep to elapse. One `host.turn()` after a 200 ms sleep
        // failed intermittently, and not for the reason a sleep usually fails:
        // nine seconds of laps against a host that never turned fill
        // `FrameSource`'s 64-frame channel on the host side, whose reader task
        // then blocks in `send().await` and stops calling `read_datagram`, so
        // everything the client sent afterwards -- the heartbeat that made the
        // reply owed included -- was dropped before the host could see it. The
        // single turn then drained sixty-odd frames from seconds ago, had
        // nothing newer to acknowledge, and correctly sent nothing at all: the
        // popup stayed up because the round trip really was incomplete. That
        // is the fixture, not the behaviour -- a real host turns every lap and
        // never lets a backlog build -- so the wait drives both ends until the
        // client's current sequence number reaches the host.
        //
        // `applied` is carried into the message because it separates the two
        // failures worth telling apart: nothing ever arrived (the fixture
        // wedged again), or a frame landed and the popup stayed up anyway --
        // the defect this test exists for. Diagnosing the CI failure took one
        // run of this message and no reproduction.
        let mut applied = 0;
        let clear_by = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < clear_by {
            host.turn().expect("the host answers at last");
            applied += client
                .turn(&[], &mut out)
                .expect("a lap that scavenges the frame")
                .applied;
            if !client.outage_at(Instant::now()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            !client.outage_at(Instant::now()),
            "the host answered and the popup stayed up through 20s of \
             scavenging laps, {applied} frames applied"
        );
    }

    /// The answer an IDLE session gets back is an ack and nothing else.
    ///
    /// A host whose screen has not moved answers a heartbeat with an ACK-ONLY
    /// frame, which by construction repeats its own state number --
    /// `Receiver::on_frame` records the ack from it and reports `Ok(false)`,
    /// because there is nothing to apply and applying a duplicate would be
    /// wrong. That frame is still proof the host is alive, and on a session
    /// where nobody is typing it is the ONLY proof there will ever be:
    /// nothing else is coming until the screen changes.
    ///
    /// `LinkState::heard` is documented as "a frame arrived", and only it can
    /// leave `Silent` -- `evaluate` returns early there so the counter does
    /// not restart every lap. So a client that calls it only for frames that
    /// APPLY sits under "no reply from host" for ever on exactly the session
    /// that has recovered, with `owed` false and the box still up.
    ///
    /// Found through the real-clock test above, which failed intermittently on
    /// this and almost always on CI: it passes only when the shell's echo is
    /// still in flight and the answer happens to carry a diff.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_answer_that_applies_nothing_still_ends_the_outage() {
        // No script: the shell's own prompt is then the last thing that
        // changes the screen, and the session settles for good.
        let (mut host, mut client) = pair("").await;
        let mut out = Vec::new();

        // Wait for the shell to draw SOMETHING -- deliberately not for a `$`.
        //
        // This used to wait for `contains('$')`, which is a property of the
        // PROMPT and therefore of whoever happens to run the tests. `/bin/sh`
        // is bash in sh mode and honours an exported `PS1` it inherits, so on
        // a machine whose prompt is `❯ ` or `% ` the shell drew its prompt
        // perfectly well and this test sat through the entire 20 s never
        // recognising it, then reported that nothing had settled. It failed
        // every run on the user's machine and none of 36 here, which is what
        // an environment-dependent test looks like when only one machine has
        // the environment -- it reads as a flake and is not one. Measured on
        // the tree this replaces: PS1='❯ ' and PS1='% ' both fail in 20.03 s,
        // PS1='$ ' passes in 0.70 s.
        //
        // A non-blank screen is all this step actually needs. It is the
        // settle loop below that proves the shell has FINISHED talking, which
        // is the precondition the test is really after, and every shell draws
        // something to a pty it owns.
        assert!(
            drive(
                &mut host,
                &mut client,
                &mut out,
                Duration::from_secs(20),
                |_, c| !text(c.screen()).trim().is_empty()
            )
            .await,
            "the client never saw the shell draw anything, so nothing has settled"
        );

        // Settled means the host has run out of things to say: the prompt has
        // been acknowledged and the acknowledgement itself carried across. Two
        // consecutive quiet laps, not one, because the first quiet lap can be
        // followed by the frame that carries the client's ack of it. The laps
        // are 150 ms apart, comfortably past `pacing_interval`'s 100 ms
        // ceiling, so "sent nothing" means "had nothing to send" rather than
        // "was asked too early". Without this the answer below would carry a
        // diff and the test would guard the applied path it exists to avoid.
        let mut quiet = 0;
        let settled_by = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < settled_by && quiet < 2 {
            if host.turn().expect("a host lap").sent.is_none() {
                quiet += 1;
            } else {
                quiet = 0;
            }
            client.turn(&[], &mut out).expect("a client lap");
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert_eq!(
            quiet, 2,
            "the host never ran out of diffs; its answer would not be ack-only"
        );

        // A heartbeat moves the client's sequence number, so an answer is owed
        // -- and nothing on the host's screen will change to carry it.
        let t = Instant::now();
        client.note_heard(t);
        client.note_sent(t);
        assert!(
            client.heartbeat(t + crate::linkstate::HEARTBEAT_IDLE),
            "no heartbeat went out, so nothing is owed and this proves nothing"
        );
        client
            .turn(&[], &mut out)
            .expect("the lap that sends the heartbeat");

        // The lap the owing begins on: the grace period is measured from when
        // the reply started being owed, so a fixture that jumped straight to
        // `SILENT_AFTER` would be asking about an owing that had only just
        // started.
        let sent_at = t + crate::linkstate::HEARTBEAT_IDLE;
        assert!(
            !client.outage_at(sent_at),
            "the popup went up the instant the heartbeat did, before the grace period"
        );
        let raised = sent_at + crate::linkstate::SILENT_AFTER;
        assert!(client.outage_at(raised), "the fixture raised no popup");

        let mut applied = 0;
        let mut answers = 0;
        let clear_by = tokio::time::Instant::now() + Duration::from_secs(20);
        while tokio::time::Instant::now() < clear_by {
            if host.turn().expect("a host lap").sent.is_some() {
                answers += 1;
            }
            applied += client
                .turn(&[], &mut out)
                .expect("a lap that scavenges the answer")
                .applied;
            if !client.outage_at(raised) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            !client.outage_at(raised),
            "the host sent {answers} answers and the popup stayed up; \
             {applied} of them applied"
        );
    }

    /// Spec §7, end to end: the link goes dark under a running session, the
    /// popup opens by itself and says so on the screen, and when the link
    /// comes back it says that too -- with nothing written to the terminal
    /// outside the renderer at any point.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_outage_raises_the_popup_and_its_end_is_reported() {
        let (mut host, mut client, relay) = pair_through_relay_sized("", BIG).await;
        let host_loop = tokio::spawn(async move { host.run().await });
        let (keys, mut typing) = keyboard();
        let out = SharedOut::default();
        let client_loop = tokio::spawn({
            let mut out = out.clone();
            async move {
                let code = client.run_on(keys, &mut out).await;
                (code, client)
            }
        });

        typing
            .write_all(b"printf 'ready-%s\\n' ok\n")
            .expect("type");
        wait_for_screen(&out, BIG, "ready-ok", Duration::from_secs(10)).await;
        assert!(
            !screen_of(&out, BIG).contains("SILENT"),
            "the popup was up before the outage"
        );

        relay.blackhole(true);
        // Sent while the phase is still `Live`, so a reply is owed and the
        // silence is noticed.
        typing.write_all(b"true\n").expect("type");
        wait_for_screen(&out, BIG, "\u{25cf} SILENT", Duration::from_secs(10)).await;

        relay.blackhole(false);
        wait_for_screen(&out, BIG, "LIVE again", Duration::from_secs(20)).await;

        // The lingering popup takes every key; it closes by itself after
        // `LINGER`, and only then does typing reach the shell.
        wait_off_screen(&out, BIG, "LIVE again", Duration::from_secs(10)).await;
        typing.write_all(b"exit 4\n").expect("type");
        let (code, client) = tokio::time::timeout(Duration::from_secs(20), client_loop)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the client never finished; the screen was:\n{}",
                    screen_of(&out, BIG)
                )
            })
            .expect("client task");
        assert_eq!(code.expect("the client loop failed"), 4);
        let host_code = tokio::time::timeout(Duration::from_secs(20), host_loop)
            .await
            .expect("the host never finished")
            .expect("host task")
            .expect("host loop");
        assert_eq!(host_code, 4);

        let link: Vec<String> = client
            .activity
            .entries()
            .filter(|e| e.kind == crate::activity::Kind::Link)
            .map(|e| e.text.clone())
            .collect();
        assert_eq!(link.first().map(String::as_str), Some("silent"), "{link:?}");
        assert!(
            link.get(1)
                .is_some_and(|t| t.starts_with("live again via this link, outage ")),
            "{link:?}"
        );
        let stray = outside_renderer(&out.bytes());
        assert!(
            stray.is_empty(),
            "written outside the renderer: {:?}",
            String::from_utf8_lossy(&stray)
        );
    }

    /// Review focus 2 and 3, end to end: the user closes the outage popup
    /// and types blind; the popup stays shut for the rest of that outage,
    /// and when the link answers it opens on the question, whose bare `s`
    /// delivers the typing to the shell.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn blind_typing_after_closing_the_popup_is_asked_about_and_sent() {
        let (mut host, mut client, relay) = pair_through_relay_sized("", BIG).await;
        let host_loop = tokio::spawn(async move { host.run().await });
        let (keys, mut typing) = keyboard();
        let out = SharedOut::default();
        let client_loop = tokio::spawn({
            let mut out = out.clone();
            async move {
                let code = client.run_on(keys, &mut out).await;
                (code, client)
            }
        });

        typing
            .write_all(b"printf 'ready-%s\\n' ok\n")
            .expect("type");
        wait_for_screen(&out, BIG, "ready-ok", Duration::from_secs(10)).await;

        relay.blackhole(true);
        typing.write_all(b"true\n").expect("type");
        wait_for_screen(&out, BIG, "\u{25cf} SILENT", Duration::from_secs(10)).await;

        close_the_popup(&mut typing, &out, BIG).await;
        typing
            .write_all(b"printf 'blind-%s\\n' ok\n")
            .expect("type");
        // Ten laps at least, all in the outage the popup was closed in:
        // none of them may open it again.
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            !screen_of(&out, BIG).contains("SILENT"),
            "the popup opened itself again for the outage it was closed in:\n{}",
            screen_of(&out, BIG)
        );
        assert!(
            !screen_of(&out, BIG).contains("blind-ok"),
            "blind typing reached the shell during the outage"
        );

        relay.blackhole(false);
        // The question shows the typing as typed (`blind-%s`); `blind-ok`
        // appears only once the shell has run it.
        wait_for_screen(
            &out,
            BIG,
            "deliver what you typed?",
            Duration::from_secs(20),
        )
        .await;
        assert!(
            !screen_of(&out, BIG).contains("blind-ok"),
            "the typing was delivered before it was asked about"
        );
        // The guard runs from the lap that first saw the question, which was
        // before it was painted, so waiting it out from here is a lower
        // bound, not a race. Nothing on the screen marks its end.
        tokio::time::sleep(crate::ui::ANSWER_GUARD).await;
        typing.write_all(b"s").expect("type");
        wait_for_screen(&out, BIG, "blind-ok", Duration::from_secs(10)).await;
        wait_off_screen(
            &out,
            BIG,
            "deliver what you typed?",
            Duration::from_secs(10),
        )
        .await;

        typing.write_all(b"exit 6\n").expect("type");
        let (code, client) = tokio::time::timeout(Duration::from_secs(20), client_loop)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the client never finished; the screen was:\n{}",
                    screen_of(&out, BIG)
                )
            })
            .expect("client task");
        assert_eq!(code.expect("the client loop failed"), 6);
        let host_code = tokio::time::timeout(Duration::from_secs(20), host_loop)
            .await
            .expect("the host never finished")
            .expect("host task")
            .expect("host loop");
        assert_eq!(host_code, 6);

        // Activity folds identical entries, so a second send would show as
        // `repeats == 1`, not as a second entry.
        let input: Vec<(String, u32)> = client
            .activity
            .entries()
            .filter(|e| e.kind == crate::activity::Kind::Input)
            .map(|e| (e.text.clone(), e.repeats))
            .collect();
        assert_eq!(input, [("held input sent (23 bytes)".to_string(), 0)]);
        // The shell ran the held line exactly once: counted on the final
        // screen, after everything that could have run it a second time.
        let final_screen = text(client.screen());
        assert_eq!(
            final_screen.matches("blind-ok").count(),
            1,
            "the held line did not run exactly once:\n{final_screen}"
        );
        let stray = outside_renderer(&out.bytes());
        assert!(
            stray.is_empty(),
            "written outside the renderer: {:?}",
            String::from_utf8_lossy(&stray)
        );
    }

    /// A quick double `Ctrl-\` is one literal `Ctrl-\` for the remote
    /// program, through the real keyboard path. `stty -isig -echo` makes the
    /// shell's `cat -v` print exactly what arrives, once: two literals would
    /// read `^\^\`, none would read nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_quick_double_ctrl_backslash_reaches_the_shell_as_one_literal() {
        let (mut host, mut client) = pair("").await;
        let host_loop = tokio::spawn(async move { host.run().await });
        let (keys, mut typing) = keyboard();
        let out = SharedOut::default();
        let client_loop = tokio::spawn({
            let mut out = out.clone();
            async move {
                let code = client.run_on(keys, &mut out).await;
                (code, client)
            }
        });

        // `ready-ok` is printed after `stty` has run, so once it is on the
        // screen nothing typed afterwards can be eaten as a signal.
        typing
            .write_all(b"stty -isig -echo; printf 'ready-%s\\n' ok; cat -v\n")
            .expect("type");
        wait_for_screen(&out, size(), "ready-ok", Duration::from_secs(10)).await;
        assert!(
            !screen_of(&out, size()).contains("^\\"),
            "a literal arrived before it was typed"
        );

        // One read: both presses carry the same timestamp, well inside the
        // window.
        typing
            .write_all(&[CTRL_BACKSLASH, CTRL_BACKSLASH, b'\n'])
            .expect("type");
        wait_for_screen(&out, size(), "^\\", Duration::from_secs(10)).await;

        // EOF ends `cat`; `-isig` leaves VEOF working.
        typing.write_all(b"\x04exit 5\n").expect("type");
        let (code, client) = tokio::time::timeout(Duration::from_secs(20), client_loop)
            .await
            .expect("the client never finished")
            .expect("client task");
        assert_eq!(code.expect("the client loop failed"), 5);
        let host_code = tokio::time::timeout(Duration::from_secs(20), host_loop)
            .await
            .expect("the host never finished")
            .expect("host task")
            .expect("host loop");
        assert_eq!(host_code, 5);

        let screen = text(client.screen());
        assert!(screen.lines().any(|l| l.trim_end() == "^\\"), "{screen}");
        assert!(!screen.contains("^\\^\\"), "two literals arrived: {screen}");
        assert_eq!(
            screen.matches("^\\").count(),
            1,
            "not exactly one literal on the final screen: {screen}"
        );
        assert!(client.shown.is_none(), "the double press left the popup up");
    }
}

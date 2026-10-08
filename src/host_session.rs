//! The remote half's loop: the shell's pty, the authoritative screen, and the
//! link to whichever client is attached.
//!
//! Split out of `session.rs`, which keeps the client's loop and what the two
//! halves share (`Turn`, the close reasons). The session switcher grows this
//! side -- a lobby, a kill -- and does it here rather than in a module that
//! was already ten thousand lines long.

// The host daemon's stderr is not a terminal anybody is looking at, but the
// rule is the same as the client's so nothing here can start printing by
// accident. The one deliberate exception is `#[expect]`-ed at its call site.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use oxutrm_proto::{Frame, ScreenState, TermSize};
use oxutrm_sync::{InputState, Receiver};
use oxutrm_term::HostTerm;

use crate::link::{Link, SendOutcome};
use crate::session::{IDLE_POLL, SHELL_EXITED, SUPERSEDED, SWITCHED, TAKEN_OVER, Turn};

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

/// How long a hung-up shell has to exit before its process group is killed
/// (switcher spec §3.4).
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(3);

/// How long a killed shell is waited for. A SIGKILLed process is reaped at
/// once in practice; this bounds the wait for one that cannot be.
const REAP_AFTER_KILL: Duration = Duration::from_secs(2);

/// Whether a session has a client right now, for the door to answer
/// `Myself` and `Sessions` without asking the loop (switcher spec §2.2).
///
/// The loop writes it -- the connection it serves and when it last heard a
/// frame -- and the door reads it, under a lock held for a copy. The answer
/// is the one [`HostSession::turn_at`] gives itself: open, and heard from
/// within [`DETACH_AFTER`].
#[derive(Clone, Default)]
pub(crate) struct Presence(std::sync::Arc<std::sync::Mutex<Option<(quinn::Connection, Instant)>>>);

impl Presence {
    fn heard(&self, conn: &quinn::Connection, at: Instant) {
        if let Ok(mut p) = self.0.lock() {
            *p = Some((conn.clone(), at));
        }
    }

    /// Whether a client is attached at `now`.
    pub(crate) fn attached(&self, now: Instant) -> bool {
        let Ok(p) = self.0.lock() else {
            return false;
        };
        p.as_ref().is_some_and(|(conn, at)| {
            conn.close_reason().is_none() && now.saturating_duration_since(*at) < DETACH_AFTER
        })
    }
}

/// What the door asks of the loop: the two things only the owner of the
/// shell can do (switcher spec §2.2, §3.4).
pub(crate) enum LoopCmd {
    /// A lobby was asked for `New` and is registered: start the shell. The
    /// reply says whether it started.
    StartShell {
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Kill the shell ([`HostSession::hang_up_shell`]) and reply with its
    /// status. Then, for its own client's request, become a lobby; for a
    /// sibling's (`end`), wait for `written` -- the door's `Done`, written
    /// after it removed the registry entry -- and end as a shell that exited.
    Kill {
        end: bool,
        reply: tokio::sync::oneshot::Sender<i32>,
        written: tokio::sync::oneshot::Receiver<()>,
    },
}

/// How long a session killed by a sibling waits for its door to have
/// answered the sibling before it closes its own client's link.
const DONE_WRITTEN_WITHIN: Duration = Duration::from_secs(2);

/// The remote half: owns the PTY and the authoritative screen.
///
/// Or, as a **lobby**, no PTY at all (switcher spec §2.1): the same link,
/// the same sync loop answering the client's frames with a blank screen, and
/// no shell. A connect-time lobby starts that way ([`HostSession::lobby`]);
/// a session whose shell was killed on its client's request becomes one. A
/// lobby ends when its link closes or its client has been silent for
/// [`DETACH_AFTER`]; asked for `New`, it starts its shell and is a session.
pub struct HostSession {
    /// `None` is a lobby.
    term: Option<HostTerm>,
    /// The shell a lobby starts, and how.
    shell: String,
    start: oxutrm_term::Start,
    scrollback: usize,
    screen_tx: oxutrm_sync::Sender<ScreenState>,
    /// `pub(crate)` for the session tests, which drive both halves.
    pub(crate) input_rx: Receiver<InputState>,
    /// `pub(crate)` for the session tests, which drive both halves.
    pub(crate) link: Link,
    /// `pub(crate)` for the session tests, which drive both halves.
    pub(crate) size: TermSize,
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
    /// `last_heard` and the link it was heard on, for the door.
    presence: Presence,
    /// How long a client may be silent before the session counts as
    /// detached -- and a lobby ends. [`DETACH_AFTER`]; a field so a test can
    /// wait for it in a second.
    detach_after: Duration,
}

impl HostSession {
    /// Start a shell and serve it over `link`.
    ///
    /// `TERM` and `COLORTERM` come from [`oxutrm_term::negotiate_term`], which
    /// takes no arguments on purpose.
    ///
    /// `start` says where and how: a session's is a login shell in `$HOME`
    /// (switcher spec §3.4); the tests' is a plain `/bin/sh`.
    #[cfg(test)]
    pub fn spawn(
        shell: &str,
        start: &oxutrm_term::Start,
        size: TermSize,
        scrollback: usize,
        link: Link,
    ) -> Result<HostSession> {
        let mut session = HostSession::lobby(shell, start, size, scrollback, link)?;
        session.start_shell()?;
        Ok(session)
    }

    /// A lobby over `link`: everything a session has but the shell, which
    /// [`HostSession::start_shell`] starts as `shell` and `start` say.
    pub(crate) fn lobby(
        shell: &str,
        start: &oxutrm_term::Start,
        size: TermSize,
        scrollback: usize,
        link: Link,
    ) -> Result<HostSession> {
        let blank = ScreenState::blank(size.rows, size.cols)?;
        let empty = InputState {
            seq: 1,
            pending: Vec::new(),
            size,
        };

        Ok(HostSession {
            term: None,
            shell: shell.to_owned(),
            start: start.clone(),
            scrollback,
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
            presence: Presence::default(),
            detach_after: DETACH_AFTER,
        })
    }

    /// Start the shell, at the session's current size: a lobby becomes a
    /// session. Its first screen goes out on the next turn.
    pub(crate) fn start_shell(&mut self) -> Result<()> {
        let (term_name, colorterm) = oxutrm_term::negotiate_term();
        let mut env = vec![("TERM".to_owned(), term_name)];
        if let Some(ct) = colorterm {
            env.push(("COLORTERM".to_owned(), ct));
        }
        let term = HostTerm::spawn_with(
            &self.shell,
            &[],
            &env,
            self.size,
            self.scrollback,
            &self.start,
        )
        .context("starting the shell on a pty")?;
        self.term = Some(term);
        self.written = self.input_rx.state().pending.len();
        self.screen_stale = true;
        Ok(())
    }

    /// Whether this is a lobby: no shell.
    pub(crate) fn is_lobby(&self) -> bool {
        self.term.is_none()
    }

    /// The shell is gone on request: the session is a lobby from here, and
    /// its client sees a blank screen.
    fn become_lobby(&mut self) -> Result<()> {
        self.term = None;
        self.screen_tx
            .update(ScreenState::blank(self.size.rows, self.size.cols)?);
        self.screen_stale = false;
        Ok(())
    }

    /// Wait for a silent client for `d` instead of [`DETACH_AFTER`].
    #[cfg(test)]
    pub(crate) fn with_detach_after(mut self, d: Duration) -> HostSession {
        self.detach_after = d;
        self
    }

    /// Share whether a client is attached with `presence`, the door's copy.
    pub(crate) fn with_presence(mut self, presence: Presence) -> HostSession {
        presence.heard(self.link.sink.connection(), self.last_heard);
        self.presence = presence;
        self
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
            self.presence.heard(self.link.sink.connection(), now);
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
        let quiet_too_long = now.duration_since(self.last_heard) >= self.detach_after;
        let attached = !closed && !quiet_too_long;
        turn.detached = !attached;

        // ---- the terminal --------------------------------------------------
        // A lobby has none: its screen is the blank one it was given.
        if let Some(term) = self.term.as_mut() {
            let moved = term.poll().context("draining the pty")?;
            if attached {
                if moved || self.screen_stale {
                    // The sequence number is a placeholder; `update` mints
                    // the real one, keeping numbering in exactly one place.
                    let snapshot = term.snapshot(1);
                    self.screen_tx.update(snapshot);
                    self.screen_stale = false;
                }
            } else if moved {
                self.screen_stale = true;
            }
        }

        // ---- outbound: the screen ------------------------------------------
        if attached {
            turn.sent = self.offer_frame();
        }
        turn.exited = self.term.as_mut().and_then(HostTerm::child_exited);
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
            // A lobby has nowhere to put typing: it is taken and dropped.
            if let Some(term) = self.term.as_mut() {
                term.write_input(&fresh).context("writing to the pty")?;
            }
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
        match self.term.as_mut() {
            Some(term) => term.resize(size).context("resizing the pty")?,
            // A lobby's screen is blank at whatever size its client is.
            None => self
                .screen_tx
                .update(ScreenState::blank(size.rows, size.cols)?),
        }
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
    #[cfg(test)]
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
    #[cfg(test)]
    pub async fn run_with_attaches(
        &mut self,
        attaches: &mut tokio::sync::mpsc::Receiver<crate::attach_exchange::Attached>,
    ) -> Result<i32> {
        let (tx, mut cmds) = tokio::sync::mpsc::channel(1);
        // Dropped, for the reason `run` drops its sender.
        drop(tx);
        self.run_with_doors(attaches, &mut cmds).await
    }

    /// [`HostSession::run_with_attaches`], plus what the door asks of the
    /// loop ([`LoopCmd`]): start a lobby's shell, kill this one's.
    ///
    /// Returns the shell's exit status, or `0` for a lobby that ended -- its
    /// link closed, or its client silent for [`DETACH_AFTER`].
    pub(crate) async fn run_with_doors(
        &mut self,
        attaches: &mut tokio::sync::mpsc::Receiver<crate::attach_exchange::Attached>,
        cmds: &mut tokio::sync::mpsc::Receiver<LoopCmd>,
    ) -> Result<i32> {
        // The shell's descriptors, while there is a shell; watched again
        // whenever the loop starts or ends one.
        let mut watch = self.watch()?;

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
            if self
                .term
                .as_ref()
                .is_some_and(HostTerm::more_output_waiting)
            {
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

            // A lobby has nothing to outlive its client for: it ends with its
            // link, or after `detach_after` of silence. Its own two arms,
            // armed only in a lobby -- a session must outlive a vanished
            // client, which is the entire point (see below).
            let lobby = self.is_lobby();
            let conn = self.link.sink.connection().clone();
            let silent_until = tokio::time::Instant::from_std(self.last_heard + self.detach_after);

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
                r = async { watch.as_ref().expect("armed").output.readable().await },
                    if watch.is_some() => match r {
                    // Cleared HERE, having just established above that the PTY
                    // came up empty. Read-then-clear is the ordering `try_io`
                    // uses, and clearing while bytes remain would stall the
                    // screen until the child happened to write again.
                    Ok(mut g) => { g.clear_ready(); HostWake::Pty }
                    Err(e) => return Err(e).context("waiting on the pty"),
                },
                r = async { watch.as_ref().and_then(|w| w.exit.as_ref()).expect("armed").readable().await },
                    if watch.as_ref().is_some_and(|w| w.exit.is_some()) => match r {
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
                Some(c) = cmds.recv() => HostWake::Cmd(c),
                _ = conn.closed(), if lobby => HostWake::LobbyOver,
                () = tokio::time::sleep_until(silent_until), if lobby => HostWake::LobbyOver,
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
                HostWake::LobbyOver => {
                    let closed = self.link.sink.connection().close_reason().is_some();
                    let silent = self.last_heard.elapsed() >= self.detach_after;
                    if self.is_lobby() && (closed || silent) {
                        if let Some(parked) = standby.take() {
                            parked
                                .sink
                                .connection()
                                .close(quinn::VarInt::from_u32(0), SUPERSEDED);
                        }
                        return Ok(0);
                    }
                }
                HostWake::Cmd(LoopCmd::StartShell { reply }) => {
                    let started = if self.is_lobby() {
                        self.start_shell().map_err(|e| format!("{e:#}"))
                    } else {
                        Err("this session already has a shell".to_string())
                    };
                    let ok = started.is_ok();
                    let _ = reply.send(started);
                    if ok {
                        watch = self.watch()?;
                    }
                }
                HostWake::Cmd(LoopCmd::Kill {
                    end,
                    reply,
                    written,
                }) => {
                    if self.is_lobby() {
                        let _ = reply.send(-1);
                        continue;
                    }
                    let code = self.hang_up_shell(KILL_GRACE).await;
                    // The parked standby belonged to this shell's client;
                    // it goes as a standby goes when the shell exits.
                    if let Some(parked) = standby.take() {
                        close_as_exited(parked.sink.connection(), code);
                    }
                    if end {
                        // A sibling asked: end as a shell that exited, once
                        // the door has told the sibling it is done.
                        let _ = reply.send(code);
                        let _ = tokio::time::timeout(DONE_WRITTEN_WITHIN, written).await;
                        self.finish(code).await;
                        return Ok(code);
                    }
                    // Our own client asked: a lobby from here.
                    self.become_lobby()?;
                    watch = None;
                    let _ = reply.send(code);
                }
            }
        }
    }

    /// The shell's descriptors to wait on, or `None` in a lobby.
    ///
    /// Duplicated out of the terminal so the loop's arms borrow locals
    /// rather than `self`, which is what lets the body call `&mut self`
    /// methods afterwards (C1). A `dup` shares the file description,
    /// harmless here in a way it is NOT for the client's keyboard: this
    /// description is ours and we set its `O_NONBLOCK` ourselves in
    /// `Pty::spawn`.
    fn watch(&self) -> Result<Option<Watch>> {
        let Some(term) = self.term.as_ref() else {
            return Ok(None);
        };
        let output = term.output_fd().try_clone_to_owned()?;
        let output = tokio::io::unix::AsyncFd::with_interest(output, tokio::io::Interest::READABLE)
            .context("waiting on the pty")?;
        let exit = match term.exit_wake().as_fd() {
            Some(fd) => Some(
                tokio::io::unix::AsyncFd::with_interest(
                    fd.try_clone_to_owned()?,
                    tokio::io::Interest::READABLE,
                )
                .context("waiting on the child")?,
            ),
            // Already gone when it was watched. The first turn reports the
            // exit before anything waits, so there is nothing to miss.
            None => None,
        };
        Ok(Some(Watch { output, exit }))
    }

    /// End the shell on request: hang it up as a closing terminal would, and
    /// SIGKILL its process group if it is still there after `grace`
    /// ([`KILL_GRACE`] in the session). Returns its exit status once it is
    /// reaped -- or `-1` for one that could not be reaped even after the
    /// kill, which a session ends on all the same.
    ///
    /// Drains the pty while it waits: on macOS a child killed while writing
    /// to a pty is not reaped until its output is read (`Pty::reap`).
    pub(crate) async fn hang_up_shell(&mut self, grace: Duration) -> i32 {
        let Some(term) = self.term.as_mut() else {
            return -1;
        };
        // A shell that is already gone is not signalled: its pid may be
        // someone else's by now.
        if let Some(code) = term.child_exited() {
            return code;
        }
        term.hang_up();
        let start = tokio::time::Instant::now();
        let mut killed = false;
        loop {
            let _ = term.poll();
            if let Some(code) = term.child_exited() {
                return code;
            }
            let waited = start.elapsed();
            if !killed && waited >= grace {
                term.kill_group();
                killed = true;
            }
            if killed && waited >= grace + REAP_AFTER_KILL {
                return -1;
            }
            tokio::time::sleep(IDLE_POLL).await;
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
        if let Some(term) = self.term.as_mut()
            && term.poll().unwrap_or(false)
        {
            let snapshot = term.snapshot(1);
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

    /// The shell's terminal, for the session tests, which type into it and
    /// read it directly.
    #[cfg(test)]
    pub(crate) fn term_mut(&mut self) -> &mut HostTerm {
        self.term.as_mut().expect("a session, not a lobby")
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
        self.presence
            .heard(self.link.sink.connection(), self.last_heard);
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
    /// The door asked for something only the loop can do.
    Cmd(LoopCmd),
    /// A lobby's link closed, or its client may have been silent too long.
    LobbyOver,
}

/// The shell's two descriptors, as the loop waits on them.
struct Watch {
    output: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    exit: Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::fixtures::link_pair;

    fn size() -> TermSize {
        TermSize {
            cols: 120,
            rows: 40,
        }
    }

    /// A "shell" that ignores SIGHUP, as a script file the session runs. It
    /// creates `ready` once its trap is set, so a test never races the trap.
    fn stubborn_shell(dir: &std::path::Path) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("stubborn");
        let ready = dir.join("ready");
        let script = format!(
            "#!/bin/sh\ntrap '' HUP\n: > '{}'\nwhile :; do sleep 1; done\n",
            ready.display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn a_hung_up_shell_ends_at_once() {
        let (host_link, _client) = link_pair().await;
        let mut host = HostSession::spawn(
            "/bin/sh",
            &oxutrm_term::Start::default(),
            size(),
            200,
            host_link,
        )
        .unwrap();
        let begun = Instant::now();
        let code = host.hang_up_shell(KILL_GRACE).await;
        assert_eq!(code, 128 + 1, "ended by the SIGHUP");
        assert!(begun.elapsed() < KILL_GRACE, "{:?}", begun.elapsed());
    }

    #[tokio::test]
    async fn a_shell_that_ignores_the_hang_up_is_killed_after_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        let shell = stubborn_shell(dir.path());
        let (host_link, _client) = link_pair().await;
        let mut host = HostSession::spawn(
            &shell,
            &oxutrm_term::Start::default(),
            size(),
            200,
            host_link,
        )
        .unwrap();
        // The script has set its trap once it has made `ready`.
        let ready = dir.path().join("ready");
        let waiting = Instant::now();
        while !ready.exists() {
            assert!(waiting.elapsed() < Duration::from_secs(10), "no trap set");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let grace = Duration::from_millis(400);
        let begun = Instant::now();
        let code = host.hang_up_shell(grace).await;
        assert_eq!(code, 128 + 9, "ended by the SIGKILL");
        assert!(begun.elapsed() >= grace, "{:?}", begun.elapsed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_lobby_whose_link_closes_ends() {
        let (host_link, client) = link_pair().await;
        let mut lobby = HostSession::lobby(
            "/bin/sh",
            &oxutrm_term::Start::default(),
            size(),
            200,
            host_link,
        )
        .unwrap();
        assert!(lobby.is_lobby());
        let task = tokio::spawn(async move { lobby.run_with_attaches(&mut closed()).await });
        client
            .sink
            .connection()
            .close(quinn::VarInt::from_u32(0), b"the client quit");
        let code = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the lobby outlived its link")
            .unwrap()
            .unwrap();
        assert_eq!(code, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_lobby_whose_client_vanished_ends_after_the_silence() {
        let (host_link, _client) = link_pair().await;
        let silence = Duration::from_millis(300);
        let mut lobby = HostSession::lobby(
            "/bin/sh",
            &oxutrm_term::Start::default(),
            size(),
            200,
            host_link,
        )
        .unwrap()
        .with_detach_after(silence);
        let begun = Instant::now();
        let task = tokio::spawn(async move { lobby.run_with_attaches(&mut closed()).await });
        let code = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("a silent lobby never ended")
            .unwrap()
            .unwrap();
        assert_eq!(code, 0);
        assert!(begun.elapsed() >= silence, "{:?}", begun.elapsed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_session_whose_client_vanished_does_not_end() {
        let (host_link, _client) = link_pair().await;
        let mut host = HostSession::spawn(
            "/bin/sh",
            &oxutrm_term::Start::default(),
            size(),
            200,
            host_link,
        )
        .unwrap()
        .with_detach_after(Duration::from_millis(100));
        let task = tokio::spawn(async move { host.run_with_attaches(&mut closed()).await });
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            !task.is_finished(),
            "only a lobby ends on its client's silence"
        );
        task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_lobby_answers_its_client_with_a_blank_screen() {
        let (host_link, client_link) = link_pair().await;
        let mut lobby = HostSession::lobby(
            "/bin/sh",
            &oxutrm_term::Start::default(),
            size(),
            200,
            host_link,
        )
        .unwrap();
        let mut client = crate::session::ClientSession::new(
            size(),
            oxutrm_proto::TerminalCaps {
                truecolor: true,
                colors: 16_777_216,
                bracketed_paste: true,
                mouse_sgr: true,
                osc52: true,
                term_name: "xterm-256color".to_owned(),
            },
            client_link,
            None,
        )
        .unwrap();
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while client.applied_kinds() == (0, 0) {
            assert!(Instant::now() < deadline, "the lobby never answered");
            client.turn(b"ls\n", &mut out).unwrap();
            lobby.turn().unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            client.screen().cells.iter().all(|c| c.text.as_str() == " "),
            "a lobby's screen is blank"
        );
    }

    /// An attach channel whose sender is gone, as `run` uses.
    fn closed() -> tokio::sync::mpsc::Receiver<crate::attach_exchange::Attached> {
        let (_, rx) = tokio::sync::mpsc::channel(1);
        rx
    }

    /// A lobby's client resizing its terminal: the blank screen follows, so
    /// no cell of the old size is left for the client to paint.
    #[tokio::test]
    async fn a_lobbys_blank_screen_follows_its_clients_size() {
        let (host_link, _client) = link_pair().await;
        let mut lobby = HostSession::lobby(
            "/bin/sh",
            &oxutrm_term::Start::default(),
            size(),
            200,
            host_link,
        )
        .unwrap();
        let bigger = TermSize {
            cols: 160,
            rows: 50,
        };
        lobby.resize(bigger).unwrap();
        let screen = lobby.screen();
        assert_eq!((screen.cols, screen.rows), (160, 50));
        assert!(screen.cells.iter().all(|c| c.text.as_str() == " "));
    }
}

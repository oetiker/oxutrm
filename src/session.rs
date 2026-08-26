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

use oxutrm_client::{Renderer, status_line, terminal_size_of};
use oxutrm_proto::{Frame, PathDescription, ScreenState, TermSize, TerminalCaps};
use oxutrm_sync::{InputState, Receiver, Sender};
use oxutrm_term::HostTerm;

use crate::link::{Link, SendOutcome};

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
        })
    }

    /// One turn: apply whatever arrived, drain the PTY, offer a frame.
    pub fn turn(&mut self) -> Result<Turn> {
        let mut turn = Turn::default();

        // ---- inbound: the client's keystrokes ------------------------------
        // (the size the client wants rides on the same diff, and is applied
        // below once the frames have been taken in)
        while let Some(frame) = self.link.source.try_recv() {
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

        // ---- the terminal --------------------------------------------------
        if self.term.poll().context("draining the pty")? {
            // The sequence number is a placeholder; `update` mints the real
            // one, keeping numbering in exactly one place.
            let snapshot = self.term.snapshot(1);
            self.screen_tx.update(snapshot);
        }

        // ---- outbound: the screen ------------------------------------------
        turn.sent = self.offer_frame();
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

    /// Run until the child exits.
    pub async fn run(&mut self) -> Result<i32> {
        loop {
            let turn = self.turn()?;
            if let Some(code) = turn.exited {
                self.finish(code).await;
                return Ok(code);
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
    pub fn close(&self, code: i32) {
        let code = u32::try_from(code).unwrap_or(255);
        self.link
            .sink
            .connection()
            .close(quinn::VarInt::from_u32(code), b"the shell exited");
    }

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

/// Why the session ended, as an exit status.
///
/// The shell's exit code has no field in the protocol and needs none: the host
/// closes the QUIC connection with it as the application error code, so it
/// rides the mechanism that ends the session. Anything else closed the link —
/// a timeout, a reset, a host that was killed — and that is an error rather
/// than a status, because no shell said it.
fn exit_code(reason: &quinn::ConnectionError) -> Result<i32> {
    match reason {
        quinn::ConnectionError::ApplicationClosed(closed) => {
            Ok(i32::try_from(closed.error_code.into_inner()).unwrap_or(255))
        }
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
}

impl ClientSession {
    pub fn new(size: TermSize, caps: TerminalCaps, link: Link) -> Result<ClientSession> {
        let blank = ScreenState::blank(size.rows, size.cols)?;
        let empty = InputState {
            seq: 1,
            pending: Vec::new(),
            size,
        };
        Ok(ClientSession {
            screen_rx: Receiver::new(blank),
            input_tx: Sender::new(empty),
            renderer: Renderer::new(size, caps),
            link,
            size,
            last_send: None,
            announced: None,
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
                }
                Ok(false) => {}
                // See the host's copy of this arm: a silently swallowed
                // BaseMismatch is a frozen screen that looks like a slow one.
                Err(e) => {
                    turn.rejected += 1;
                    eprintln!("oxutrm: client dropped an unapplicable screen frame: {e}");
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

    /// The window changed size. The renderer forgets what is painted and the
    /// next input diff tells the host.
    pub fn resize(&mut self, size: TermSize) {
        if size == self.size {
            return;
        }
        self.renderer.resize(size);
        self.size = size;
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
        let tty = std::fs::File::options()
            .read(true)
            .open("/dev/tty")
            .context("opening the terminal to read the keyboard")?;
        self.run_on(tty, out).await
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
        // Whether the "cannot read the window size" note has been printed.
        // See the `Winch` arm.
        let mut warned_size = false;
        // Now, so the first lap sends immediately: an attach owes the host a
        // frame before anything has happened, because that frame is what
        // carries our ack of zero (R5).
        let mut deadline = tokio::time::Instant::now();

        loop {
            let wake = tokio::select! {
                r = keys_readable(&mut keys) => match r {
                    Ok(mut guard) => match guard.try_io(|k| k.get_mut().read(&mut buf)) {
                        Ok(Ok(n)) => Wake::Keys(n),
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
                    self.turn(&buf[..n], out)?;
                }
                Wake::Frame(frame) => {
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
                    match terminal_size_of(&window) {
                        Ok(size) => self.resize(size),
                        // Once. The condition lasts as long as the descriptor
                        // does, so repeating it would bury everything else —
                        // and this is stderr, which shares the user's screen.
                        Err(e) if !warned_size => {
                            warned_size = true;
                            eprintln!(
                                "oxutrm: cannot read the window size ({e:#}); keeping \
                                 {}x{}. The session is unaffected.",
                                self.size.cols, self.size.rows
                            );
                        }
                        Err(_) => {}
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
            deadline = tokio::time::Instant::now() + self.link.sink.pacing_interval();
        }
    }

    /// Move to a new local socket without dropping the connection.
    pub fn rebind(&mut self, socket: Arc<tokio::net::UdpSocket>) -> Result<()> {
        self.link.rebind(socket)?;
        // The path changed; what is on the terminal is still correct, so
        // nothing is repainted. Only the address moved.
        Ok(())
    }

    pub fn screen(&self) -> &ScreenState {
        self.screen_rx.state()
    }

    /// Applied screen frames that carried a diff, and those that carried a
    /// whole screen. See `Receiver::applied_kinds`.
    #[must_use]
    pub fn applied_kinds(&self) -> (u64, u64) {
        self.screen_rx.applied_kinds()
    }

    pub fn size(&self) -> TermSize {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let (host_ep, _stun) = quic_server(&host_sock, cert, key, ClientSpki::new(client_fp))
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
        let (host_ep, _s) = quic_server(&host_sock, cert, key, ClientSpki::new(client_fp))
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

    #[cfg(target_os = "linux")]
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

    #[test]
    fn only_an_application_close_is_an_exit_status() {
        // A host that was killed and a path that went away end the session
        // too. Reporting either as "the shell exited 0" would be a lie the
        // user acts on, so the status has to come from the shell or not at
        // all.
        let closed = quinn::ApplicationClose {
            error_code: quinn::VarInt::from_u32(42),
            reason: Default::default(),
        };
        assert_eq!(
            exit_code(&quinn::ConnectionError::ApplicationClosed(closed)).unwrap(),
            42
        );
        assert!(exit_code(&quinn::ConnectionError::TimedOut).is_err());
        assert!(exit_code(&quinn::ConnectionError::LocallyClosed).is_err());
    }

    /// This THREAD's CPU time, in milliseconds.
    ///
    /// Per-thread and not per-process: the test binary runs several tests at
    /// once, and a process-wide figure would be measuring them instead.
    /// `#[tokio::test]` with no flavor is a current-thread runtime, so the
    /// loop under test and every task it spawns stay on this one thread.
    #[cfg(target_os = "linux")]
    fn thread_cpu_millis() -> u64 {
        let stat =
            std::fs::read_to_string("/proc/thread-self/stat").expect("/proc/thread-self/stat");
        // The comm field can hold spaces and brackets, so the fields are taken
        // from after the LAST ')' rather than by splitting the whole line.
        let tail = &stat[stat.rfind(')').expect("a comm field") + 1..];
        let fields: Vec<&str> = tail.split_whitespace().collect();
        // utime and stime are fields 14 and 15 of the line, so 11 and 12 of
        // what is left. USER_HZ is 100, and has been for this interface's
        // whole life.
        let ticks: u64 =
            fields[11].parse::<u64>().expect("utime") + fields[12].parse::<u64>().expect("stime");
        ticks * 10
    }

    #[cfg(target_os = "linux")]
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
}

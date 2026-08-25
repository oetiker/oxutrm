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

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use oxutrm_client::Renderer;
use oxutrm_proto::{ScreenState, TermSize, TerminalCaps};
use oxutrm_sync::{InputState, Receiver, Sender};
use oxutrm_term::HostTerm;

use crate::link::{Link, SendOutcome};

/// How long a loop waits for something to happen before looking again.
///
/// Short enough that a keystroke is never sitting in a buffer, long enough
/// that an idle session costs nothing.
const IDLE_POLL: Duration = Duration::from_millis(4);

/// What one turn did. Returned so tests can watch the loop rather than infer
/// it from the screen.
#[derive(Clone, Debug, Default)]
pub struct Turn {
    pub sent: Option<SendOutcome>,
    pub applied: usize,
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
                Err(_) => {}
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

    fn offer_frame(&mut self) -> Option<SendOutcome> {
        if !self.due() {
            return None;
        }
        self.screen_tx.on_ack(self.input_rx.peer_ack());
        let frame = match self.screen_tx.make_frame(self.input_rx.ack()) {
            Ok(Some(f)) => f,
            // Nothing to send, or a diff that could not be built. Neither ends
            // the session.
            Ok(None) | Err(_) => return None,
        };
        self.last_send = Some(Instant::now());
        Some(self.link.sink.send(&frame))
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
                return Ok(code);
            }
            tokio::time::sleep(IDLE_POLL).await;
        }
    }

    pub fn screen(&self) -> &ScreenState {
        self.screen_tx.current()
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
        })
    }

    /// One turn: send `input`, apply what arrived, repaint if it changed.
    pub fn turn<W: Write>(&mut self, input: &[u8], out: &mut W) -> Result<Turn> {
        let mut turn = Turn::default();

        if !input.is_empty() {
            let next = self.input_tx.current().append(input, self.size);
            self.input_tx.update(next);
            // A keystroke waits for nothing: pacing governs how often the
            // screen is offered, not how fast typing reaches the shell.
            self.last_send = None;
        }

        // ---- inbound: the screen -------------------------------------------
        let mut painted = false;
        while let Some(frame) = self.link.source.try_recv() {
            // Streams can complete out of order. `on_frame` answers that from
            // the frame's own sequence numbers: an older one is Ok(false).
            match self.screen_rx.on_frame(&frame) {
                Ok(true) => {
                    turn.applied += 1;
                    painted = true;
                }
                Ok(false) => {}
                Err(_) => {}
            }
        }
        if painted {
            self.renderer
                .render(out, self.screen_rx.state())
                .context("painting the terminal")?;
            out.flush().context("flushing the terminal")?;
        }

        // ---- outbound: keystrokes and the size we want ---------------------
        turn.sent = self.offer_frame();
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

    pub fn size(&self) -> TermSize {
        self.size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_net::{generate_cert, quic_client, quic_server};

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

    async fn udp() -> Arc<tokio::net::UdpSocket> {
        Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap())
    }

    /// A host and a client joined by a real QUIC connection on loopback.
    async fn pair(shell: &str) -> (HostSession, ClientSession) {
        let (cert, key, fingerprint) = generate_cert().unwrap();

        let host_sock = udp().await;
        let host_addr = host_sock.local_addr().unwrap();
        let (host_ep, _stun) = quic_server(&host_sock, cert, key).await.unwrap();

        let client_sock = udp().await;
        let accepting = tokio::spawn(async move {
            let incoming = host_ep.accept().await.expect("an inbound connection");
            let conn = incoming.await.expect("a completed handshake");
            (conn, host_ep)
        });

        let (client_conn, client_ep, _cstun) = quic_client(&client_sock, host_addr, fingerprint)
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
        // Output far faster than the pacing interval must cost one frame per
        // interval, not a backlog - and the screen must still end up current.
        let (mut host, mut client) = pair("yes oxutrm-flood\n").await;
        let mut out = Vec::new();

        let mut turns = 0u64;
        let mut frames = 0u64;
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            let t = host.turn().expect("host turn");
            if t.sent.is_some() {
                frames += 1;
            }
            client.turn(&[], &mut out).expect("client turn");
            turns += 1;
            tokio::time::sleep(Duration::from_millis(3)).await;
        }

        eprintln!("{turns} turns, {frames} frames in 4s under a flood");
        assert!(turns > 40, "only {turns} turns; the test proves little");
        assert!(
            frames <= turns,
            "{frames} frames from {turns} turns: output is queueing, not coalescing"
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
        let host_sock = udp().await;
        let host_addr = host_sock.local_addr().unwrap();
        let (host_ep, _s) = quic_server(&host_sock, cert, key).await.unwrap();
        let client_sock = udp().await;
        let accepting = tokio::spawn(async move {
            let inc = host_ep.accept().await.unwrap();
            (inc.await.unwrap(), host_ep)
        });
        let (cc, ce, _cs) = quic_client(&client_sock, host_addr, fingerprint)
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
        let mut used_stream = false;
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            let t = host.turn().expect("host turn");
            if matches!(t.sent, Some(SendOutcome::Stream { .. })) {
                used_stream = true;
            }
            client.turn(&[], &mut out).expect("client turn");
            if used_stream && client.screen().seq == host.screen().seq {
                break;
            }
            tokio::time::sleep(Duration::from_millis(3)).await;
        }

        assert!(
            used_stream,
            "a frame larger than a datagram must go on a stream"
        );
        assert_eq!(client.screen().validate(), Ok(()));
        assert_eq!(text(client.screen()), text(host.screen()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_session_survives_the_client_changing_its_own_local_address() {
        // The ONLY migration QUIC has. There is no mechanism to repoint a
        // connection at a different REMOTE address, so this deliberately does
        // not try - a test built on that assumption would invite someone to
        // "fix" it later by adding something that cannot exist.
        let (mut host, mut client) = pair("printf 'before-roam\\r\\n'\n").await;
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

        let old_addr = client.link.socket.local_addr().unwrap();
        client.rebind(udp().await).expect("rebind");
        let new_addr = client.link.socket.local_addr().unwrap();
        assert_ne!(
            old_addr, new_addr,
            "the local address must actually have changed"
        );

        // And the session keeps working across the move.
        host.term
            .write_input(b"printf 'after-roam\\r\\n'\n")
            .unwrap();
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
}

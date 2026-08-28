//! Getting a [`Frame`] across a QUIC connection, and back.
//!
//! # Channel selection replaces fragmentation
//!
//! A frame that fits in a datagram goes in a datagram: unreliable, unordered,
//! and free of head-of-line blocking, which is what screen state wants — a
//! lost one costs nothing because the next diff is computed against the same
//! acknowledged base and contains whatever was lost.
//!
//! A peer with datagrams turned off has no channel at all, and that case is
//! refused rather than quietly redirected onto streams. `max_datagram_size()`
//! returning `None` is not a missing number to guess at and not a cue to send
//! everything reliably: it means the peer never advertised
//! `max_datagram_frame_size`. Streaming every frame instead would turn the
//! recovery channel into the whole transport, at one frame per pacing
//! interval, and present a configuration bug as a slow terminal. This is the
//! one rule that used to live only in an abstraction nothing called; it lives
//! here now, on the path a frame actually takes.
//!
//! A frame that does **not** fit goes on a fresh unidirectional stream,
//! reliably. It is not split across several datagrams, and that is a decision
//! rather than an omission. An unreliable state split into F pieces arrives
//! only if all F arrive, so delivery is `(1-p)^F`. A 200x60 truecolor full
//! state is about 125 pieces: 28% at 1% loss, 0.16% at 5%. A full state is
//! exactly what the ring-miss recovery path must send after a burst of loss,
//! so fragmentation would make the mechanism most needed after loss the one
//! least able to survive it.
//!
//! # Never queue
//!
//! At most one stream is in flight. If a newer state becomes current while one
//! is still being written, the old stream is **reset** and a new one opened for
//! the current state. Queueing would deliver a screen the user has already
//! moved past, and then another, and then another.
//!
//! "In flight" has to mean *still being written*, not *was started once*. A
//! writer that has finished, failed, or been reset stops counting, so a state
//! that never advances can be offered again after a lost attempt — otherwise
//! every state gets exactly one try and retry does not exist.
//!
//! Resetting has to be explicit: dropping a `quinn::SendStream` calls
//! `finish()`, which would deliver a **truncated** frame rather than
//! cancelling it. The receiver would reject that — rejection is not
//! disconnection — but it would have burned the bandwidth for nothing.
//!
//! # A send failure is not the end of the session
//!
//! Nothing here propagates a send error outward. A dropped frame costs one
//! pacing interval: the next tick re-diffs against the same acknowledged base
//! and carries everything the lost one would have. Ending the session because
//! one datagram would not fit in a socket buffer is the same mistake as
//! disconnecting because one diff failed to apply.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use oxutrm_proto::Frame;
use tokio::sync::oneshot;

/// Application error code for a superseded stream.
///
/// The peer sees this on `RESET_STREAM` and can tell "a newer state made this
/// one irrelevant" from a real failure.
const SUPERSEDED: u32 = 1;

/// The largest frame we will put on a stream. Beyond this something has gone
/// wrong upstream — a 200x60 truecolor full state is well under a megabyte.
const MAX_FRAME: usize = 8 * 1024 * 1024;

/// How long [`FrameSink::send_final`] waits for the peer to acknowledge the
/// last frame of a session before giving up on it.
///
/// Generous, because this is the screen the user is left looking at, and paid
/// only once, at the very end. Bounded, because a host that hangs for ever
/// waiting on a client that is already gone is worse than a lost last screen.
const FINAL_ACK_WAIT: Duration = Duration::from_secs(2);

/// Why a frame did not go out. None of these end a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// It went in a datagram.
    Datagram(usize),
    /// It went on a fresh unidirectional stream, superseding any in flight.
    Stream { bytes: usize, superseded: bool },
    /// The peer never advertised `max_datagram_frame_size`, so nothing can be
    /// sent on this connection at all. See [`FrameSink::send`].
    DatagramsDisabled,
    /// It did not go. The next pacing tick will carry the same information.
    Dropped(String),
}

/// The sending half.
pub struct FrameSink {
    conn: quinn::Connection,
    /// The stream currently being written, if any. Dropping the sender tells
    /// the writer task to reset rather than finish.
    in_flight: Option<InFlight>,
    /// Whether the "this peer has datagrams off" warning has been printed.
    /// Once per sink, not once per frame: the condition is permanent for the
    /// life of a connection, so repeating it would bury everything else.
    warned_no_datagrams: bool,
}

struct InFlight {
    /// Which state it is carrying, so a newer one can supersede it.
    my_state: u64,
    cancel: oneshot::Sender<()>,
    /// Set by this entry's own writer task when it stops touching its stream.
    ///
    /// A flag rather than a channel or a shared `Option`, for two reasons.
    ///
    /// [`FrameSink::send`] is sync, non-blocking and infallible outward, so it
    /// cannot await a completion signal and must not take a lock that a writer
    /// task holds across an await. An [`AtomicBool`] is readable from `send`
    /// with one load and no blocking.
    ///
    /// And it makes the obvious race unrepresentable. The clear happens in the
    /// task while the check and the take happen in `send`, so a task that
    /// reached into `in_flight` could wipe an entry that a newer frame had
    /// already installed. This flag belongs to exactly one writer, is written
    /// by nobody else, and is thrown away with the entry that owns it — a
    /// superseded writer sets a flag that `send` no longer holds.
    done: Arc<AtomicBool>,
}

impl InFlight {
    fn finished(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

/// Marks an [`InFlight`] finished however its writer task leaves: stream
/// opened or not, written, failed, cancelled, or unwound by a panic.
///
/// A plain store at the end of the task would miss the early return when
/// `open_uni` fails — which is precisely the case that must stay retryable.
struct DoneOnDrop(Arc<AtomicBool>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl FrameSink {
    pub fn new(conn: quinn::Connection) -> FrameSink {
        FrameSink {
            conn,
            in_flight: None,
            warned_no_datagrams: false,
        }
    }

    /// Send one frame, choosing the channel by size.
    ///
    /// Never fails outward: every error becomes [`SendOutcome::Dropped`] and
    /// the caller carries on.
    pub fn send(&mut self, frame: &Frame) -> SendOutcome {
        let bytes = match frame.encode() {
            Ok(b) => b,
            Err(e) => return SendOutcome::Dropped(format!("encoding a frame: {e}")),
        };
        if bytes.len() > MAX_FRAME {
            return SendOutcome::Dropped(format!("frame of {} bytes is absurd", bytes.len()));
        }

        // `max_datagram_size` shrinks when the path MTU does, so it is asked
        // for on every frame rather than cached: a limit decided once when the
        // connection was built would survive a migration that invalidated it,
        // and every send after that would fail for no visible reason.
        //
        // `None` is a different thing entirely, and it is NOT a size to guess
        // at nor a cue to put everything on a stream. It means the peer never
        // advertised `max_datagram_frame_size`, so no frame of any size can
        // travel unreliably on this connection. See the outcome's own note for
        // why that refuses instead of falling through.
        let Some(limit) = self.conn.max_datagram_size() else {
            return self.no_datagrams();
        };

        if bytes.len() <= limit {
            let n = bytes.len();
            return match self.conn.send_datagram(bytes.into()) {
                Ok(()) => SendOutcome::Datagram(n),
                // Full send buffer, or a peer that stopped accepting
                // datagrams. Neither is fatal.
                Err(e) => SendOutcome::Dropped(format!("datagram: {e}")),
            };
        }

        self.send_on_stream(frame.my_state, bytes)
    }

    /// The peer turned datagrams off, so this connection cannot carry a
    /// session at all.
    ///
    /// Falling through to [`FrameSink::send_on_stream`] would "work" and is
    /// what makes this worth a comment: every frame, keystrokes included,
    /// would take a fresh unidirectional stream, one at a time, at one frame
    /// per pacing interval, with everything offered in between dropped. That
    /// is a terminal that feels mysteriously broken rather than a
    /// configuration bug anybody can find — and it silently converts the
    /// stream path, which exists as a recovery channel for oversized states,
    /// into the whole transport.
    ///
    /// Both ends of oxutrm set both datagram buffer sizes, so reaching here
    /// means either that config grew a hole (`oxutrm_net::quic` documents how
    /// easily: omit one of the two lines and datagrams vanish silently) or the
    /// peer is not oxutrm. Say so once, out loud, and keep returning a
    /// non-fatal outcome — a send failure still never ends a session.
    fn no_datagrams(&mut self) -> SendOutcome {
        if !self.warned_no_datagrams {
            self.warned_no_datagrams = true;
            eprintln!(
                "oxutrm: this peer advertised no QUIC datagram support, so no screen \
                 state can be sent. Nothing will be displayed until the connection is \
                 replaced."
            );
        }
        SendOutcome::DatagramsDisabled
    }

    fn send_on_stream(&mut self, my_state: u64, bytes: Vec<u8>) -> SendOutcome {
        // Reap a writer that has already stopped, BEFORE deciding anything.
        // Without this an entry outlives its task for ever, so a state that
        // does not advance gets exactly one stream attempt in its whole life —
        // a failed `open_uni` or a reset would never be retried — and a
        // long-finished stream gets reported as superseded by the next one.
        if self.in_flight.as_ref().is_some_and(InFlight::finished) {
            self.in_flight = None;
        }

        // Supersede whatever is still genuinely in flight: dropping the cancel
        // sender makes the writer task reset its stream rather than finish it.
        let superseded = match self.in_flight.take() {
            Some(old) if old.my_state < my_state => {
                drop(old.cancel);
                true
            }
            // A stream carrying this state or a newer one is already going.
            // Starting a second would be the queue this design forbids.
            Some(old) => {
                self.in_flight = Some(old);
                return SendOutcome::Dropped("a newer state is already in flight".to_owned());
            }
            None => false,
        };

        let (cancel_tx, cancel_rx) = oneshot::channel();
        let conn = self.conn.clone();
        let n = bytes.len();
        let done = Arc::new(AtomicBool::new(false));
        let mark_done = DoneOnDrop(Arc::clone(&done));

        tokio::spawn(async move {
            // Held for the whole task. However this ends — an `open_uni` that
            // never succeeded, a completed write, a failed one, a reset, or a
            // runtime that drops the future unpolled — the entry stops
            // counting as in flight.
            let _mark_done = mark_done;
            let mut stream = match conn.open_uni().await {
                Ok(s) => s,
                // Out of stream credit or a closing connection. The next tick
                // tries again — which it now can.
                Err(_) => return,
            };
            tokio::select! {
                // The write finished, or failed. Either way we are done.
                result = stream.write_all(&bytes) => {
                    if result.is_ok() {
                        let _ = stream.finish();
                    }
                }
                // The sender was dropped: a newer state superseded this one.
                // RESET explicitly - dropping the stream would `finish` it and
                // deliver a truncated frame.
                _ = cancel_rx => {
                    let _ = stream.reset(SUPERSEDED.into());
                }
            }
        });

        self.in_flight = Some(InFlight {
            my_state,
            cancel: cancel_tx,
            done,
        });
        SendOutcome::Stream {
            bytes: n,
            superseded,
        }
    }

    /// Send one frame and do not come back until the peer has it.
    ///
    /// The whole file above says a send failure costs one pacing interval,
    /// because the next interval re-diffs from the same base and carries
    /// whatever was lost. That argument has exactly one hole in it, and this
    /// is the method for the moment it opens: **there is no next interval.**
    /// When the shell has exited, the frame being offered is the last one the
    /// session will ever produce, and the close that follows it discards
    /// anything still in flight — a datagram outright, and a stream whose
    /// writer task has not yet reached `open_uni`.
    ///
    /// So this ignores size and takes the stream path unconditionally, writes
    /// inline rather than in a spawned task the close could outrun, calls
    /// `finish()`, and then waits on [`quinn::SendStream::stopped`], which
    /// resolves once the peer has **acknowledged every byte**. Only after that
    /// is closing the connection safe.
    ///
    /// It is still infallible outward: a peer that has already gone is not an
    /// error to report to a shell that has already exited.
    pub async fn send_final(&mut self, frame: &Frame) -> SendOutcome {
        let bytes = match frame.encode() {
            Ok(b) => b,
            Err(e) => return SendOutcome::Dropped(format!("encoding a frame: {e}")),
        };
        if bytes.len() > MAX_FRAME {
            return SendOutcome::Dropped(format!("frame of {} bytes is absurd", bytes.len()));
        }
        let n = bytes.len();

        // Anything still being written is carrying an older state than this
        // one by construction — this frame was minted from the newest state
        // there is. Dropping the entry resets that stream exactly as an
        // ordinary supersede would, rather than leaving it to race the close.
        let superseded = self.in_flight.take().is_some_and(|old| !old.finished());

        let mut stream = match self.conn.open_uni().await {
            Ok(s) => s,
            Err(e) => return SendOutcome::Dropped(format!("opening the final stream: {e}")),
        };
        if let Err(e) = stream.write_all(&bytes).await {
            return SendOutcome::Dropped(format!("writing the final frame: {e}"));
        }
        // `finish` only promises that no more data is coming; it does not wait
        // for what was already written. `stopped` is the part that makes the
        // close safe.
        if let Err(e) = stream.finish() {
            return SendOutcome::Dropped(format!("finishing the final stream: {e}"));
        }
        match tokio::time::timeout(FINAL_ACK_WAIT, stream.stopped()).await {
            Ok(_) => SendOutcome::Stream {
                bytes: n,
                superseded,
            },
            // A peer that has stopped acknowledging is a peer that is already
            // gone. Bounded rather than open-ended because the alternative is
            // a host process that never exits — and the user is owed their
            // shell's status even when the last screen could not be delivered.
            Err(_) => {
                SendOutcome::Dropped("the peer never acknowledged the final frame".to_owned())
            }
        }
    }

    /// How long to wait before offering the next frame.
    ///
    /// Taken from `quinn`'s own round-trip estimate rather than measured
    /// again: `clamp(rtt / 2, 8ms, 100ms)`. Half the RTT is roughly one
    /// one-way delay, so a frame lands about as often as the peer could
    /// acknowledge one. The floor stops a loopback link spinning; the ceiling
    /// keeps a satellite link from feeling dead.
    pub fn pacing_interval(&self) -> Duration {
        (self.conn.rtt() / 2).clamp(Duration::from_millis(8), Duration::from_millis(100))
    }

    pub fn connection(&self) -> &quinn::Connection {
        &self.conn
    }
}

/// The receiving half: datagrams and unidirectional streams, whichever arrives.
///
/// Streams may complete out of order, so the caller must apply a frame only if
/// its `my_state` is newer than what it holds. `Receiver::on_frame` already
/// does exactly that — a stale frame is `Ok(false)`, not an error — so nothing
/// here needs to reorder anything.
pub struct FrameSource {
    rx: tokio::sync::mpsc::Receiver<Frame>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl FrameSource {
    pub fn new(conn: quinn::Connection) -> FrameSource {
        // Bounded: if the consumer stalls, dropping frames is right. Every one
        // of them is superseded by the next diff anyway.
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        let datagrams = {
            let conn = conn.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                while let Ok(bytes) = conn.read_datagram().await {
                    match Frame::decode(&bytes) {
                        // A malformed datagram is one bad datagram, not a bad
                        // session.
                        Err(_) => continue,
                        Ok(f) => {
                            if tx.send(f).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            })
        };

        let streams = {
            let conn = conn.clone();
            tokio::spawn(async move {
                while let Ok(mut recv) = conn.accept_uni().await {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        // A stream that was reset resolves as an error here,
                        // which is exactly what a superseded frame should do:
                        // vanish without a trace.
                        let Ok(bytes) = recv.read_to_end(MAX_FRAME).await else {
                            return;
                        };
                        if let Ok(f) = Frame::decode(&bytes) {
                            let _ = tx.send(f).await;
                        }
                    });
                }
            })
        };

        FrameSource {
            rx,
            _tasks: vec![datagrams, streams],
        }
    }

    /// The next frame, or `None` once the connection is gone.
    pub async fn recv(&mut self) -> Option<Frame> {
        self.rx.recv().await
    }

    /// A frame if one is waiting, without blocking.
    pub fn try_recv(&mut self) -> Option<Frame> {
        self.rx.try_recv().ok()
    }
}

/// Everything a session needs from the transport, so the loops never touch
/// `quinn` directly.
pub struct Link {
    pub sink: FrameSink,
    pub source: FrameSource,
    /// Kept so the session can rebind it while roaming.
    /// Read by [`Link::rebind`], which `run_connect` will call.
    #[allow(dead_code)]
    pub endpoint: quinn::Endpoint,
    /// The socket the ladder punched. Held because ICE keepalives send on it
    /// directly, alongside QUIC.
    pub socket: Arc<tokio::net::UdpSocket>,
}

impl Link {
    pub fn new(
        conn: quinn::Connection,
        endpoint: quinn::Endpoint,
        socket: Arc<tokio::net::UdpSocket>,
    ) -> Link {
        Link {
            sink: FrameSink::new(conn.clone()),
            source: FrameSource::new(conn),
            endpoint,
            socket,
        }
    }

    /// Move to a new local socket without dropping the connection.
    ///
    /// This is the **only** kind of migration QUIC has: a client may change
    /// its own local address. There is no mechanism, in the protocol or in
    /// `quinn`, to repoint an established connection at a different *remote*
    /// address — so a better path discovered later is lost for this attach and
    /// picked up on the next one.
    /// **No caller: roaming is not wired.** Only the client may change its
    /// local address, and nothing in `run_connect` yet notices that it has --
    /// there is no watcher on the host's routes and no trigger to rebind from.
    /// The mechanism is here and tested; what is missing is whatever decides
    /// when to use it.
    #[allow(dead_code)]
    pub fn rebind(&mut self, socket: Arc<tokio::net::UdpSocket>) -> anyhow::Result<()> {
        let (demux, _stun) = oxutrm_net::StunDemuxSocket::new(&socket)?;
        self.endpoint.rebind_abstract(demux)?;
        self.socket = socket;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Instant;

    use oxutrm_net::{
        ALPN, CERT_NAME, PinnedClientSpki, PinnedSpki, generate_cert, install_crypto_provider,
        provider,
    };
    use oxutrm_proto::{ClientSpki, HostSpki};
    use quinn::rustls;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    const BUFFER: usize = 1024 * 1024;

    /// A transport config whose datagram support can be turned off.
    ///
    /// It is the RECEIVE buffer that decides what the peer sees: leaving it
    /// unset is exactly how a peer "disables datagrams", and it is what makes
    /// the far end's `max_datagram_size()` return `None`. That asymmetry is
    /// documented in `oxutrm_net::quic` and is the failure this file has to
    /// cope with.
    fn transport(datagrams: bool) -> Arc<quinn::TransportConfig> {
        let mut t = quinn::TransportConfig::default();
        t.datagram_send_buffer_size(BUFFER);
        t.datagram_receive_buffer_size(datagrams.then_some(BUFFER));
        t.max_concurrent_uni_streams(quinn::VarInt::from_u32(1024));
        Arc::new(t)
    }

    fn server_config(
        cert: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
        expect_client: ClientSpki,
        datagrams: bool,
    ) -> quinn::ServerConfig {
        install_crypto_provider();
        let mut tls = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            // Mirrors `oxutrm_net::quic`, deliberately. These fixtures build
            // their own configs so they can turn datagrams off, and a fixture
            // that quietly kept `with_no_client_auth()` would be a second,
            // unauthenticated way to build an oxutrm server living in the same
            // binary as the tests that prove there is none.
            .with_client_cert_verifier(Arc::new(PinnedClientSpki::new(expect_client)))
            .with_single_cert(vec![cert], key)
            .unwrap();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
        let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        cfg.transport_config(transport(datagrams));
        cfg
    }

    fn client_config(
        fingerprint: [u8; 32],
        cert: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
        datagrams: bool,
    ) -> quinn::ClientConfig {
        install_crypto_provider();
        let mut tls = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedSpki::new(HostSpki::new(fingerprint))))
            .with_client_auth_cert(vec![cert], key)
            .unwrap();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
        let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
        cfg.transport_config(transport(datagrams));
        cfg
    }

    /// A QUIC pair on loopback, with no ICE and no STUN demux in the way.
    ///
    /// `peer_datagrams` is what the SERVER advertises, so it decides what the
    /// returned client-side [`FrameSink`] sees from `max_datagram_size()`.
    /// The endpoints come back because dropping them closes the connection.
    async fn pair(peer_datagrams: bool) -> (FrameSink, FrameSource, Vec<quinn::Endpoint>) {
        let (cert, key, fingerprint) = generate_cert().unwrap();
        let (client_cert, client_key, client_fp) = generate_cert().unwrap();
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let server_ep = quinn::Endpoint::server(
            server_config(cert, key, ClientSpki::new(client_fp), peer_datagrams),
            addr,
        )
        .unwrap();
        let server_addr = server_ep.local_addr().unwrap();
        let accepting = {
            let ep = server_ep.clone();
            tokio::spawn(async move { ep.accept().await.unwrap().await.unwrap() })
        };

        let mut client_ep = quinn::Endpoint::client(addr).unwrap();
        client_ep.set_default_client_config(client_config(
            fingerprint,
            client_cert,
            client_key,
            true,
        ));
        let client_conn = client_ep
            .connect(server_addr, CERT_NAME)
            .unwrap()
            .await
            .unwrap();
        let server_conn = accepting.await.unwrap();

        (
            FrameSink::new(client_conn),
            FrameSource::new(server_conn),
            vec![client_ep, server_ep],
        )
    }

    /// Far larger than any datagram, so channel selection must reach for a
    /// stream.
    fn big(my_state: u64) -> Frame {
        Frame {
            my_state,
            from_state: 0,
            ack_state: 1,
            flags: 0,
            payload: vec![0xab; 64 * 1024],
        }
    }

    fn small(my_state: u64) -> Frame {
        Frame {
            my_state,
            from_state: my_state.saturating_sub(1),
            ack_state: 1,
            flags: 0,
            payload: vec![0xcd; 32],
        }
    }

    /// Wait until a frame comes out the far end, which proves the writer task
    /// ran to completion: the peer cannot finish `read_to_end` before
    /// `finish()` was called on the sending side.
    async fn arrived(source: &mut FrameSource, my_state: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(f) = source.try_recv()
                && f.my_state == my_state
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("frame for state {my_state} never arrived");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn size_alone_picks_the_channel_when_datagrams_are_available() {
        let (mut sink, mut source, _eps) = pair(true).await;
        let limit = sink
            .connection()
            .max_datagram_size()
            .expect("the peer enabled datagrams");

        match sink.send(&small(1)) {
            SendOutcome::Datagram(n) => assert!(n <= limit),
            other => panic!("a small frame did not go in a datagram: {other:?}"),
        }
        match sink.send(&big(2)) {
            SendOutcome::Stream { bytes, .. } => {
                assert!(
                    bytes > limit,
                    "a frame of {bytes} bytes fits the {limit}-byte datagram limit, so the \
                     stream path was taken for some reason other than size"
                );
            }
            other => panic!("an oversized frame did not go on a stream: {other:?}"),
        }
        arrived(&mut source, 2).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_finished_stream_stops_counting_as_in_flight() {
        // The retry that defect 1 killed. A state that got exactly one stream
        // attempt must be offered again once that attempt is over: nothing is
        // in flight any more, whatever it was carrying.
        let (mut sink, mut source, _eps) = pair(true).await;

        assert!(matches!(sink.send(&big(7)), SendOutcome::Stream { .. }));
        arrived(&mut source, 7).await;

        // The SAME state again. Nothing newer exists, so the only reason to
        // refuse would be an `in_flight` entry that never got cleared.
        let again = sink.send(&big(7));
        assert!(
            matches!(again, SendOutcome::Stream { .. }),
            "a state whose only stream attempt has already finished can never be \
             retried: {again:?}"
        );
        arrived(&mut source, 7).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn superseding_is_reported_only_when_a_live_stream_was_actually_reset() {
        let (mut sink, mut source, _eps) = pair(true).await;

        assert_eq!(
            sink.send(&big(1)),
            SendOutcome::Stream {
                bytes: big(1).encode().unwrap().len(),
                superseded: false
            }
        );
        arrived(&mut source, 1).await;

        // State 1's stream is over. State 2 supersedes nothing.
        match sink.send(&big(2)) {
            SendOutcome::Stream { superseded, .. } => assert!(
                !superseded,
                "a long-finished stream was reported as superseded, so the caller \
                 cannot tell a real reset from a stale bookkeeping entry"
            ),
            other => panic!("expected a stream: {other:?}"),
        }
        arrived(&mut source, 2).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_peer_with_datagrams_disabled_is_refused_rather_than_silently_streamed() {
        // `max_datagram_size()` is `None` here. That is not a missing number to
        // guess at and not a cue to put everything on streams: it means no
        // frame of any size can travel unreliably. Falling through to the
        // stream path would silently move an entire session onto the recovery
        // channel, at one frame per pacing interval, and look like a
        // mysteriously slow terminal rather than the configuration bug it is.
        let (mut sink, _source, _eps) = pair(false).await;
        assert!(
            sink.connection().max_datagram_size().is_none(),
            "this test needs a peer that disabled datagrams"
        );

        // A frame that would comfortably fit a datagram.
        let outcome = sink.send(&small(1));
        assert_eq!(
            outcome,
            SendOutcome::DatagramsDisabled,
            "a small frame on a datagram-less path came back as {outcome:?}"
        );
        // And an oversized one gets the same answer: the path is unusable, not
        // selectively usable.
        let outcome = sink.send(&big(2));
        assert_eq!(
            outcome,
            SendOutcome::DatagramsDisabled,
            "an oversized frame on a datagram-less path came back as {outcome:?}"
        );
    }
}

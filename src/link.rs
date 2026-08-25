//! Getting a [`Frame`] across a QUIC connection, and back.
//!
//! # Channel selection replaces fragmentation
//!
//! A frame that fits in a datagram goes in a datagram: unreliable, unordered,
//! and free of head-of-line blocking, which is what screen state wants — a
//! lost one costs nothing because the next diff is computed against the same
//! acknowledged base and contains whatever was lost.
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

/// Why a frame did not go out. None of these end a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// It went in a datagram.
    Datagram(usize),
    /// It went on a fresh unidirectional stream, superseding any in flight.
    Stream { bytes: usize, superseded: bool },
    /// It did not go. The next pacing tick will carry the same information.
    Dropped(String),
}

/// The sending half.
pub struct FrameSink {
    conn: quinn::Connection,
    /// The stream currently being written, if any. Dropping the sender tells
    /// the writer task to reset rather than finish.
    in_flight: Option<InFlight>,
}

struct InFlight {
    /// Which state it is carrying, so a newer one can supersede it.
    my_state: u64,
    cancel: oneshot::Sender<()>,
}

impl FrameSink {
    pub fn new(conn: quinn::Connection) -> FrameSink {
        FrameSink {
            conn,
            in_flight: None,
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

        // `max_datagram_size` is None when the peer disabled datagrams, and
        // shrinks when the path MTU does. Asking every time rather than
        // caching is what keeps this correct across a migration.
        if let Some(limit) = self.conn.max_datagram_size() {
            if bytes.len() <= limit {
                let n = bytes.len();
                return match self.conn.send_datagram(bytes.into()) {
                    Ok(()) => SendOutcome::Datagram(n),
                    // Full send buffer, or a peer that stopped accepting
                    // datagrams. Neither is fatal.
                    Err(e) => SendOutcome::Dropped(format!("datagram: {e}")),
                };
            }
        }

        self.send_on_stream(frame.my_state, bytes)
    }

    fn send_on_stream(&mut self, my_state: u64, bytes: Vec<u8>) -> SendOutcome {
        // Supersede whatever is in flight: dropping the cancel sender makes
        // the writer task reset its stream rather than finish it.
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

        tokio::spawn(async move {
            let mut stream = match conn.open_uni().await {
                Ok(s) => s,
                // Out of stream credit or a closing connection. The next tick
                // tries again.
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
        });
        SendOutcome::Stream {
            bytes: n,
            superseded,
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
    pub fn rebind(&mut self, socket: Arc<tokio::net::UdpSocket>) -> anyhow::Result<()> {
        let (demux, _stun) = oxutrm_net::StunDemuxSocket::new(&socket)?;
        self.endpoint.rebind_abstract(demux)?;
        self.socket = socket;
        Ok(())
    }
}

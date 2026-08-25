//! One UDP socket carrying both STUN and QUIC.
//!
//! # The problem
//!
//! `quinn::Endpoint::new` takes the socket and runs **its own** receive loop on
//! it. So does every STUN client, and so does any hand-rolled `recv_from` loop.
//! Two receivers on one UDP socket do not cooperate: each `recvmsg` removes a
//! datagram from the kernel queue, so whichever task wins the race gets it and
//! the other never sees it. STUN answers vanish into quinn, which discards
//! them; QUIC packets vanish into the STUN loop, which discards them. Nothing
//! errors. The connection intermittently fails to come up, on a timer nobody
//! can reproduce.
//!
//! # The asymmetry the fix rests on
//!
//! **Sending** on a UDP socket from several places at once is fine.
//! **Receiving** must have exactly one owner. So there is one receiver —
//! quinn's — and this wrapper sits in front of it: [`StunDemuxSocket::poll_recv`]
//! asks the real socket for a batch, moves every STUN datagram into an `mpsc`
//! channel, and hands quinn only what is left. The caller keeps its own
//! `Arc<tokio::net::UdpSocket>` and uses it for `send_to` alone, which is how
//! ICE keepalives keep working after QUIC has started.
//!
//! Construct the endpoint with `Endpoint::new_with_abstract_socket`, **never**
//! `Endpoint::new`.
//!
//! # Two implementation notes
//!
//! **Delegate, do not reimplement.** `quinn::Runtime::wrap_udp_socket` returns
//! quinn's own `AsyncUdpSocket`, with all its GSO/GRO/ECN platform handling.
//! This wrapper duplicates the file descriptor, hands one copy to that, and
//! forwards every trait method. Two descriptors, one socket: both may send,
//! only quinn's copy ever receives.
//!
//! **Generic receive offload is turned off.** With GRO a single `RecvMeta` can
//! describe several coalesced datagrams sharing one buffer, and splitting a
//! mixed STUN/QUIC batch apart by `stride` is fiddly and easy to get subtly
//! wrong. Overriding [`StunDemuxSocket::max_receive_segments`] to 1 costs a few
//! syscalls under bulk load and makes the demultiplexer obviously correct
//! instead of quietly wrong.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::Context as _;
use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};

use crate::is_stun;

/// Datagrams peeled off the front of the QUIC stream.
pub type StunRx = tokio::sync::mpsc::Receiver<(Vec<u8>, SocketAddr)>;

/// How many STUN datagrams may queue before they are dropped.
///
/// ICE checks are idempotent and retried, so dropping one is always better
/// than applying back-pressure to QUIC.
const STUN_QUEUE: usize = 64;

pub struct StunDemuxSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    stun: tokio::sync::mpsc::Sender<(Vec<u8>, SocketAddr)>,
}

impl std::fmt::Debug for StunDemuxSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StunDemuxSocket")
            .field("local_addr", &self.inner.local_addr().ok())
            .finish()
    }
}

impl StunDemuxSocket {
    /// Wrap `inner` for `quinn`.
    ///
    /// The caller **keeps** `inner` and uses it for `send_to` only. The
    /// returned socket is the single owner of the receive side; hand it to
    /// `quinn::Endpoint::new_with_abstract_socket`.
    pub fn new(
        inner: &Arc<tokio::net::UdpSocket>,
    ) -> anyhow::Result<(Arc<StunDemuxSocket>, StunRx)> {
        use std::os::fd::AsFd as _;

        // Duplicate the descriptor so quinn's own AsyncUdpSocket - with all
        // its GSO, GRO and ECN handling - can own one copy while the caller
        // keeps the other for sending.
        let dup = inner
            .as_fd()
            .try_clone_to_owned()
            .context("duplicating the session socket for quinn")?;
        let std_socket = std::net::UdpSocket::from(dup);

        let wrapped = quinn::Runtime::wrap_udp_socket(&quinn::TokioRuntime, std_socket)
            .context("handing the session socket to quinn")?;

        let (tx, rx) = tokio::sync::mpsc::channel(STUN_QUEUE);
        Ok((
            Arc::new(StunDemuxSocket {
                inner: wrapped,
                stun: tx,
            }),
            rx,
        ))
    }
}

impl AsyncUdpSocket for StunDemuxSocket {
    fn create_io_poller(self: Arc<Self>) -> std::pin::Pin<Box<dyn UdpPoller>> {
        Arc::clone(&self.inner).create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            let n = match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Ready(Ok(n)) => n,
                other => return other,
            };

            // Stable compaction: keep the non-STUN datagrams in order and
            // divert the rest. `write <= read` always holds, so the copy is
            // never an overlapping move.
            let mut write = 0usize;
            for read in 0..n {
                let len = meta[read].len;
                if is_stun(&bufs[read][..len]) {
                    // Non-blocking on purpose: a full or closed channel drops
                    // the check rather than stalling QUIC behind it.
                    let _ = self
                        .stun
                        .try_send((bufs[read][..len].to_vec(), meta[read].addr));
                    continue;
                }
                if write != read {
                    let (dst, src) = bufs.split_at_mut(read);
                    dst[write][..len].copy_from_slice(&src[0][..len]);
                    meta[write] = meta[read];
                }
                write += 1;
            }

            // A batch that was entirely STUN is not "nothing arrived". Telling
            // quinn zero would read as a spurious wakeup, so go round again.
            if write > 0 {
                return Poll::Ready(Ok(write));
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    /// One datagram per `RecvMeta`. See the module docs.
    fn max_receive_segments(&self) -> usize {
        1
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    /// Pull one batch out of the wrapper, the way quinn's driver does.
    async fn recv_one(sock: &Arc<StunDemuxSocket>) -> (Vec<u8>, SocketAddr) {
        let mut storage = [0u8; 2048];
        let mut meta = [RecvMeta::default()];
        let n = std::future::poll_fn(|cx| {
            let mut bufs = [io::IoSliceMut::new(&mut storage)];
            sock.poll_recv(cx, &mut bufs, &mut meta)
        })
        .await
        .expect("poll_recv");
        assert_eq!(n, 1);
        (storage[..meta[0].len].to_vec(), meta[0].addr)
    }

    fn stun_datagram() -> Vec<u8> {
        // A syntactically real Binding Request: type, length, cookie, tid.
        let mut v = vec![0x00, 0x01, 0x00, 0x00];
        v.extend_from_slice(&crate::STUN_MAGIC_COOKIE);
        v.extend_from_slice(&[0xA1; 12]);
        v
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stun_goes_to_the_channel_and_everything_else_goes_to_quinn() {
        let inner = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let target = inner.local_addr().unwrap();
        let (demux, mut stun_rx) = StunDemuxSocket::new(&inner).unwrap();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let from = sender.local_addr().unwrap();
        sender.send_to(&stun_datagram(), target).await.unwrap();
        sender.send_to(&[0xC3; 64], target).await.unwrap();

        // quinn sees only the QUIC packet, and is not blocked by the STUN one.
        let (bytes, addr) = tokio::time::timeout(Duration::from_secs(5), recv_one(&demux))
            .await
            .expect("the QUIC packet must arrive");
        assert_eq!(bytes, vec![0xC3; 64]);
        assert_eq!(addr, from);

        let (stun_bytes, stun_from) = tokio::time::timeout(Duration::from_secs(5), stun_rx.recv())
            .await
            .expect("the STUN packet must arrive")
            .expect("channel open");
        assert_eq!(stun_bytes, stun_datagram());
        assert_eq!(stun_from, from);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_of_stun_datagrams_does_not_stall_the_quic_side() {
        let inner = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let target = inner.local_addr().unwrap();
        let (demux, mut stun_rx) = StunDemuxSocket::new(&inner).unwrap();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for _ in 0..16 {
            sender.send_to(&stun_datagram(), target).await.unwrap();
        }
        sender.send_to(&[0xC3; 64], target).await.unwrap();

        let (bytes, _) = tokio::time::timeout(Duration::from_secs(5), recv_one(&demux))
            .await
            .expect("poll_recv must keep looking until it has a non-STUN datagram");
        assert_eq!(bytes, vec![0xC3; 64]);

        let mut seen = 0;
        while stun_rx.try_recv().is_ok() {
            seen += 1;
        }
        assert_eq!(seen, 16, "every STUN datagram must reach the channel");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_caller_can_still_send_on_the_socket_it_kept() {
        // The asymmetry the whole design rests on: sending from two
        // descriptors is safe, receiving from two is not.
        let inner = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let (demux, _rx) = StunDemuxSocket::new(&inner).unwrap();

        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        inner.send_to(b"from the caller", peer_addr).await.unwrap();

        let mut buf = [0u8; 64];
        let (n, from) = tokio::time::timeout(Duration::from_secs(5), peer.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"from the caller");
        assert_eq!(from, inner.local_addr().unwrap());
        assert_eq!(demux.local_addr().unwrap(), inner.local_addr().unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn generic_receive_offload_is_off_so_every_meta_is_one_datagram() {
        let inner = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let (demux, _rx) = StunDemuxSocket::new(&inner).unwrap();
        assert_eq!(
            demux.max_receive_segments(),
            1,
            "a coalesced batch cannot be demultiplexed one datagram at a time"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_stun_channel_does_not_wedge_the_receive_path() {
        // Nobody drains STUN unless keepalives are running. It must be
        // discarded, never back-pressure QUIC.
        let inner = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let target = inner.local_addr().unwrap();
        let (demux, rx) = StunDemuxSocket::new(&inner).unwrap();
        drop(rx);

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for _ in 0..64 {
            sender.send_to(&stun_datagram(), target).await.unwrap();
        }
        sender.send_to(&[0xC3; 64], target).await.unwrap();

        let (bytes, _) = tokio::time::timeout(Duration::from_secs(5), recv_one(&demux))
            .await
            .expect("a closed STUN channel must not wedge the receive path");
        assert_eq!(bytes, vec![0xC3; 64]);
    }
}

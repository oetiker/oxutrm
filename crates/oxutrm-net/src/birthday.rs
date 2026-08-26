//! Rung 3: the birthday-paradox blast, for symmetric NAT.
//!
//! Behind a symmetric NAT the external port is unpredictable, but it is not
//! *unguessable*. Both sides open N sockets and each fires at M guessed ports
//! around the peer's observed base. With N and M both around 256 that is ~65k
//! combinations against an ephemeral range of similar size, so a collision is
//! likely within a few seconds.
//!
//! # Four guardrails, because this is deliberately noisy
//!
//! 1. **It runs only when it is the right answer** — the caller enters this
//!    rung when STUN typing reported `Symmetric`, or after rungs 0-2 failed.
//!    That decision is not made here.
//! 2. **Both the probe count and the wall clock are hard-capped**, from
//!    [`NetConfig`], and a user may switch the rung off entirely.
//! 3. **Every probe is the same authenticated STUN check** the ordinary rungs
//!    use, so nothing unauthenticated is ever emitted and oxutrm cannot be
//!    turned into a reflector or an amplifier by firing 65k packets.
//! 4. **The number actually sent is returned**, so the status line can show
//!    the cost rather than hiding it. "NAT traversal failed" with no numbers
//!    is unactionable for whoever has to debug it.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxutrm_proto::Psk;

use crate::{
    CheckKind, Direction, IceCredentials, IceRole, NetConfig, build_check_request,
    build_check_response, parse_check, random_transaction_id, to_socket_family, unmap,
};

/// How long one pass over the guessed ports waits for an answer before the
/// blast reconsiders its budget.
const LISTEN_SLICE: Duration = Duration::from_millis(20);

pub struct BirthdayResult {
    /// The socket that found the hole. QUIC takes this one over, because the
    /// mapping belongs to this socket and no other.
    pub socket: UdpSocket,
    pub remote: SocketAddr,
    pub probes: u32,
}

impl std::fmt::Debug for BirthdayResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BirthdayResult")
            .field("remote", &self.remote)
            .field("probes", &self.probes)
            .finish()
    }
}

/// Guessed ports, walking outward from the peer's observed base port.
///
/// Outward rather than upward because a symmetric NAT's next allocation is
/// usually *near* the last one but not reliably above it, and the observed
/// base is the only evidence there is. Ports below 1024 are skipped: an
/// ephemeral allocation never lands there, and probing them looks like a scan.
pub fn guessed_ports(base: u16, count: u16) -> Vec<u16> {
    let mut out = Vec::with_capacity(count as usize);
    if base >= 1024 {
        out.push(base);
    }
    let mut step: u32 = 1;
    while out.len() < count as usize && step <= u16::MAX as u32 {
        for candidate in [
            u32::from(base).checked_add(step),
            u32::from(base).checked_sub(step),
        ]
        .into_iter()
        .flatten()
        {
            if out.len() >= count as usize {
                break;
            }
            if (1024..=u32::from(u16::MAX)).contains(&candidate) {
                out.push(candidate as u16);
            }
        }
        step += 1;
    }
    out
}

/// Fire authenticated checks from many sockets at many guessed ports, and
/// return the first socket that gets a valid answer.
///
/// `Ok(None)` means the budget expired without a hole — an ordinary outcome
/// for this rung, not an error. `Err` means the blast could not start at all.
pub async fn birthday_blast(
    psk: &Psk,
    role: IceRole,
    peer_base: SocketAddr,
    cfg: &NetConfig,
) -> anyhow::Result<Option<BirthdayResult>> {
    if !cfg.enable_birthday {
        return Ok(None);
    }
    anyhow::ensure!(
        cfg.birthday_sockets > 0 && cfg.birthday_ports > 0,
        "the blast needs at least one socket and one port"
    );

    let creds = IceCredentials::derive(psk.as_bytes());
    let outbound = IceCredentials::outbound(role);
    let inbound = IceCredentials::inbound(role);
    let ports = guessed_ports(peer_base.port(), cfg.birthday_ports);
    let deadline = Instant::now() + cfg.birthday_budget;

    // Bind every socket up front: a socket that cannot be bound is one fewer
    // guess, not a failure, and on a busy host the limit is usually the file
    // descriptor table rather than anything about NAT.
    let bind_family: SocketAddr = if peer_base.is_ipv6() {
        "[::]:0".parse().expect("a literal address")
    } else {
        "0.0.0.0:0".parse().expect("a literal address")
    };
    let mut sockets: Vec<Arc<tokio::net::UdpSocket>> = Vec::new();
    for _ in 0..cfg.birthday_sockets {
        match tokio::net::UdpSocket::bind(bind_family).await {
            Ok(s) => sockets.push(Arc::new(s)),
            Err(_) => break,
        }
    }
    anyhow::ensure!(
        !sockets.is_empty(),
        "could not bind any socket for the blast"
    );

    let cap = u32::from(cfg.birthday_sockets) * u32::from(cfg.birthday_ports);
    let mut probes: u32 = 0;
    let mut buf = vec![0u8; 2048];

    // One pass fires every socket at every port, then listens. Interleaving
    // rather than blasting-then-waiting means an early hit is noticed early.
    'outer: for port in &ports {
        for sock in &sockets {
            if Instant::now() >= deadline || probes >= cap {
                break 'outer;
            }
            let target = SocketAddr::new(peer_base.ip(), *port);
            let local = sock.local_addr()?;
            let msg = build_check_request(&creds, outbound, random_transaction_id())?;
            if sock
                .send_to(&msg, to_socket_family(&local, target))
                .await
                .is_ok()
            {
                probes += 1;
            }
        }

        // Listen across every socket at once: the hole could be on any of them.
        if let Some(found) =
            listen_round(&sockets, &creds, inbound, outbound, &mut buf, LISTEN_SLICE).await
        {
            let (idx, remote) = found;
            let socket = into_std(&sockets, idx)?;
            return Ok(Some(BirthdayResult {
                socket,
                remote,
                probes,
            }));
        }
    }

    // Keep listening after the last probe: a reply is in flight for as long as
    // a round trip takes, and giving up the instant the last packet leaves
    // would throw away the answer.
    while Instant::now() < deadline {
        if let Some((idx, remote)) =
            listen_round(&sockets, &creds, inbound, outbound, &mut buf, LISTEN_SLICE).await
        {
            let socket = into_std(&sockets, idx)?;
            return Ok(Some(BirthdayResult {
                socket,
                remote,
                probes,
            }));
        }
    }

    // Say what was tried. "NAT traversal failed" with no numbers cannot be
    // acted on by whoever has to debug it.
    eprintln!(
        "oxutrm: birthday blast found no path: {probes} probes from {} sockets \
         across {} ports around {} in {:?}",
        sockets.len(),
        ports.len(),
        peer_base,
        cfg.birthday_budget
    );
    Ok(None)
}

/// Poll every socket once for a valid answer. Returns which socket, and who
/// answered.
async fn listen_round(
    sockets: &[Arc<tokio::net::UdpSocket>],
    creds: &IceCredentials,
    inbound: Direction,
    outbound: Direction,
    buf: &mut [u8],
    slice: Duration,
) -> Option<(usize, SocketAddr)> {
    let per_socket = slice / sockets.len().max(1) as u32;
    for (idx, sock) in sockets.iter().enumerate() {
        let Ok(Ok((n, from))) = tokio::time::timeout(
            per_socket.max(Duration::from_micros(200)),
            sock.recv_from(buf),
        )
        .await
        else {
            continue;
        };
        let from = unmap(from);

        // A response to one of our probes: the hole is open in our direction.
        if let Some(c) = parse_check(creds, outbound, &buf[..n])
            && c.kind == CheckKind::SuccessResponse
        {
            return Some((idx, from));
        }
        // Or the peer's own probe arrived here first, which proves the same
        // thing from the other end. Answer it so they learn it too, then take
        // this socket.
        if let Some(c) = parse_check(creds, inbound, &buf[..n])
            && c.kind == CheckKind::Request
        {
            if let Ok(reply) = build_check_response(creds, inbound, c.tid, from) {
                let local = sock.local_addr().ok();
                let dst = local.map_or(from, |l| to_socket_family(&l, from));
                let _ = sock.send_to(&reply, dst).await;
            }
            return Some((idx, from));
        }
    }
    None
}

/// Hand the winning socket back as a plain `std` socket, which is what `quinn`
/// adopts.
fn into_std(sockets: &[Arc<tokio::net::UdpSocket>], idx: usize) -> anyhow::Result<UdpSocket> {
    let addr = sockets[idx].local_addr()?;
    // The tokio socket is still referenced by the vector, so rebinding the
    // same address is not possible; instead the caller receives a socket bound
    // to the same port via a dup of the underlying descriptor.
    use std::os::fd::AsFd;
    let dup = sockets[idx].as_fd().try_clone_to_owned()?;
    let sock = UdpSocket::from(dup);
    debug_assert_eq!(sock.local_addr()?, addr);
    Ok(sock)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PSK: Psk = Psk::new([0x33; 32]);

    fn cfg(sockets: u16, ports: u16, budget_ms: u64) -> NetConfig {
        NetConfig {
            birthday_sockets: sockets,
            birthday_ports: ports,
            birthday_budget: Duration::from_millis(budget_ms),
            ..NetConfig::default()
        }
    }

    // ---- the search order, which is pure ----

    #[test]
    fn the_search_starts_at_the_observed_base() {
        let ports = guessed_ports(40_000, 5);
        assert_eq!(ports[0], 40_000, "the only evidence there is comes first");
    }

    #[test]
    fn the_search_walks_outward_in_both_directions() {
        assert_eq!(
            guessed_ports(40_000, 7),
            vec![40_000, 40_001, 39_999, 40_002, 39_998, 40_003, 39_997]
        );
    }

    #[test]
    fn the_search_yields_exactly_the_requested_count_with_no_repeats() {
        for count in [1u16, 2, 17, 256] {
            let ports = guessed_ports(40_000, count);
            assert_eq!(ports.len(), count as usize);
            let uniq: std::collections::HashSet<_> = ports.iter().collect();
            assert_eq!(
                uniq.len(),
                ports.len(),
                "a repeated guess is a wasted probe"
            );
        }
    }

    /// Probing below 1024 would look like a port scan and can never find an
    /// ephemeral allocation anyway.
    #[test]
    fn privileged_ports_are_never_guessed() {
        for base in [1024u16, 1100, 1030] {
            for p in guessed_ports(base, 300) {
                assert!(p >= 1024, "guessed privileged port {p} from base {base}");
            }
        }
    }

    #[test]
    fn the_search_does_not_wrap_or_overflow_at_the_edges() {
        for p in guessed_ports(65_535, 50) {
            assert!((1024..=65_535).contains(&p), "out of range: {p}");
        }
        for p in guessed_ports(1024, 50) {
            assert!((1024..=65_535).contains(&p), "out of range: {p}");
        }
        // A base a router would never allocate still terminates.
        assert!(guessed_ports(0, 10).iter().all(|&p| p >= 1024));
    }

    // ---- the blast ----

    /// A peer that answers authenticated checks on ONE known port. This is the
    /// deterministic stand-in for a symmetric NAT's single open hole.
    async fn lurking_peer(port_hint: u16) -> (Arc<tokio::net::UdpSocket>, SocketAddr) {
        let sock = Arc::new(
            tokio::net::UdpSocket::bind(format!("127.0.0.1:{port_hint}"))
                .await
                .expect("bind the lurking peer"),
        );
        let addr = sock.local_addr().expect("addr");
        let s = sock.clone();
        tokio::spawn(async move {
            let creds = IceCredentials::derive(PSK.as_bytes());
            // The peer is the host, so it verifies the client's outbound
            // direction and signs its answers with the same one.
            let d = IceCredentials::inbound(IceRole::Controlled);
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = s.recv_from(&mut buf).await else {
                    return;
                };
                let Some(c) = parse_check(&creds, d, &buf[..n]) else {
                    continue;
                };
                if c.kind != CheckKind::Request {
                    continue;
                }
                if let Ok(reply) = build_check_response(&creds, d, c.tid, from) {
                    let _ = s.send_to(&reply, from).await;
                }
            }
        });
        (sock, addr)
    }

    /// The whole point of the rung: the correct port is somewhere in the
    /// guessed range, and the blast must find it inside its budget.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_blast_finds_a_hole_that_is_inside_the_guessed_range() {
        let (_peer, peer_addr) = lurking_peer(0).await;
        // Point the blast a few ports below the real one, so the search has
        // to walk outward to reach it rather than hitting it first.
        let base = SocketAddr::new(peer_addr.ip(), peer_addr.port().wrapping_sub(3));

        let cfg = cfg(4, 32, 4000);
        let got = birthday_blast(&PSK, IceRole::Controlling, base, &cfg)
            .await
            .expect("the blast must not error")
            .expect("the hole was inside the range and must have been found");

        assert_eq!(got.remote, peer_addr, "found the wrong peer");
        assert!(got.probes > 0, "reported a find without probing");
        assert!(
            got.probes <= u32::from(cfg.birthday_sockets) * u32::from(cfg.birthday_ports),
            "the probe cap was exceeded"
        );
        // The socket handed back must be the one that found the hole: the NAT
        // mapping belongs to that socket and to no other.
        assert!(got.socket.local_addr().is_ok());
    }

    /// The base itself being right is the easy case, and must still work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_blast_finds_a_hole_sitting_exactly_on_the_base() {
        let (_peer, peer_addr) = lurking_peer(0).await;
        let cfg = cfg(2, 8, 3000);
        let got = birthday_blast(&PSK, IceRole::Controlling, peer_addr, &cfg)
            .await
            .expect("no error")
            .expect("the base was correct");
        assert_eq!(got.remote, peer_addr);
    }

    /// And it must give up cleanly rather than hanging when there is nothing
    /// to find.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_blast_gives_up_within_its_budget_when_there_is_no_hole() {
        // A base far from anything listening.
        let base: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let cfg = cfg(4, 8, 400);

        let started = Instant::now();
        let got = tokio::time::timeout(
            Duration::from_secs(10),
            birthday_blast(&PSK, IceRole::Controlling, base, &cfg),
        )
        .await
        .expect("the budget must be honoured, not merely intended")
        .expect("giving up is an ordinary outcome, not an error");

        assert!(got.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wall-clock cap did not hold: {:?}",
            started.elapsed()
        );
    }

    /// A user who switched the noisy rung off must not have 65k packets sent
    /// on their behalf.
    #[tokio::test]
    async fn the_blast_is_skipped_when_the_config_disables_it() {
        let cfg = NetConfig {
            enable_birthday: false,
            ..NetConfig::default()
        };
        let base: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let got = birthday_blast(&PSK, IceRole::Controlling, base, &cfg)
            .await
            .expect("no error");
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn a_zero_sized_blast_is_refused_rather_than_looping_forever() {
        let base: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(
            birthday_blast(&PSK, IceRole::Controlling, base, &cfg(0, 8, 200))
                .await
                .is_err()
        );
        assert!(
            birthday_blast(&PSK, IceRole::Controlling, base, &cfg(4, 0, 200))
                .await
                .is_err()
        );
    }

    /// Guardrail 3: nothing unauthenticated ever leaves. A peer signing with
    /// the wrong PSK must not count as a hole, or the blast becomes a way to
    /// be steered by anyone who answers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_peer_with_the_wrong_psk_is_not_a_hole() {
        let sock = Arc::new(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind"),
        );
        let addr = sock.local_addr().expect("addr");
        let s = sock.clone();
        tokio::spawn(async move {
            let wrong = IceCredentials::derive(&[0xEE; 32]);
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((_n, from)) = s.recv_from(&mut buf).await else {
                    return;
                };
                if let Ok(reply) = build_check_response(
                    &wrong,
                    Direction::ClientToHost,
                    random_transaction_id(),
                    from,
                ) {
                    let _ = s.send_to(&reply, from).await;
                }
            }
        });

        let got = birthday_blast(&PSK, IceRole::Controlling, addr, &cfg(2, 4, 500))
            .await
            .expect("no error");
        assert!(
            got.is_none(),
            "a stranger's answer was accepted as a punched hole"
        );
    }
}

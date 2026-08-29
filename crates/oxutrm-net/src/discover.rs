//! Finding out what address the world sees us at, and what kind of NAT is in
//! the way.
//!
//! # Query from the socket QUIC will actually use
//!
//! NAT mappings are **per-socket**. An address discovered on any other socket
//! describes a hole that our traffic will never come out of, so
//! [`stun_discover`] takes the session socket and nothing else. This is also
//! why an HTTP echo service is not an acceptable substitute: it answers over
//! TCP and reports only the IP, while the port is the hard half.
//!
//! # The probes, and which verdict each one can reach
//!
//! | Probe | Destination | What it isolates |
//! |---|---|---|
//! | **P1** | the first server, at its configured port | the baseline mapping |
//! | **P2** | a **genuinely different server IP** | whether the destination *IP* moves the mapping |
//! | **P3** | a **second configured port on P1's IP**, when the server list names one | whether the destination *port* moves the mapping |
//!
//! P1 and P2 are the load-bearing pair, and they are the only two the default
//! configuration can actually run. A mapping that differs between two
//! destination IPs is per-destination — the address a STUN server reports is
//! then not the address a peer will see — and that is the `Symmetric` verdict
//! that sends the ladder to rung 3.
//!
//! P3 is the refinement, and it is **best effort by configuration, never by
//! guesswork**. An address-dependent mapping keys on the destination IP alone,
//! so a second port on the same server reuses it; a symmetric mapping keys on
//! both, so the second port gets a new external port. Only a server that
//! genuinely answers on two ports can show the difference, so P3 runs only when
//! [`crate::NetConfig::stun_servers`] names one — two entries with the same
//! resolved IP and different ports. When it does not run, the two verdicts stay
//! merged under `Symmetric`.
//!
//! ## Why P3 is not `P1.port() + 1`
//!
//! It used to be, and that made `Symmetric` unreachable in every real
//! deployment. RFC 5780's 3478/3479 pairing is a convention some operators
//! follow, not a property of a `host:port` string: nothing in the type says
//! the neighbouring port is served, and no entry in `NetConfig::default()`
//! promises it — the first one, `stun.cloudflare.com:3478`, does not answer on
//! 3479, and `stun.nextcloud.com:443` plainly has nothing at 444. So the
//! guessed probe timed out every time, the classifier never saw more than two
//! answers, and the arm that fires the birthday blast was dead code outside
//! the test suite — where a helper stood a responder up on `port + 1` and
//! manufactured the one precondition production could never meet.
//!
//! A configured entry is a claim the operator can be held to; `port + 1` is a
//! guess the configuration never made.
//!
//! ## What the merged verdict costs, and why it is the right way round
//!
//! Without P3, `AddressDependent` and `Symmetric` are indistinguishable and
//! both are reported as `Symmetric`. Calling a genuinely address-dependent NAT
//! symmetric skips rung 2, whose premise — that the server-reflexive candidate
//! we advertised is an address the peer can reach — is already false for a
//! mapping that changes per destination; rung 3 subsumes the attempt anyway.
//! Calling a genuinely symmetric NAT `Unknown`, which is what the old table
//! did, burns the whole gather budget on rung 2 to learn something the probes
//! had already established. Neither loses the connection; the merged verdict
//! is the cheaper mistake, and an operator who wants the distinction back can
//! configure the alternate port that produces it.

use std::net::{IpAddr, SocketAddr};

use oxutrm_proto::{Candidate, CandidateKind, NatType};

use crate::{NetConfig, ice_priority, unmap};

/// One observed mapping, and which probe observed it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Probe {
    pub server: SocketAddr,
    pub mapped: SocketAddr,
}

/// The truth table, as a pure function, so it is testable without a socket.
///
/// The argument order is the order the probes are *described* in, not the
/// order they run: `alt_port` is the optional refinement and `other_ip` is the
/// probe that actually decides.
///
/// | P1 vs alt-port (same IP) | P1 vs other-IP | result |
/// |---|---|---|
/// | all equal, and equal to our own address | — | `None` |
/// | same or absent | same | `EndpointIndependent` |
/// | same | **different** | `AddressDependent` |
/// | **absent** | **different** | `Symmetric` (merged with `AddressDependent`) |
/// | **different** | anything | `Symmetric` |
///
/// A probe can simply time out, and the degradations are deliberate.
///
/// | Answers in hand | result | why |
/// |---|---|---|
/// | none, or P1 alone | `Unknown` | nothing to compare |
/// | P1 + alt-port equal | `Unknown` | one server cannot show whether the destination IP moves the mapping |
/// | P1 + alt-port different | `Symmetric` | the port alone moved the mapping; nothing else does that |
/// | P1 + other-IP equal | `EndpointIndependent` | the destination IP does not move the mapping |
/// | P1 + other-IP different | `Symmetric` | the mapping is per-destination; without an alternate port this cannot be narrowed to `AddressDependent`, and the module docs explain why that is the cheaper mistake |
pub fn classify(
    same_ip_same_port: Option<SocketAddr>,
    same_ip_alt_port: Option<SocketAddr>,
    other_ip: Option<SocketAddr>,
    local: SocketAddr,
    local_ips: &[IpAddr],
) -> NatType {
    let Some(p1) = same_ip_same_port else {
        return NatType::Unknown;
    };

    // No NAT at all: every server saw the address we are actually bound to.
    // The port must match too — a NAT that happens to preserve the IP but not
    // the port is still a NAT.
    let is_ours = p1.port() == local.port() && local_ips.contains(&p1.ip());
    let all_agree = [same_ip_alt_port, other_ip]
        .into_iter()
        .flatten()
        .all(|p| p == p1);
    if is_ours && all_agree && (same_ip_alt_port.is_some() || other_ip.is_some()) {
        return NatType::None;
    }

    match (same_ip_alt_port, other_ip) {
        // The destination port alone changed the mapping. Only an
        // address-and-port-dependent NAT does that, so this is conclusive even
        // without the other-IP probe.
        (Some(p2), _) if p2 != p1 => NatType::Symmetric,

        // The port did not move it and the IP did not either.
        (Some(_), Some(p3)) if p3 == p1 => NatType::EndpointIndependent,

        // The port did not move it but the IP did: keyed on destination IP.
        (Some(_), Some(_)) => NatType::AddressDependent,

        // The alternate port agreed but no other server IP answered. One
        // server cannot show whether the destination IP moves the mapping, so
        // nothing above EndpointIndependent is provable and nothing below it
        // is ruled out.
        (Some(_), None) => NatType::Unknown,

        // No alternate port -- the ordinary case, because the default server
        // list has none. The other-IP probe alone still settles the question
        // the ladder asks: does the mapping change per destination?
        (None, Some(p3)) if p3 == p1 => NatType::EndpointIndependent,
        (None, Some(_)) => NatType::Symmetric,
        (None, None) => NatType::Unknown,
    }
}

/// Query several STUN servers from `socket`; classify the NAT by comparing the
/// mapped addresses they report.
///
/// The probes run **one at a time**.
/// `stunclient::StunClient::query_external_address_async` owns `recv_from` on
/// the socket for as long as it runs and discards every datagram whose source
/// is not its own server, so two of them on one socket would eat each other's
/// replies. Each gets a third of the gather budget.
///
/// This departs from the design spec's "queried in parallel" (§5.3). The spec
/// describes an outcome — three probes inside the gather budget — which
/// sequential queries with a divided timeout achieve just as well. Doing it in
/// parallel would mean re-implementing discovery on `stun_codec`, and
/// `stunclient` is reserved for exactly this job.
pub async fn stun_discover(
    socket: &tokio::net::UdpSocket,
    cfg: &NetConfig,
) -> (Vec<Candidate>, NatType) {
    let local = match socket.local_addr() {
        Ok(a) => unmap(a),
        Err(_) => return (Vec::new(), NatType::Unknown),
    };
    let per_probe = cfg.gather_timeout / 3;

    let servers = resolve_servers(&cfg.stun_servers);
    let Some(&first) = servers.first() else {
        return (Vec::new(), NatType::Unknown);
    };

    // P1: the baseline.
    let p1 = probe(socket, first, per_probe).await;

    // P2: a genuinely different IP. A second server behind the same address
    // would answer, agree, and prove nothing. This is the probe that reaches
    // `Symmetric`, and the only one besides P1 the default list can supply.
    let other = servers.iter().copied().find(|s| s.ip() != first.ip());
    let p_other = match other {
        Some(s) => probe(socket, s, per_probe).await,
        None => None,
    };

    // P3: a second port on P1's IP, and ONLY one the server list actually
    // names. Guessing `port + 1` looks like RFC 5780's 3478/3479 convention
    // and is not one: none of the default servers answer there, so the probe
    // timed out every time and the `Symmetric` arm it feeds was unreachable
    // outside the tests. Configure the pair or do without the refinement.
    let alt = servers
        .iter()
        .copied()
        .find(|s| s.ip() == first.ip() && s.port() != first.port());
    let p_alt = match alt {
        Some(s) => probe(socket, s, per_probe).await,
        None => None,
    };

    let local_ips = local_ip_set();
    let nat = classify(p1, p_alt, p_other, local, &local_ips);

    // Every distinct mapped address is a server-reflexive candidate, including
    // the extra ones a symmetric NAT produced: a peer may still be able to
    // reach one of them.
    let mut candidates: Vec<Candidate> = Vec::new();
    for mapped in [p1, p_other, p_alt].into_iter().flatten() {
        let mapped = unmap(mapped);
        if candidates.iter().any(|c| c.addr == mapped) {
            continue;
        }
        candidates.push(Candidate {
            addr: mapped,
            kind: CandidateKind::ServerReflexive,
            priority: ice_priority(CandidateKind::ServerReflexive, &mapped.ip()),
        });
    }
    candidates.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.addr.cmp(&b.addr)));

    (candidates, nat)
}

/// One Binding Request, and the address it reported.
async fn probe(
    socket: &tokio::net::UdpSocket,
    server: SocketAddr,
    timeout: std::time::Duration,
) -> Option<SocketAddr> {
    let mut client = stunclient::StunClient::new(server);
    client.set_timeout(timeout);
    client
        .query_external_address_async(socket)
        .await
        .ok()
        .map(unmap)
}

/// Resolve the configured `host:port` strings. An entry that does not resolve
/// is skipped rather than fatal: a STUN server list is a list of hopes.
fn resolve_servers(servers: &[String]) -> Vec<SocketAddr> {
    use std::net::ToSocketAddrs;
    let mut out: Vec<SocketAddr> = Vec::new();
    for s in servers {
        let Ok(addrs) = s.to_socket_addrs() else {
            continue;
        };
        // One address per name: the point of the list is distinct operators,
        // and a second A record for the same host proves nothing.
        if let Some(a) = addrs.into_iter().next()
            && !out.contains(&a)
        {
            out.push(a);
        }
    }
    out
}

/// Our own addresses, so a mapping identical to one of them means no NAT.
fn local_ip_set() -> Vec<IpAddr> {
    let mut ips: Vec<IpAddr> = Vec::new();
    for iface in netdev::get_interfaces() {
        ips.extend(iface.ipv6.iter().map(|n| IpAddr::V6(n.addr())));
        ips.extend(iface.ipv4.iter().map(|n| IpAddr::V4(n.addr())));
    }
    ips
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stunserver::{MappingBehaviour, StunResponder};

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    // ---- the truth table, exhaustively ----

    #[test]
    fn no_nat_when_every_server_saw_our_own_address() {
        let ours = a("198.51.100.7:443");
        let mine = [ip("198.51.100.7")];
        assert_eq!(
            classify(Some(ours), Some(ours), Some(ours), ours, &mine),
            NatType::None
        );
    }

    /// A NAT that preserves the IP but not the port is still a NAT.
    #[test]
    fn a_preserved_ip_with_a_changed_port_is_not_no_nat() {
        let local = a("198.51.100.7:443");
        let seen = a("198.51.100.7:51000");
        let mine = [ip("198.51.100.7")];
        assert_eq!(
            classify(Some(seen), Some(seen), Some(seen), local, &mine),
            NatType::EndpointIndependent
        );
    }

    #[test]
    fn endpoint_independent_when_neither_the_port_nor_the_ip_moves_it() {
        let seen = a("203.0.113.9:51000");
        assert_eq!(
            classify(Some(seen), Some(seen), Some(seen), a("10.0.0.2:443"), &[]),
            NatType::EndpointIndependent
        );
    }

    /// The distinction two probes cannot make.
    #[test]
    fn address_dependent_when_the_ip_moves_it_but_the_port_does_not() {
        let p1 = a("203.0.113.9:51000");
        let p3 = a("203.0.113.9:51001");
        assert_eq!(
            classify(Some(p1), Some(p1), Some(p3), a("10.0.0.2:443"), &[]),
            NatType::AddressDependent
        );
    }

    #[test]
    fn symmetric_when_the_destination_port_alone_moves_the_mapping() {
        let p1 = a("203.0.113.9:51000");
        let p2 = a("203.0.113.9:51001");
        let p3 = a("203.0.113.9:51002");
        assert_eq!(
            classify(Some(p1), Some(p2), Some(p3), a("10.0.0.2:443"), &[]),
            NatType::Symmetric
        );
    }

    /// The alternate port differing is conclusive on its own: nothing but an
    /// address-and-port-dependent mapping reacts to the destination port, so
    /// no other-IP probe is needed to say so.
    #[test]
    fn symmetric_is_conclusive_without_the_other_ip_probe() {
        let p1 = a("203.0.113.9:51000");
        let p2 = a("203.0.113.9:51001");
        assert_eq!(
            classify(Some(p1), Some(p2), None, a("10.0.0.2:443"), &[]),
            NatType::Symmetric
        );
    }

    /// The verdict that has to be reachable without an alternate port, because
    /// no public STUN server in the default list serves one. Without the
    /// alternate port the answer cannot be narrowed to `AddressDependent`, and
    /// the merged verdict is deliberate — see the module docs.
    #[test]
    fn a_mapping_that_differs_between_two_server_ips_is_symmetric() {
        let p1 = a("203.0.113.9:51000");
        let p3 = a("203.0.113.9:51002");
        assert_eq!(
            classify(Some(p1), None, Some(p3), a("10.0.0.2:443"), &[]),
            NatType::Symmetric,
        );
    }

    /// The alternate port is what buys the distinction back: the same evidence
    /// as above, plus a same-IP second port that agreed, is address-dependent
    /// and NOT a reason to fire the birthday blast.
    #[test]
    fn a_configured_alternate_port_narrows_symmetric_to_address_dependent() {
        let p1 = a("203.0.113.9:51000");
        let p3 = a("203.0.113.9:51002");
        assert_eq!(
            classify(Some(p1), Some(p1), Some(p3), a("10.0.0.2:443"), &[]),
            NatType::AddressDependent,
        );
    }

    /// The shape of `NetConfig::default()`: servers on distinct IPs, and
    /// **nothing** answering on `port + 1` anywhere. If the classifier can
    /// only reach `Symmetric` through an alternate port, this fails.
    #[tokio::test]
    async fn a_default_shaped_server_list_still_reaches_symmetric() {
        const NAME: &str = "a_default_shaped_server_list_still_reaches_symmetric";
        let Some(second) = second_loopback_ip() else {
            eprintln!(
                "SKIP {}: this host has no second loopback IP \
                 (macOS: sudo ifconfig lo0 alias 127.0.0.2)",
                NAME
            );
            return;
        };
        let (s1, _hole) =
            responder_with_a_silent_neighbour("127.0.0.1", MappingBehaviour::RewritePort(50_001))
                .await;
        let s3 = StunResponder::start_on(
            a(&format!("{second}:0")),
            MappingBehaviour::RewritePort(50_002),
        )
        .await
        .expect("a second loopback IP");

        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = NetConfig {
            stun_servers: vec![s1.server_string(), s3.server_string()],
            gather_timeout: std::time::Duration::from_secs(3),
            ..NetConfig::default()
        };
        let (_cands, nat) = stun_discover(&sock, &cfg).await;
        assert_eq!(
            nat,
            NatType::Symmetric,
            "a per-destination mapping must be detectable from the servers the \
             default configuration actually has"
        );
    }

    /// Start a responder on `ip` and hold a **silent** socket on its
    /// `port + 1`, so the alternate-port probe provably gets no answer. This
    /// is the inverse of a helper that stands a responder up there: it
    /// reproduces production rather than papering over it.
    /// A second loopback IPv4 address, or `None` where this host has none.
    ///
    /// Linux puts the whole of `127.0.0.0/8` on `lo`, so `127.0.0.2` is simply
    /// there. macOS assigns only `127.0.0.1` to `lo0`; a second address needs
    /// `sudo ifconfig lo0 alias 127.0.0.2`, which a test may not require.
    ///
    /// Probed, not `cfg`-ed, for the same reason `FD_DIRS` is a candidate list:
    /// a machine that HAS the alias then runs the test on either platform, and
    /// the decision follows the host rather than the compile target.
    fn second_loopback_ip() -> Option<&'static str> {
        std::net::UdpSocket::bind("127.0.0.2:0")
            .ok()
            .map(|_| "127.0.0.2")
    }

    async fn responder_with_a_silent_neighbour(
        ip: &str,
        behaviour: MappingBehaviour,
    ) -> (StunResponder, std::net::UdpSocket) {
        let base = {
            let probe = std::net::UdpSocket::bind(format!("{ip}:0")).expect("probe");
            probe.local_addr().expect("addr").port()
        };
        for port in base..base.saturating_add(400) {
            let Ok(responder) =
                StunResponder::start_on(a(&format!("{ip}:{port}")), behaviour).await
            else {
                continue;
            };
            if let Ok(hole) = std::net::UdpSocket::bind(format!("{ip}:{}", port + 1)) {
                return (responder, hole);
            }
        }
        panic!("could not find a free port with a free neighbour on {ip}");
    }

    #[test]
    fn the_degradations_are_honest_rather_than_confident() {
        let p1 = a("203.0.113.9:51000");
        let local = a("10.0.0.2:443");

        // Nothing at all.
        assert_eq!(classify(None, None, None, local, &[]), NatType::Unknown);
        // P1 alone compares with nothing.
        assert_eq!(classify(Some(p1), None, None, local, &[]), NatType::Unknown);
        // The alternate port agreed but no second server IP answered: one
        // server cannot show whether the destination IP moves the mapping.
        assert_eq!(
            classify(Some(p1), Some(p1), None, local, &[]),
            NatType::Unknown
        );
        // The other-IP probe alone, agreeing, is enough for
        // endpoint-independence.
        assert_eq!(
            classify(Some(p1), None, Some(p1), local, &[]),
            NatType::EndpointIndependent
        );
        // A missing P1 makes the others meaningless.
        assert_eq!(
            classify(None, Some(p1), Some(p1), local, &[]),
            NatType::Unknown
        );
    }

    /// `None` requires corroboration: one probe agreeing with our own address
    /// is not evidence that nothing rewrites it.
    #[test]
    fn a_single_probe_never_yields_no_nat() {
        let ours = a("198.51.100.7:443");
        let mine = [ip("198.51.100.7")];
        assert_eq!(
            classify(Some(ours), None, None, ours, &mine),
            NatType::Unknown
        );
    }

    // ---- end to end, against local responders ----

    /// The whole path, end to end, against local responders only, in the shape
    /// production actually has: two server IPs and nothing on `port + 1`.
    #[tokio::test]
    async fn two_truthful_probes_classify_a_direct_path_as_no_nat() {
        const NAME: &str = "two_truthful_probes_classify_a_direct_path_as_no_nat";
        let Some(second) = second_loopback_ip() else {
            eprintln!(
                "SKIP {}: this host has no second loopback IP \
                 (macOS: sudo ifconfig lo0 alias 127.0.0.2)",
                NAME
            );
            return;
        };
        let (s1, _hole) =
            responder_with_a_silent_neighbour("127.0.0.1", MappingBehaviour::Truthful).await;
        let s3 = StunResponder::start_on(a(&format!("{second}:0")), MappingBehaviour::Truthful)
            .await
            .expect("a second loopback IP");

        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = NetConfig {
            stun_servers: vec![s1.server_string(), s3.server_string()],
            gather_timeout: std::time::Duration::from_secs(3),
            ..NetConfig::default()
        };
        let (cands, nat) = stun_discover(&sock, &cfg).await;

        // Every probe saw our real address, so nothing rewrites it.
        assert_eq!(nat, NatType::None, "a truthful loopback path has no NAT");
        assert!(!cands.is_empty(), "discovery produced no candidate at all");
        for c in &cands {
            assert_eq!(c.kind, CandidateKind::ServerReflexive);
        }
    }

    /// A server list confined to ONE IP can never supply the other-IP probe,
    /// so it can never reach a confident answer. Worth asserting: it is the
    /// failure mode of a well-meaning `stun_servers` list that names one host
    /// twice.
    #[tokio::test]
    async fn servers_that_share_an_ip_cannot_classify() {
        // Both rewrite to the SAME port, so the alternate-port probe agrees
        // and the mapping is provably not our own address. What is left is
        // exactly the gap one IP cannot close.
        let s1 = StunResponder::start(MappingBehaviour::RewritePort(50_000))
            .await
            .expect("s1");
        let s2 = StunResponder::start(MappingBehaviour::RewritePort(50_000))
            .await
            .expect("s2");
        assert_eq!(s1.addr().ip(), s2.addr().ip());

        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = NetConfig {
            stun_servers: vec![s1.server_string(), s2.server_string()],
            gather_timeout: std::time::Duration::from_secs(2),
            ..NetConfig::default()
        };
        let (cands, nat) = stun_discover(&sock, &cfg).await;
        assert_eq!(
            nat,
            NatType::Unknown,
            "two servers on one IP cannot show whether the destination IP \
             moves the mapping, so nothing above EndpointIndependent is provable"
        );
        assert!(!cands.is_empty(), "discovery still learned our address");
    }

    /// The refinement, and the only way to reach it: the operator names a
    /// second port on the first server's IP, and it disagrees.
    #[tokio::test]
    async fn a_configured_alternate_port_that_disagrees_is_symmetric() {
        let s1 = StunResponder::start(MappingBehaviour::RewritePort(50_001))
            .await
            .expect("s1");
        let s1_alt = StunResponder::start(MappingBehaviour::RewritePort(50_002))
            .await
            .expect("s1 alt port");
        assert_eq!(s1.addr().ip(), s1_alt.addr().ip());

        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = NetConfig {
            stun_servers: vec![s1.server_string(), s1_alt.server_string()],
            gather_timeout: std::time::Duration::from_secs(3),
            ..NetConfig::default()
        };
        let (_cands, nat) = stun_discover(&sock, &cfg).await;
        assert_eq!(
            nat,
            NatType::Symmetric,
            "a mapping that changes with the destination port is symmetric, \
             and symmetric is what sends the ladder to rung 3"
        );
    }

    /// The distinction the alternate port exists to make: the same port from
    /// the alt-port probe, a different one from another IP, is
    /// address-dependent — NOT symmetric, and NOT a reason to fire the
    /// birthday blast. It needs a server list that names the alternate port,
    /// because the default one does not.
    #[tokio::test]
    async fn a_mapping_keyed_on_destination_ip_alone_is_address_dependent() {
        const NAME: &str = "a_mapping_keyed_on_destination_ip_alone_is_address_dependent";
        let Some(second) = second_loopback_ip() else {
            eprintln!(
                "SKIP {}: this host has no second loopback IP \
                 (macOS: sudo ifconfig lo0 alias 127.0.0.2)",
                NAME
            );
            return;
        };
        let s1 = StunResponder::start(MappingBehaviour::RewritePort(60_001))
            .await
            .expect("s1");
        // Same reported port: the destination PORT did not move it.
        let s1_alt = StunResponder::start(MappingBehaviour::RewritePort(60_001))
            .await
            .expect("s1 alt port");
        // Different IP, different reported port: the destination IP did.
        let s3 = StunResponder::start_on(
            a(&format!("{second}:0")),
            MappingBehaviour::RewritePort(60_002),
        )
        .await
        .expect("a second loopback IP");

        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = NetConfig {
            stun_servers: vec![
                s1.server_string(),
                s1_alt.server_string(),
                s3.server_string(),
            ],
            gather_timeout: std::time::Duration::from_secs(3),
            ..NetConfig::default()
        };
        let (_cands, nat) = stun_discover(&sock, &cfg).await;
        assert_eq!(
            nat,
            NatType::AddressDependent,
            "without the configured alternate port this merges into Symmetric \
             and rung 2 is skipped for nothing"
        );
    }

    #[tokio::test]
    async fn a_server_that_never_answers_degrades_to_unknown() {
        // A port nobody is listening on.
        let dead = {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
            let a = s.local_addr().expect("addr");
            drop(s);
            a
        };
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = NetConfig {
            stun_servers: vec![dead.to_string()],
            gather_timeout: std::time::Duration::from_millis(300),
            ..NetConfig::default()
        };
        let (cands, nat) = stun_discover(&sock, &cfg).await;
        assert_eq!(nat, NatType::Unknown);
        assert!(cands.is_empty(), "a silent server produced a candidate");
    }

    #[tokio::test]
    async fn an_empty_server_list_is_unknown_rather_than_a_panic() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = NetConfig {
            stun_servers: Vec::new(),
            ..NetConfig::default()
        };
        let (cands, nat) = stun_discover(&sock, &cfg).await;
        assert_eq!(nat, NatType::Unknown);
        assert!(cands.is_empty());
    }

    #[test]
    fn unresolvable_servers_are_skipped_rather_than_fatal() {
        let got = resolve_servers(&[
            "127.0.0.1:3478".to_string(),
            "this-host-does-not-exist.invalid:3478".to_string(),
            "127.0.0.2:3478".to_string(),
        ]);
        assert_eq!(got.len(), 2, "a bad entry must be skipped, not fatal");
    }

    /// Discovery must query the session socket itself: a mapping learned on
    /// any other socket describes a hole our traffic never comes out of.
    #[tokio::test]
    async fn discovery_reports_the_querying_sockets_own_port() {
        let server = StunResponder::start(MappingBehaviour::Truthful)
            .await
            .expect("start");
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let ours = sock.local_addr().expect("addr");

        let cfg = NetConfig {
            stun_servers: vec![server.server_string()],
            gather_timeout: std::time::Duration::from_secs(3),
            ..NetConfig::default()
        };
        let (cands, _) = stun_discover(&sock, &cfg).await;
        assert!(
            cands.iter().any(|c| c.addr.port() == ours.port()),
            "discovery reported a port that is not this socket's: {cands:?}"
        );
    }

    /// Never in CI: a third party's uptime must not decide whether our build
    /// is red. Run with `--ignored` to check the real defaults still work.
    #[tokio::test]
    #[ignore = "talks to public STUN servers"]
    async fn the_default_servers_work_against_the_real_internet() {
        let sock = tokio::net::UdpSocket::bind("0.0.0.0:0")
            .await
            .expect("bind");
        let (cands, nat) = stun_discover(&sock, &NetConfig::default()).await;
        assert!(!cands.is_empty(), "no public server answered");
        assert_ne!(nat, NatType::Unknown, "could not classify the NAT");
    }
}

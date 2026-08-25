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
//! # Three probes, not two
//!
//! Two servers at two different IPs separate `EndpointIndependent` from
//! everything else, and nothing more. They **cannot** tell `AddressDependent`
//! from `Symmetric`, because both allocate a fresh mapping when the
//! destination IP changes. The difference shows only when the destination
//! **port** changes while the IP stays the same:
//!
//! - an **address-dependent** mapping keys on the destination IP alone, so a
//!   second port on the same server reuses the mapping;
//! - a **symmetric** mapping keys on both, so a second port on the same server
//!   gets a new external port.
//!
//! | Probe | Destination | What it isolates |
//! |---|---|---|
//! | **P1** | first server, its configured port | the baseline mapping |
//! | **P2** | the same IP as P1, **port + 1** | whether the destination *port* moves the mapping |
//! | **P3** | a **different** server IP | whether the destination *IP* moves the mapping |
//!
//! This matters concretely, not academically. `Symmetric` is what sends the
//! ladder to rung 3, the birthday blast. Misclassifying it as
//! `AddressDependent` means several seconds spent failing at ordinary hole
//! punching first — on exactly the connections that are already hardest.
//!
//! **Anyone tempted to "simplify" this back to two probes is deleting a
//! `NatType` variant.** The tests below encode the full table; they will fail.

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
/// | P1 vs P2 (same IP, different port) | P1 vs P3 (different IP) | result |
/// |---|---|---|
/// | all equal, and equal to our own address | — | `None` |
/// | same | same | `EndpointIndependent` |
/// | same | **different** | `AddressDependent` |
/// | **different** | anything | `Symmetric` |
///
/// A probe can simply time out, and the degradations are deliberate: an
/// honest `Unknown` costs one wasted rung, whereas a confident wrong answer
/// costs the connection.
///
/// | Answers in hand | result | why |
/// |---|---|---|
/// | none, or P1 alone | `Unknown` | nothing to compare |
/// | P1 + P2 equal | `Unknown` | cannot rule out `AddressDependent` |
/// | P1 + P2 different | `Symmetric` | the port alone moved the mapping; nothing else does that |
/// | P1 + P3 equal | `EndpointIndependent` | neither IP nor port moved it |
/// | P1 + P3 different | `Unknown` | could be `AddressDependent` or `Symmetric` |
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
        // without P3.
        (Some(p2), _) if p2 != p1 => NatType::Symmetric,

        // The port did not move it and the IP did not either.
        (Some(_), Some(p3)) if p3 == p1 => NatType::EndpointIndependent,

        // The port did not move it but the IP did: keyed on destination IP.
        (Some(_), Some(_)) => NatType::AddressDependent,

        // P2 agreed but P3 never answered: cannot rule out AddressDependent.
        (Some(_), None) => NatType::Unknown,

        // No P2. P3 alone can only confirm endpoint-independence; if it
        // differs, AddressDependent and Symmetric are indistinguishable.
        (None, Some(p3)) if p3 == p1 => NatType::EndpointIndependent,
        (None, Some(_)) => NatType::Unknown,
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

    // P2: the same IP, port + 1. RFC 5780's convention (3478/3479), which
    // dedicated STUN servers follow and `stun.l.google.com:19302` does not —
    // hence best effort, degrading to `Unknown` rather than inventing a type.
    let alt = SocketAddr::new(first.ip(), first.port().wrapping_add(1));
    let p2 = probe(socket, alt, per_probe).await;

    // P3: a genuinely different IP. A second server behind the same address
    // would answer, agree, and prove nothing.
    let other = servers.iter().copied().find(|s| s.ip() != first.ip());
    let p3 = match other {
        Some(s) => probe(socket, s, per_probe).await,
        None => None,
    };

    let local_ips = local_ip_set();
    let nat = classify(p1, p2, p3, local, &local_ips);

    // Every distinct mapped address is a server-reflexive candidate, including
    // the extra ones a symmetric NAT produced: a peer may still be able to
    // reach one of them.
    let mut candidates: Vec<Candidate> = Vec::new();
    for mapped in [p1, p2, p3].into_iter().flatten() {
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

    /// P2 differing is conclusive on its own: nothing but an
    /// address-and-port-dependent mapping reacts to the destination port.
    #[test]
    fn symmetric_is_conclusive_without_the_third_probe() {
        let p1 = a("203.0.113.9:51000");
        let p2 = a("203.0.113.9:51001");
        assert_eq!(
            classify(Some(p1), Some(p2), None, a("10.0.0.2:443"), &[]),
            NatType::Symmetric
        );
    }

    /// Exactly the gap that makes two probes insufficient: with P2 missing,
    /// a moved mapping could be either type, and guessing would send us down
    /// the wrong rung.
    #[test]
    fn two_probes_alone_cannot_separate_address_dependent_from_symmetric() {
        let p1 = a("203.0.113.9:51000");
        let p3 = a("203.0.113.9:51002");
        assert_eq!(
            classify(Some(p1), None, Some(p3), a("10.0.0.2:443"), &[]),
            NatType::Unknown,
            "without P2 this is a guess, and a wrong guess costs the connection"
        );
    }

    #[test]
    fn the_degradations_are_honest_rather_than_confident() {
        let p1 = a("203.0.113.9:51000");
        let local = a("10.0.0.2:443");

        // Nothing at all.
        assert_eq!(classify(None, None, None, local, &[]), NatType::Unknown);
        // P1 alone compares with nothing.
        assert_eq!(classify(Some(p1), None, None, local, &[]), NatType::Unknown);
        // P2 agreed, P3 silent: AddressDependent is still possible.
        assert_eq!(
            classify(Some(p1), Some(p1), None, local, &[]),
            NatType::Unknown
        );
        // P3 alone, agreeing, is enough for endpoint-independence.
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

    /// Find two adjacent free loopback ports on `ip`, so P1 and its alt-port
    /// probe both land on a real responder.
    async fn adjacent_pair(
        ip: &str,
        first: MappingBehaviour,
        second: MappingBehaviour,
    ) -> (StunResponder, StunResponder) {
        let base = {
            let probe = std::net::UdpSocket::bind(format!("{ip}:0")).expect("probe");
            probe.local_addr().expect("addr").port()
        };
        for port in base..base.saturating_add(400) {
            let Ok(a1) = StunResponder::start_on(a(&format!("{ip}:{port}")), first).await else {
                continue;
            };
            if let Ok(a2) = StunResponder::start_on(a(&format!("{ip}:{}", port + 1)), second).await
            {
                return (a1, a2);
            }
        }
        panic!("could not find an adjacent pair of free ports on {ip}");
    }

    /// The full three-probe path, end to end, against local responders only.
    /// P1 and P2 share an IP; P3 is on a genuinely different one.
    #[tokio::test]
    async fn three_truthful_probes_classify_a_direct_path_as_no_nat() {
        let (s1, _s2) = adjacent_pair(
            "127.0.0.1",
            MappingBehaviour::Truthful,
            MappingBehaviour::Truthful,
        )
        .await;
        let s3 = StunResponder::start_on(a("127.0.0.2:0"), MappingBehaviour::Truthful)
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

    /// A server list confined to ONE IP can never supply P3, so it can never
    /// reach a confident answer. Worth asserting: it is the failure mode of a
    /// well-meaning `stun_servers` list that names one host twice.
    #[tokio::test]
    async fn servers_that_share_an_ip_cannot_classify() {
        let s1 = StunResponder::start(MappingBehaviour::Truthful)
            .await
            .expect("s1");
        let s2 = StunResponder::start(MappingBehaviour::Truthful)
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
            "two servers on one IP give no third probe, so nothing is provable"
        );
        assert!(!cands.is_empty(), "discovery still learned our address");
    }

    /// The case that sends us to rung 3. Responders on the same IP reporting
    /// different ports are exactly what a symmetric NAT looks like.
    #[tokio::test]
    async fn a_rewriting_pair_on_one_ip_classifies_as_symmetric() {
        let (s1, _s2) = adjacent_pair(
            "127.0.0.1",
            MappingBehaviour::RewritePort(50_001),
            MappingBehaviour::RewritePort(50_002),
        )
        .await;

        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let cfg = NetConfig {
            stun_servers: vec![s1.server_string()],
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

    /// The distinction three probes exist to make: same port from P2, a
    /// different one from P3 on another IP, is address-dependent — NOT
    /// symmetric, and NOT a reason to fire the birthday blast.
    #[tokio::test]
    async fn a_mapping_keyed_on_destination_ip_alone_is_address_dependent() {
        let (s1, _s2) = adjacent_pair(
            "127.0.0.1",
            MappingBehaviour::RewritePort(60_001),
            // Same reported port: the destination PORT did not move it.
            MappingBehaviour::RewritePort(60_001),
        )
        .await;
        // Different IP, different reported port: the destination IP did.
        let s3 = StunResponder::start_on(a("127.0.0.2:0"), MappingBehaviour::RewritePort(60_002))
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
            NatType::AddressDependent,
            "two probes would have called this Symmetric and wasted rung 3"
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

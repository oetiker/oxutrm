//! Which of our own addresses are worth telling the peer about, and in what
//! order they should be tried.
//!
//! The ordering is load-bearing rather than cosmetic. Rung 0 — direct IPv6 —
//! exists because where both ends have a global IPv6 address there is no NAT
//! to traverse at all, only a stateful firewall pinhole that the outbound
//! punch itself creates. Ranking IPv6 Host highest is what makes that rung win
//! when it is available.

use std::net::{IpAddr, SocketAddr, UdpSocket};

use oxutrm_proto::{Candidate, CandidateKind};

use crate::socketfam::unmap_ip;

/// Type preferences, most preferred first.
///
/// The shape is RFC 8445 §5.1.2 but the values are the design spec's: the spec
/// wants IPv6 Host highest and PeerReflexive lowest, whereas RFC 8445 ranks
/// peer-reflexive above server-reflexive. Both peers run this same function,
/// so the ordering only has to be self-consistent.
const PREF_HOST_V6: u32 = 126;
const PREF_HOST_V4: u32 = 110;
const PREF_PORT_MAPPED: u32 = 100;
const PREF_SERVER_REFLEXIVE: u32 = 90;
const PREF_PEER_REFLEXIVE: u32 = 80;

/// RFC 8445 §5.1.2.1, with one component, so the last term is always 255.
pub fn ice_priority(kind: CandidateKind, ip: &IpAddr) -> u32 {
    let type_pref = match kind {
        CandidateKind::Host => {
            if ip.is_ipv6() {
                PREF_HOST_V6
            } else {
                PREF_HOST_V4
            }
        }
        CandidateKind::PortMapped => PREF_PORT_MAPPED,
        CandidateKind::ServerReflexive => PREF_SERVER_REFLEXIVE,
        CandidateKind::PeerReflexive => PREF_PEER_REFLEXIVE,
    };
    // Within a type, prefer IPv6 — same reason as above, one rung down.
    let local_pref: u32 = if ip.is_ipv6() { 65_535 } else { 32_768 };
    (type_pref << 24) | (local_pref << 8) | 255
}

/// True for `169.254.0.0/16` and `fe80::/10`.
///
/// Link-local addresses are excluded from candidates because they are not
/// routable off the link and, for IPv6, are ambiguous without a scope id that
/// means nothing to the peer.
pub fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Addresses that can never be a useful candidate, whatever the caller wants.
fn is_never_useful(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_unspecified() || v4.is_broadcast() || v4.is_multicast(),
        IpAddr::V6(v6) => v6.is_unspecified() || v6.is_multicast(),
    }
}

/// Local interface addresses as `CandidateKind::Host`.
///
/// Loopback and link-local are excluded: neither can be reached by a peer.
pub fn local_candidates(socket: &UdpSocket) -> Vec<Candidate> {
    local_candidates_filtered(socket, false)
}

/// The same, with a door for tests.
///
/// Production callers use [`local_candidates`]. A loopback candidate is
/// useless in the field, but it is the only address a test on one machine can
/// actually punch to.
pub fn local_candidates_filtered(socket: &UdpSocket, include_loopback: bool) -> Vec<Candidate> {
    let port = match socket.local_addr() {
        Ok(a) => a.port(),
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<Candidate> = Vec::new();
    for iface in netdev::get_interfaces() {
        let ips = iface
            .ipv6
            .iter()
            .map(|n| IpAddr::V6(n.addr()))
            .chain(iface.ipv4.iter().map(|n| IpAddr::V4(n.addr())));

        for ip in ips {
            let ip = unmap_ip(ip);
            if is_never_useful(&ip) || is_link_local(&ip) {
                continue;
            }
            if ip.is_loopback() && !include_loopback {
                continue;
            }
            out.push(Candidate {
                addr: SocketAddr::new(ip, port),
                kind: CandidateKind::Host,
                priority: ice_priority(CandidateKind::Host, &ip),
            });
        }
    }

    // Same address on two interfaces is one candidate.
    out.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.addr.cmp(&b.addr)));
    out.dedup_by(|a, b| a.addr == b.addr);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// Rung 0 is the highest-value rung precisely because global IPv6 has no
    /// NAT to traverse. If this ordering ever inverts, the ladder stops
    /// preferring the rung that works best.
    #[test]
    fn an_ipv6_host_candidate_outranks_every_other_kind() {
        let v6 = ice_priority(CandidateKind::Host, &ip("2001:db8::1"));
        for (kind, addr) in [
            (CandidateKind::Host, "198.51.100.1"),
            (CandidateKind::PortMapped, "198.51.100.1"),
            (CandidateKind::ServerReflexive, "198.51.100.1"),
            (CandidateKind::PeerReflexive, "198.51.100.1"),
        ] {
            assert!(
                v6 > ice_priority(kind, &ip(addr)),
                "IPv6 Host did not outrank {kind:?}"
            );
        }
    }

    /// The spec's order, which departs from RFC 8445 in ranking
    /// peer-reflexive last.
    #[test]
    fn the_kinds_rank_in_the_order_the_spec_asks_for() {
        let a = ip("198.51.100.1");
        let host = ice_priority(CandidateKind::Host, &a);
        let mapped = ice_priority(CandidateKind::PortMapped, &a);
        let srflx = ice_priority(CandidateKind::ServerReflexive, &a);
        let prflx = ice_priority(CandidateKind::PeerReflexive, &a);
        assert!(host > mapped, "host must outrank a router mapping");
        assert!(
            mapped > srflx,
            "a router mapping is exact; a reflexive one is observed"
        );
        assert!(srflx > prflx, "the spec ranks peer-reflexive lowest");
    }

    #[test]
    fn ipv6_outranks_ipv4_within_the_same_kind() {
        for kind in [
            CandidateKind::Host,
            CandidateKind::PortMapped,
            CandidateKind::ServerReflexive,
            CandidateKind::PeerReflexive,
        ] {
            assert!(
                ice_priority(kind, &ip("2001:db8::1")) > ice_priority(kind, &ip("198.51.100.1")),
                "IPv4 outranked IPv6 for {kind:?}"
            );
        }
    }

    /// RFC 8445 §5.1.2: priorities are 32-bit and must not collide across
    /// types, or the check ordering becomes arbitrary.
    #[test]
    fn priorities_fit_in_32_bits_and_never_collide_across_kinds() {
        let mut seen = std::collections::HashSet::new();
        for kind in [
            CandidateKind::Host,
            CandidateKind::PortMapped,
            CandidateKind::ServerReflexive,
            CandidateKind::PeerReflexive,
        ] {
            for addr in ["2001:db8::1", "198.51.100.1"] {
                let p = ice_priority(kind, &ip(addr));
                assert!(p > 0);
                assert!(seen.insert(p), "duplicate priority {p} for {kind:?} {addr}");
            }
        }
    }

    #[test]
    fn link_local_is_recognised_in_both_families() {
        assert!(is_link_local(&ip("169.254.1.1")));
        assert!(is_link_local(&ip("fe80::1")));
        assert!(is_link_local(&ip("febf::1")), "fe80::/10 runs to febf");
        assert!(!is_link_local(&ip("169.253.1.1")));
        assert!(!is_link_local(&ip("2001:db8::1")));
        assert!(!is_link_local(&ip("10.0.0.1")));
        assert!(
            !is_link_local(&ip("fec0::1")),
            "site-local is not link-local"
        );
    }

    #[test]
    fn unroutable_addresses_are_never_candidates() {
        for a in ["0.0.0.0", "255.255.255.255", "224.0.0.1", "::", "ff02::1"] {
            assert!(is_never_useful(&ip(a)), "{a} should never be a candidate");
        }
        for a in ["10.0.0.1", "198.51.100.1", "2001:db8::1"] {
            assert!(!is_never_useful(&ip(a)), "{a} is a usable candidate");
        }
    }

    #[test]
    fn candidates_carry_the_sockets_own_port() {
        let sock = UdpSocket::bind("0.0.0.0:0").expect("bind");
        let port = sock.local_addr().unwrap().port();
        for c in local_candidates_filtered(&sock, true) {
            assert_eq!(c.addr.port(), port, "a candidate must name our own port");
            assert_eq!(c.kind, CandidateKind::Host);
        }
    }

    #[test]
    fn production_candidates_exclude_loopback_and_link_local() {
        let sock = UdpSocket::bind("0.0.0.0:0").expect("bind");
        for c in local_candidates(&sock) {
            assert!(!c.addr.ip().is_loopback(), "loopback leaked: {}", c.addr);
            assert!(
                !is_link_local(&c.addr.ip()),
                "link-local leaked: {}",
                c.addr
            );
        }
    }

    /// The test door: on a machine with no external addresses at all — a
    /// sandboxed CI runner — loopback is the only thing to punch to.
    #[test]
    fn the_test_door_admits_loopback_and_the_default_does_not() {
        let sock = UdpSocket::bind("0.0.0.0:0").expect("bind");
        let with = local_candidates_filtered(&sock, true);
        let without = local_candidates_filtered(&sock, false);
        assert!(with.len() >= without.len());
        assert!(
            with.iter().any(|c| c.addr.ip().is_loopback()),
            "every host has a loopback address"
        );
        assert!(!without.iter().any(|c| c.addr.ip().is_loopback()));
    }

    #[test]
    fn candidates_are_deduplicated_and_sorted_best_first() {
        let sock = UdpSocket::bind("0.0.0.0:0").expect("bind");
        let cands = local_candidates_filtered(&sock, true);

        let addrs: std::collections::HashSet<_> = cands.iter().map(|c| c.addr).collect();
        assert_eq!(addrs.len(), cands.len(), "duplicate candidate addresses");

        for pair in cands.windows(2) {
            assert!(
                pair[0].priority >= pair[1].priority,
                "candidates are not in descending priority order"
            );
        }
    }

    /// The whole point of the ordering, end to end: if this host has a global
    /// IPv6 address, it must be offered before any IPv4 one.
    #[test]
    fn a_global_ipv6_candidate_is_offered_before_any_ipv4_candidate() {
        let sock = UdpSocket::bind("0.0.0.0:0").expect("bind");
        let cands = local_candidates_filtered(&sock, true);
        let first_v4 = cands.iter().position(|c| c.addr.is_ipv4());
        let last_v6 = cands.iter().rposition(|c| c.addr.is_ipv6());
        if let (Some(v4), Some(v6)) = (first_v4, last_v6) {
            assert!(v6 < v4, "an IPv4 candidate was offered before an IPv6 one");
        }
    }
}

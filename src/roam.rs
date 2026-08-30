//! Which local address this machine would use to reach the host, and whether
//! it changed.
//!
//! QUIC identifies a connection by connection IDs rather than by addresses, so
//! a client that changes its own local address is survivable by design --
//! `Link::rebind` performs the swap. What was missing was anything that
//! noticed the change, and this is it.
//!
//! **The trigger is evidence, not a platform API.** Bind a throwaway UDP
//! socket, `connect` it to the peer, and read its `local_addr()`. `connect` on
//! a UDP socket sends nothing: it fixes a default destination and runs the
//! route lookup, and `getsockname` then reports the source address the kernel
//! chose. So this asks the routing table the only question that matters --
//! "what would you do for THIS peer, right now" -- without a packet, without
//! netlink, without route sockets and without a `cfg`. Both platforms walk the
//! same code, which is the rule `FD_DIRS`, `open_keyboard` and
//! `second_loopback_ip` already follow.
//!
//! # What it is compared against, and what it must not be
//!
//! Not the session socket. `oxutrm_net::bind_socket` binds wildcard --
//! dual-stack `[::]` where it can -- so the session socket's `local_addr()` is
//! `[::]:port` and says nothing about any route. Measured on the dev Mac:
//! the session socket reports `('::', 54214)` while a probe to a LAN peer
//! reports `('::ffff:192.168.17.5', 57050)`. Those can never be equal, so a
//! check against the held socket -- which the design spec's 4.2 describes --
//! would call every healthy link a moved route and rebind, and a rebind moves
//! our source port and invalidates a punched NAT hole. It would break the path
//! it exists to repair, on every probe.
//!
//! So [`RouteWatch`] compares a probe against the PREVIOUS probe: a baseline
//! taken while the link was known good, replaced whenever a rebind succeeds.
//!
//! **The IP only, never the port.** The throwaway socket's ephemeral port is
//! its own; two probes seconds apart on an unchanged machine gave 49974 and
//! 57050. Comparing ports would make every probe a route change.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

/// How often the route is probed while `Silent`.
///
/// The client's loop wakes every `pacing_interval` -- 8 ms to 100 ms -- so a
/// probe on every lap would be up to 125 bind/connect pairs a second. Once a
/// second is far faster than any human notices and slower than anything that
/// could matter.
///
/// It is a floor on probing while `Silent` and nothing else. A healthy session
/// never reaches it: probing runs only in `Silent`, so a working link makes no
/// probe syscalls at all. This is emphatically not `IDLE_POLL` coming back as
/// a pace.
///
/// **No caller yet: wiring the probe into the session loop is the next
/// task's job**, same as `Link::rebind` before it.
#[allow(dead_code)]
pub const ROUTE_PROBE_EVERY: Duration = Duration::from_secs(1);

/// The local address the kernel would use to reach `peer`, right now.
///
/// Sends nothing. Fails when the peer is unroutable, which is an ordinary
/// thing for a machine in the middle of the outage this runs during -- the
/// caller must treat an error as "no answer this time" and never as fatal.
///
/// **No caller yet**: see [`ROUTE_PROBE_EVERY`].
#[allow(dead_code)]
pub fn route_source(peer: SocketAddr) -> std::io::Result<IpAddr> {
    // Unmapped first, so the throwaway socket can be bound in the peer's own
    // family and `connect` needs no mapping of its own. `oxutrm_net`'s own doc
    // warns that getting this wrong "produces a socket that silently talks to
    // nobody"; here it would produce EINVAL instead, which is at least loud.
    let peer = oxutrm_net::unmap(peer);
    let bind: SocketAddr = match peer {
        SocketAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], 0)),
        SocketAddr::V6(_) => SocketAddr::from(([0u16; 8], 0)),
    };

    let probe = UdpSocket::bind(bind)?;
    probe.connect(peer)?;
    Ok(oxutrm_net::unmap_ip(probe.local_addr()?.ip()))
}

/// The source address that was true when the link last worked.
///
/// **No caller yet**: see [`ROUTE_PROBE_EVERY`].
#[allow(dead_code)]
pub struct RouteWatch {
    baseline: Option<IpAddr>,
}

#[allow(dead_code)]
impl RouteWatch {
    /// `baseline` is what a probe said while the link was known good. `None`
    /// when no probe has succeeded yet.
    pub fn new(baseline: Option<IpAddr>) -> RouteWatch {
        RouteWatch {
            baseline: baseline
                .map(Self::normalise)
                .filter(|ip| !ip.is_unspecified()),
        }
    }

    /// Whether `seen` says this machine's route to the peer has moved.
    ///
    /// False with no baseline: a rebind on no evidence is strictly worse than
    /// doing nothing, because it costs a punched NAT hole to learn nothing.
    pub fn moved(&self, seen: IpAddr) -> bool {
        let seen = Self::normalise(seen);
        if seen.is_unspecified() {
            return false;
        }
        self.baseline.is_some_and(|base| base != seen)
    }

    /// Adopt `seen` as the address the link now works from.
    ///
    /// Called after a successful rebind, so one route change asks for one
    /// rebind rather than one per probe for the rest of the session.
    pub fn settle(&mut self, seen: IpAddr) {
        let seen = Self::normalise(seen);
        if !seen.is_unspecified() {
            self.baseline = Some(seen);
        }
    }

    /// v4-mapped v6 and plain v4 are the same address. The session socket is
    /// dual-stack, so a baseline can be learned in one form and a probe read
    /// back in the other; treating them as different would rebind for ever.
    fn normalise(ip: IpAddr) -> IpAddr {
        oxutrm_net::unmap_ip(ip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("a literal address")
    }

    /// The whole point: a baseline taken while the link worked, compared with
    /// what the kernel would do now.
    #[test]
    fn a_changed_source_address_is_a_moved_route() {
        let w = RouteWatch::new(Some(ip("192.168.17.5")));
        assert!(w.moved(ip("10.46.18.101")));
        assert!(!w.moved(ip("192.168.17.5")));
    }

    /// Guards the defect in spec 4.2. The session socket is bound wildcard, so
    /// comparing against ITS local_addr would compare `::` with a real source
    /// address and call every healthy link a moved route -- then rebind, which
    /// invalidates a punched NAT hole. The unspecified address is not a
    /// baseline and must never be treated as one.
    #[test]
    fn the_wildcard_address_is_not_a_baseline() {
        let w = RouteWatch::new(Some(ip("::")));
        assert!(
            !w.moved(ip("192.168.17.5")),
            "`::` was treated as a real source address; every probe would rebind"
        );
    }

    /// With nothing known yet there is nothing to compare against, and a
    /// rebind on no evidence is strictly worse than doing nothing.
    #[test]
    fn no_baseline_is_not_a_moved_route() {
        let w = RouteWatch::new(None);
        assert!(!w.moved(ip("192.168.17.5")));
    }

    /// A rebind is once per actual change, not once per probe. After settling
    /// on the new address the same reading must stop asking to move.
    #[test]
    fn settling_stops_the_same_change_asking_twice() {
        let mut w = RouteWatch::new(Some(ip("192.168.17.5")));
        assert!(w.moved(ip("10.46.18.101")));
        w.settle(ip("10.46.18.101"));
        assert!(
            !w.moved(ip("10.46.18.101")),
            "the same route change asked for a second rebind"
        );
    }

    /// v4-mapped v6 and plain v4 are the same address. The session socket is
    /// dual-stack, so a baseline can be learned in one form and a probe read
    /// back in the other; treating them as different would rebind for ever.
    #[test]
    fn a_mapped_address_equals_its_plain_form() {
        let w = RouteWatch::new(Some(ip("::ffff:192.168.17.5")));
        assert!(
            !w.moved(IpAddr::V4(Ipv4Addr::new(192, 168, 17, 5))),
            "a v4-mapped baseline did not match its own plain v4 probe"
        );
    }

    /// Against a real kernel: the source address for a peer is a concrete
    /// address, never the wildcard the session socket reports. This is the
    /// measurement the whole trigger rests on, so it is asserted rather than
    /// assumed. Loopback, so it needs no network.
    #[test]
    fn probing_a_peer_yields_a_concrete_source_address() {
        let peer = "127.0.0.1:9".parse().expect("a literal address");
        let seen = route_source(peer).expect("loopback is always routable");

        assert!(
            !seen.is_unspecified(),
            "the probe returned the wildcard: {seen}"
        );
        assert_eq!(
            seen,
            ip("127.0.0.1"),
            "the route to loopback is not loopback"
        );
    }

    /// A probe that cannot answer must be silent, not fatal. An unroutable
    /// peer makes `connect` fail with ENETUNREACH, and that is a normal thing
    /// for a machine mid-outage -- which is exactly when this code runs.
    #[test]
    fn an_unroutable_peer_is_an_error_and_not_a_panic() {
        // Documentation range, guaranteed never routed anywhere real.
        let peer = "192.0.2.1:9".parse().expect("a literal address");
        // Either answer is legitimate: some stacks route it to a default
        // gateway and some refuse. Neither may panic, and neither may return
        // the wildcard.
        if let Ok(seen) = route_source(peer) {
            assert!(
                !seen.is_unspecified(),
                "the probe returned the wildcard: {seen}"
            );
        }
    }
}

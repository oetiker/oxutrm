//! Binding the one socket, and keeping IPv4-mapped addresses straight.
//!
//! The whole crate revolves around a single UDP socket: bound once, used for
//! STUN discovery, used again for ICE checks, and finally handed to `quinn`.
//! NAT mappings are per-socket, so an address learned on any other socket
//! describes nothing useful.
//!
//! Because that socket is dual-stack where possible, IPv4 peers appear — and
//! must be addressed — in their IPv4-mapped form (`::ffff:198.51.100.7`).
//! `send_to` on a `[::]` socket fails with `EINVAL` for a plain IPv4 address.
//! Every send goes through [`to_socket_family`] and every source address read
//! off the socket goes through [`unmap`]; forgetting either produces a socket
//! that silently talks to nobody.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};

use anyhow::Context;

use crate::NetConfig;

/// Bind the session socket, preferring UDP/443, dual-stack where possible.
///
/// Four attempts, in order, and the first that succeeds wins:
///
/// 1. dual-stack `[::]:443`
/// 2. IPv4 `0.0.0.0:443`
/// 3. dual-stack `[::]:0`
/// 4. IPv4 `0.0.0.0:0`
///
/// Binding 443 requires privilege on most systems, so falling through to a
/// high port is the **ordinary** path rather than an error path. A high port
/// costs the host nothing: the peer learns the real port from the candidate
/// exchange, and 443 is only ever an advantage against middleboxes that
/// classify by port.
pub fn bind_socket(cfg: &NetConfig) -> anyhow::Result<UdpSocket> {
    for port in [cfg.prefer_port, 0] {
        // Dual-stack first: one socket that can reach both families is what
        // lets rung 0 and rung 2 share a single NAT mapping.
        if let Ok(sock) = bind_dual_stack(port) {
            return Ok(sock);
        }
        // A host with IPv6 disabled entirely, which is rarer than it used to
        // be but has not gone away.
        if let Ok(sock) = UdpSocket::bind(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            port,
        ))) {
            return Ok(sock);
        }
    }

    // Nothing worked. Repeat the most permissive attempt rather than reporting
    // a stale error from an earlier one: whatever fails here is the reason
    // this host cannot bind a UDP socket at all, which is worth saying
    // accurately.
    UdpSocket::bind(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))
        .context("could not bind a UDP socket on any port or family")
}

/// A socket bound to `[::]` with `IPV6_V6ONLY` off, so it accepts IPv4 too.
fn bind_dual_stack(port: u16) -> std::io::Result<UdpSocket> {
    let addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0);
    let sock = std::net::UdpSocket::bind(SocketAddr::V6(addr))?;
    set_v6only(&sock, false)?;
    Ok(sock)
}

/// `IPV6_V6ONLY`. Not in `std`, and `rustix` exposes it directly.
fn set_v6only(sock: &UdpSocket, on: bool) -> std::io::Result<()> {
    use std::os::fd::AsFd;
    rustix::net::sockopt::set_ipv6_v6only(sock.as_fd(), on).map_err(std::io::Error::from)
}

/// Rewrite `peer` into the form `local`'s socket family can actually send to.
///
/// A dual-stack socket needs IPv4 peers mapped into IPv6; an IPv4 socket
/// cannot address IPv6 at all and the peer is returned unchanged, which lets
/// the caller fail at `send_to` with a real error rather than here with a
/// guess.
pub fn to_socket_family(local: &SocketAddr, peer: SocketAddr) -> SocketAddr {
    match (local, peer) {
        (SocketAddr::V6(_), SocketAddr::V4(v4)) => {
            SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
        }
        _ => peer,
    }
}

/// Undo IPv4-mapping, so a candidate carries the address a peer would dial.
///
/// A candidate advertising `[::ffff:198.51.100.7]:443` is useless to a peer on
/// an IPv4-only host, and it is the same address anyway.
pub fn unmap(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, v6.port())),
            None => addr,
        },
        SocketAddr::V4(_) => addr,
    }
}

/// The same, for a bare address.
pub fn unmap_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => ip,
        },
        IpAddr::V4(_) => ip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_succeeds_and_yields_a_usable_socket() {
        let sock = bind_socket(&NetConfig::default()).expect("bind");
        let local = sock.local_addr().expect("local_addr");
        assert_ne!(local.port(), 0, "a bound socket has a real port");
    }

    /// The important one: `bind_socket` MUST come back with a usable socket
    /// whether or not the preferred port was available, and any fallback it
    /// takes must be to a port it could actually have bound. On a developer
    /// machine and in CI the fallback is the NORMAL path, not an error path.
    #[test]
    fn a_privileged_port_falls_back_to_a_high_port() {
        let cfg = NetConfig::default();
        let sock = bind_socket(&cfg).expect("bind must succeed even without privilege");
        let port = sock.local_addr().expect("local_addr").port();
        assert_ne!(port, 0, "a bound socket has a real port");

        // Judged on the OUTCOME, and deliberately not on a prior question
        // about privilege.
        //
        // This asserted "an unprivileged process cannot have bound 443", which
        // is not true everywhere: macOS hands UDP and TCP 443 to an ordinary
        // user, measured at euid 501, so a correct bind failed the test there.
        // Probing "can I bind 443?" first is worse - the probe is a different
        // moment from the bind, and the sibling tests in this module bind too,
        // so the answer goes stale in between. That version failed about one
        // run in four, which is a race and not flakiness.
        if port == cfg.prefer_port {
            // The preferred port itself, legitimate wherever the platform
            // allows it. No fallback was required, so there is none to judge.
            return;
        }
        // Something else, so a fallback happened: it must be a port that could
        // have been bound without privilege in the first place.
        assert!(
            port >= 1024,
            "fell back to another privileged port, which cannot have worked: {port}"
        );
    }

    /// A preferred port that is definitely unavailable, so the fallback is
    /// forced regardless of privilege — including for root, where the test
    /// above cannot conclude anything.
    #[test]
    fn an_unbindable_preferred_port_falls_back_rather_than_failing() {
        // Take a port on both families, then ask bind_socket for it: neither
        // of the first two attempts can succeed.
        let squat_v4 = UdpSocket::bind("0.0.0.0:0").expect("squat v4");
        let taken = squat_v4.local_addr().expect("local_addr").port();
        let squat_v6 = UdpSocket::bind(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::UNSPECIFIED,
            taken,
            0,
            0,
        )))
        .ok();

        let cfg = NetConfig {
            prefer_port: taken,
            ..NetConfig::default()
        };
        let sock = bind_socket(&cfg).expect("must fall back rather than fail");
        let got = sock.local_addr().expect("local_addr").port();
        assert_ne!(got, 0, "the fallback must yield a real port");
        assert_ne!(got, taken, "the fallback did not actually happen");

        drop(squat_v4);
        drop(squat_v6);
        drop(sock);
    }

    #[test]
    fn a_dual_stack_socket_accepts_both_families() {
        let sock = match bind_dual_stack(0) {
            Ok(s) => s,
            // A host with IPv6 switched off entirely; the fallback covers it
            // and there is nothing to assert here.
            Err(_) => return,
        };
        let local = sock.local_addr().expect("local_addr");
        assert!(local.is_ipv6(), "dual-stack sockets are bound as IPv6");

        use std::os::fd::AsFd;
        let v6only = rustix::net::sockopt::ipv6_v6only(sock.as_fd()).expect("read IPV6_V6ONLY");
        assert!(!v6only, "the socket refuses IPv4 and is not dual-stack");
    }

    #[test]
    fn an_ipv4_peer_is_mapped_for_a_dual_stack_socket() {
        let local: SocketAddr = "[::]:443".parse().unwrap();
        let peer: SocketAddr = "198.51.100.7:1234".parse().unwrap();
        let sent = to_socket_family(&local, peer);
        assert_eq!(
            sent,
            "[::ffff:198.51.100.7]:1234".parse::<SocketAddr>().unwrap()
        );
        assert!(
            sent.is_ipv6(),
            "send_to on a [::] socket needs an IPv6 form"
        );
    }

    #[test]
    fn an_ipv6_peer_and_an_ipv4_socket_are_both_left_alone() {
        let v6local: SocketAddr = "[::]:443".parse().unwrap();
        let v6peer: SocketAddr = "[2001:db8::2]:443".parse().unwrap();
        assert_eq!(to_socket_family(&v6local, v6peer), v6peer);

        let v4local: SocketAddr = "0.0.0.0:443".parse().unwrap();
        let v4peer: SocketAddr = "198.51.100.7:1234".parse().unwrap();
        assert_eq!(to_socket_family(&v4local, v4peer), v4peer);
    }

    #[test]
    fn unmapping_recovers_the_address_a_peer_would_dial() {
        let mapped: SocketAddr = "[::ffff:198.51.100.7]:1234".parse().unwrap();
        assert_eq!(
            unmap(mapped),
            "198.51.100.7:1234".parse::<SocketAddr>().unwrap()
        );

        // A genuine IPv6 address is not a mapped one and must survive intact.
        let real: SocketAddr = "[2001:db8::2]:443".parse().unwrap();
        assert_eq!(unmap(real), real);
        let v4: SocketAddr = "198.51.100.7:1234".parse().unwrap();
        assert_eq!(unmap(v4), v4);
    }

    #[test]
    fn mapping_and_unmapping_round_trip() {
        let local: SocketAddr = "[::]:0".parse().unwrap();
        for peer in ["198.51.100.7:1234", "0.0.0.0:1", "255.255.255.255:65535"] {
            let peer: SocketAddr = peer.parse().unwrap();
            assert_eq!(unmap(to_socket_family(&local, peer)), peer);
        }
    }

    #[test]
    fn unmap_ip_matches_unmap() {
        assert_eq!(
            unmap_ip("::ffff:10.0.0.1".parse().unwrap()),
            "10.0.0.1".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            unmap_ip("2001:db8::1".parse().unwrap()),
            "2001:db8::1".parse::<IpAddr>().unwrap()
        );
    }
}

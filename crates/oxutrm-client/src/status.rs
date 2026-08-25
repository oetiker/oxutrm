//! The one line printed on connect, and then silence.
//!
//! Spec §10.3. The point is that oxutrm never does anything clever silently:
//! the user is told which rung of the ladder actually won, how far away the
//! host is, and — when it matters — what the connection cannot do.

use oxutrm_proto::{NatType, PathDescription, Rung};
use std::net::SocketAddr;

fn family(a: &SocketAddr) -> &'static str {
    if a.is_ipv6() { "IPv6" } else { "IPv4" }
}

fn nat_label(n: NatType) -> &'static str {
    match n {
        NatType::None => "none",
        NatType::EndpointIndependent => "endpoint-independent",
        NatType::AddressDependent => "address-dependent",
        NatType::Symmetric => "symmetric",
        NatType::Unknown => "unknown",
    }
}

/// How the path is described to the user, in one phrase.
///
/// `PathDescription` records that a router mapping won, but not whether it was
/// NAT-PMP, PCP or UPnP-IGD, so the phrase says "port mapped" rather than
/// naming a protocol it cannot actually distinguish.
pub fn rung_label(path: &PathDescription) -> String {
    match path.rung {
        Rung::Ipv6Direct => "IPv6 direct".to_string(),
        Rung::PortMapped => format!("{} punched (port mapped)", family(&path.remote)),
        Rung::StunPunch => format!("{} punched", family(&path.remote)),
        Rung::Birthday => format!(
            "{} punched (birthday, {} probes)",
            family(&path.remote),
            path.probes_sent
        ),
        Rung::SshTunnel => "SSH tunnel".to_string(),
    }
}

/// The single connect-time line.
pub fn status_line(path: &PathDescription) -> String {
    match path.rung {
        // Rung 4 runs its transport inside the SSH connection, so it can never
        // daemonize. Naming only "SSH tunnel" would leave the user to discover
        // that by closing their laptop lid and losing the session.
        Rung::SshTunnel => format!(
            "oxutrm  SSH tunnel — no UDP path, not detachable  \u{b7}  {} ms      [warning]",
            path.rtt_ms
        ),
        // Rung 3 was entered *because* the NAT is symmetric, so saying so
        // explains the probe count better than an MTU would.
        Rung::Birthday => format!(
            "oxutrm  {}  \u{b7}  {} ms  \u{b7}  {} NAT",
            rung_label(path),
            path.rtt_ms,
            nat_label(path.nat_type)
        ),
        _ => format!(
            "oxutrm  {}  \u{b7}  {} ms  \u{b7}  mtu {}",
            rung_label(path),
            path.rtt_ms,
            path.mtu
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(
        rung: Rung,
        remote: &str,
        rtt_ms: u32,
        mtu: u16,
        probes: u32,
        nat: NatType,
    ) -> PathDescription {
        PathDescription {
            rung,
            local: "127.0.0.1:443".parse().unwrap(),
            remote: remote.parse().unwrap(),
            probes_sent: probes,
            nat_type: nat,
            rtt_ms,
            mtu,
        }
    }

    #[test]
    fn the_ipv6_direct_line_is_exact() {
        let p = path(
            Rung::Ipv6Direct,
            "[2001:db8::2]:443",
            11,
            1452,
            0,
            NatType::None,
        );
        assert_eq!(
            status_line(&p),
            "oxutrm  IPv6 direct  \u{b7}  11 ms  \u{b7}  mtu 1452"
        );
    }

    #[test]
    fn the_port_mapped_line_names_the_address_family() {
        let p = path(
            Rung::PortMapped,
            "203.0.113.7:443",
            38,
            1392,
            0,
            NatType::EndpointIndependent,
        );
        assert_eq!(
            status_line(&p),
            "oxutrm  IPv4 punched (port mapped)  \u{b7}  38 ms  \u{b7}  mtu 1392"
        );
    }

    #[test]
    fn the_plain_punch_line_distinguishes_v4_from_v6() {
        let v4 = path(
            Rung::StunPunch,
            "203.0.113.7:443",
            22,
            1400,
            6,
            NatType::EndpointIndependent,
        );
        assert_eq!(
            status_line(&v4),
            "oxutrm  IPv4 punched  \u{b7}  22 ms  \u{b7}  mtu 1400"
        );
        let v6 = path(
            Rung::StunPunch,
            "[2001:db8::2]:443",
            22,
            1400,
            6,
            NatType::EndpointIndependent,
        );
        assert_eq!(
            status_line(&v6),
            "oxutrm  IPv6 punched  \u{b7}  22 ms  \u{b7}  mtu 1400"
        );
    }

    #[test]
    fn the_birthday_line_reports_probes_and_the_nat_rather_than_the_mtu() {
        let p = path(
            Rung::Birthday,
            "203.0.113.7:41234",
            61,
            1392,
            312,
            NatType::Symmetric,
        );
        assert_eq!(
            status_line(&p),
            "oxutrm  IPv4 punched (birthday, 312 probes)  \u{b7}  61 ms  \u{b7}  symmetric NAT"
        );
    }

    /// The rung-4 line must name what the session cannot do. A user who is not
    /// told "not detachable" finds out by closing the lid.
    #[test]
    fn the_ssh_tunnel_line_warns_and_says_it_is_not_detachable() {
        let p = path(
            Rung::SshTunnel,
            "127.0.0.1:41234",
            45,
            1200,
            0,
            NatType::Unknown,
        );
        assert_eq!(
            status_line(&p),
            "oxutrm  SSH tunnel — no UDP path, not detachable  \u{b7}  45 ms      [warning]"
        );
        assert!(status_line(&p).contains("not detachable"));
        assert!(status_line(&p).contains("[warning]"));
    }

    #[test]
    fn every_line_is_a_single_line_beginning_with_the_product_name() {
        for rung in [
            Rung::Ipv6Direct,
            Rung::PortMapped,
            Rung::StunPunch,
            Rung::Birthday,
            Rung::SshTunnel,
        ] {
            let p = path(rung, "203.0.113.7:443", 5, 1400, 1, NatType::Unknown);
            let line = status_line(&p);
            assert!(line.starts_with("oxutrm  "), "{line:?}");
            assert!(
                !line.contains('\n'),
                "the connect line must be one line: {line:?}"
            );
        }
    }
}

//! Types shared by the signalling channel, the datagram framing and the
//! stream messages.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

/// How a candidate address was learned (ICE terminology).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum CandidateKind {
    Host,
    PortMapped,
    ServerReflexive,
    PeerReflexive,
}

/// What comparing the mapped port from two different STUN servers revealed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum NatType {
    None,
    EndpointIndependent,
    AddressDependent,
    Symmetric,
    Unknown,
}

/// Which rung of the ladder produced the path in use.
///
/// `SshTunnel` is not merely slower: a session on it runs its transport inside
/// the SSH connection, so it can never daemonize and can never be reattached.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Rung {
    Ipv6Direct,
    PortMapped,
    StunPunch,
    Birthday,
    SshTunnel,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub addr: std::net::SocketAddr,
    pub kind: CandidateKind,
    /// ICE-style: IPv6 Host highest, PeerReflexive lowest.
    pub priority: u32,
}

/// What the client's own terminal can render.
///
/// These never reach the child shell's environment: `TERM` is derived solely
/// from what the emulator supports, because a `TERM` narrowed to today's
/// client would bake degraded output into the authoritative screen state
/// forever (spec §9.4). Capabilities steer the client's own down-conversion,
/// and are carried for diagnosis.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalCaps {
    pub truecolor: bool,
    /// 8, 16, 256 or 16_777_216 — which is why this is not a `u16`.
    pub colors: u32,
    pub bracketed_paste: bool,
    pub mouse_sgr: bool,
    pub osc52: bool,
    /// The client's own `$TERM`, for diagnosis only. Never propagated.
    pub term_name: String,
}

/// The path the ladder settled on, as shown in the status line.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathDescription {
    pub rung: Rung,
    pub local: std::net::SocketAddr,
    pub remote: std::net::SocketAddr,
    pub probes_sent: u32,
    pub nat_type: NatType,
    pub rtt_ms: u32,
    pub mtu: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_size_round_trips_through_json() {
        let s = TermSize {
            cols: 132,
            rows: 43,
        };
        let text = serde_json::to_string(&s).expect("encode");
        assert_eq!(text, r#"{"cols":132,"rows":43}"#);
        assert_eq!(serde_json::from_str::<TermSize>(&text).expect("decode"), s);
    }

    #[test]
    fn the_enums_are_named_on_the_wire_not_numbered() {
        // Renaming a variant is a wire break; numbering them would make a
        // reorder one too. Both are pinned here on purpose.
        assert_eq!(
            serde_json::to_string(&CandidateKind::Host).unwrap(),
            r#""Host""#
        );
        assert_eq!(
            serde_json::to_string(&CandidateKind::ServerReflexive).unwrap(),
            r#""ServerReflexive""#
        );
        assert_eq!(
            serde_json::to_string(&NatType::EndpointIndependent).unwrap(),
            r#""EndpointIndependent""#
        );
        assert_eq!(serde_json::to_string(&NatType::None).unwrap(), r#""None""#);
        assert_eq!(
            serde_json::to_string(&Rung::Ipv6Direct).unwrap(),
            r#""Ipv6Direct""#
        );
        assert_eq!(
            serde_json::to_string(&Rung::SshTunnel).unwrap(),
            r#""SshTunnel""#
        );
    }

    #[test]
    fn a_candidate_carries_an_address_a_kind_and_a_priority() {
        let c = Candidate {
            addr: "192.0.2.7:443".parse().expect("v4 address"),
            kind: CandidateKind::PortMapped,
            priority: 1_000,
        };
        let text = serde_json::to_string(&c).expect("encode");
        let back: Candidate = serde_json::from_str(&text).expect("decode");
        assert_eq!(back.addr, c.addr);
        assert_eq!(back.kind, c.kind);
        assert_eq!(back.priority, c.priority);
    }

    #[test]
    fn an_ipv6_candidate_survives_the_round_trip() {
        // Rung 0 is the whole point of the ladder, so v6 addresses must not be
        // mangled by the JSON representation.
        let c = Candidate {
            addr: "[2001:db8::1]:443".parse().expect("v6 address"),
            kind: CandidateKind::Host,
            priority: u32::MAX,
        };
        let back: Candidate =
            serde_json::from_str(&serde_json::to_string(&c).expect("encode")).expect("decode");
        assert_eq!(back.addr, c.addr);
        assert!(back.addr.is_ipv6());
    }

    #[test]
    fn terminal_caps_can_hold_a_truecolor_count() {
        // 16_777_216 does not fit in a u16; the field is u32 for this reason.
        let caps = TerminalCaps {
            truecolor: true,
            colors: 16_777_216,
            bracketed_paste: true,
            mouse_sgr: true,
            osc52: true,
            term_name: "xterm-256color".to_string(),
        };
        let back: TerminalCaps =
            serde_json::from_str(&serde_json::to_string(&caps).expect("encode")).expect("decode");
        assert_eq!(back.colors, 16_777_216);
        assert_eq!(back.term_name, "xterm-256color");
        assert!(back.truecolor && back.bracketed_paste && back.mouse_sgr && back.osc52);
    }

    #[test]
    fn a_path_description_round_trips() {
        let p = PathDescription {
            rung: Rung::Birthday,
            local: "[2001:db8::1]:443".parse().expect("local"),
            remote: "198.51.100.9:60001".parse().expect("remote"),
            probes_sent: 312,
            nat_type: NatType::Symmetric,
            rtt_ms: 61,
            mtu: 1392,
        };
        let back: PathDescription =
            serde_json::from_str(&serde_json::to_string(&p).expect("encode")).expect("decode");
        assert_eq!(back.rung, p.rung);
        assert_eq!(back.local, p.local);
        assert_eq!(back.remote, p.remote);
        assert_eq!(back.probes_sent, 312);
        assert_eq!(back.nat_type, NatType::Symmetric);
        assert_eq!(back.rtt_ms, 61);
        assert_eq!(back.mtu, 1392);
    }
}

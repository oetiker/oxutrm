//! Types shared by the signalling channel, the datagram framing and the
//! stream messages.

use serde::{Deserialize, Serialize};

/// The largest screen oxutrm will hold — the ceiling behind **I7**.
///
/// `rows` and `cols` are `u16`, so the wire permits 65535x65535 = 4.29e9 cells.
/// A `Cell` is around 40 bytes, so a peer could ask for ~170 GB of allocation
/// with a ten-byte diff, and both ends keep a ring of states. A bound is not a
/// nicety here: without one, the smallest hostile message in the protocol is
/// also the most expensive.
///
/// 256Ki cells is roughly five times the largest terminal anyone actually
/// runs — a 4K display at a 6-pixel font is about 400x120, or 48,000 cells —
/// and caps one state at about 10 MB. It is deliberately generous: the point
/// is to make the allocation bounded, not to police window sizes.
pub const MAX_SCREEN_CELLS: usize = 262_144;

/// The largest value either dimension may take on its own.
///
/// [`MAX_SCREEN_CELLS`] already bounds the allocation; this bounds the
/// *geometry*, so a 65535x4 screen cannot exist either. It is beyond any real
/// terminal: an 8K display at a 4-pixel font would be about 1920 columns.
pub const MAX_SCREEN_DIM: u16 = 2048;

/// The most bytes one cell's text may carry — the ceiling behind **I8**.
///
/// A cell holds one grapheme cluster: a base character plus whatever combining
/// marks hang off it. Four bytes of base plus a long stack of three-byte marks
/// covers every script anyone writes — a fully pointed Hebrew word, a Tibetan
/// stack, a Devanagari conjunct, an emoji with a variation selector and a ZWJ
/// all fit inside 32 bytes with room to spare.
///
/// It is a ceiling, not a policy on typography, and it is the bound that stops
/// the amplification. Nothing else in the protocol measures bytes-per-cell:
/// `MAX_DECOMPRESSED` is 64 MiB, so without this one cell could legally carry
/// ~60 MiB of text, and the apply loop clones a run's cells `repeat + 1` times
/// across a row — up to [`MAX_SCREEN_DIM`] columns. That is ~123 GiB from a
/// diff of a few hundred bytes. Same shape as the resize bomb I7 closed,
/// entering through the one dimension I7 does not measure.
///
/// Note that `alacritty_terminal` puts **no** cap on how many zero-width marks
/// it stacks on one cell, so an honest host running an honest program can
/// exceed this. That is why `oxutrm-term` *maintains* the bound at the source
/// rather than asserting it: see `cell_text` there.
pub const MAX_CELL_TEXT: usize = 32;

/// The most bytes a window title may carry — the second half of **I8**.
///
/// Comfortably past any real title: a path plus a command name plus a host
/// name is well under a hundred bytes. The number exists so that "the title"
/// is a bounded quantity at all, because the client interpolates it into an
/// OSC sequence written to the user's real terminal.
pub const MAX_TITLE: usize = 512;

/// The most bytes of text one clipboard copy (OSC 52) may carry -- the third
/// bound of **I8**, beside [`MAX_CELL_TEXT`] and [`MAX_TITLE`].
///
/// Enough for a vim yank of a large source file; after zstd about the size of
/// one truecolor full state. A larger copy is dropped whole on the host, never
/// truncated: a truncated paste is silent data corruption.
pub const MAX_CLIPBOARD: usize = 256 * 1024;

/// The most bytes one hyperlink URI (OSC 8) may carry -- the bound of
/// **I10**, beside the I8 bounds above.
///
/// VTE's own limit, and VTE is the most common local terminal: a longer URI
/// would be dropped there anyway.
pub const MAX_LINK_URI: usize = 2083;

/// The most hyperlinks one state may hold (**I10**).
///
/// Far above any real screen, and it bounds a state's link table to about
/// 1 MiB. On the host a link that does not fit is dropped; its text stays.
pub const MAX_LINKS: usize = 512;

/// Which piece of text an **I8** rejection is about.
///
/// The two fields obey the same rule with different bounds, and an error that
/// did not say which one it meant would send whoever reads the log looking at
/// the wrong half of the screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextField {
    /// [`Cell::text`](crate::Cell::text).
    CellText,
    /// [`ScreenState::title`](crate::ScreenState::title).
    Title,
    /// The text of [`ScreenState::clipboard`](crate::ScreenState::clipboard).
    Clipboard,
    /// A URI in [`ScreenState::links`](crate::ScreenState::links) (**I10**).
    LinkUri,
}

impl TextField {
    /// The ceiling this field is held to.
    pub const fn max_bytes(self) -> usize {
        match self {
            TextField::CellText => MAX_CELL_TEXT,
            TextField::Title => MAX_TITLE,
            TextField::Clipboard => MAX_CLIPBOARD,
            TextField::LinkUri => MAX_LINK_URI,
        }
    }
}

impl std::fmt::Display for TextField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TextField::CellText => f.write_str("cell text"),
            TextField::Title => f.write_str("title"),
            TextField::Clipboard => f.write_str("clipboard"),
            TextField::LinkUri => f.write_str("link URI"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

impl TermSize {
    /// **I7.** Both dimensions are within [`MAX_SCREEN_DIM`] and their product
    /// is within [`MAX_SCREEN_CELLS`].
    ///
    /// Call this **before allocating**, never after. Every other invariant can
    /// be checked on a state that already exists, because building the state
    /// is cheap and being wrong about it is what costs. I7 is the opposite: by
    /// the time an oversized state exists, the damage — the allocation — has
    /// already been done. That is why this is a method on the size rather than
    /// only another arm of [`ScreenState::validate`], and why `validate` calls
    /// it rather than the other way round.
    ///
    /// [`ScreenState::validate`]: crate::ScreenState::validate
    pub fn check_bounds(self) -> Result<(), crate::ApplyError> {
        // `usize` is at least 32 bits on every platform oxutrm targets and both
        // operands are `u16`, so this product cannot overflow.
        if self.rows > MAX_SCREEN_DIM
            || self.cols > MAX_SCREEN_DIM
            || self.rows as usize * self.cols as usize > MAX_SCREEN_CELLS
        {
            return Err(crate::ApplyError::ScreenTooLarge {
                rows: self.rows,
                cols: self.cols,
            });
        }
        Ok(())
    }
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

/// The NAT type of each address family, side by side.
///
/// The two families are classified separately because they fail separately:
/// IPv6 is usually unNATed (`None`) while the same host's IPv4 sits behind a
/// symmetric NAT, and a single verdict would hide one behind the other. Each
/// rung of the ladder belongs to one family, so the planner asks for the NAT
/// of the family it is planning, never for a merged answer. A family nobody
/// probed (no socket in that family, no server answered) is `Unknown`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FamilyNat {
    pub v4: NatType,
    pub v6: NatType,
}

impl FamilyNat {
    /// Nothing is known about either family.
    pub const UNKNOWN: FamilyNat = FamilyNat {
        v4: NatType::Unknown,
        v6: NatType::Unknown,
    };

    /// The NAT type of one family.
    pub fn get(self, f: Family) -> NatType {
        match f {
            Family::V4 => self.v4,
            Family::V6 => self.v6,
        }
    }
}

/// An address family. Every rung of the ladder except the ssh tunnel exists
/// once per family, because the two families fail independently: a host can
/// have a working IPv6 path while its IPv4 NAT is symmetric, or the reverse.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    /// Both families in ladder order: IPv6 first, because it needs no NAT
    /// traversal and so is the cheaper path whenever it works.
    pub const BOTH: [Family; 2] = [Family::V6, Family::V4];

    /// The family of an address. A v4-mapped IPv6 address (`::ffff:a.b.c.d`)
    /// is an IPv4 path that a dual-stack socket reports in IPv6 clothing, so
    /// it counts as `V4`.
    pub fn of(addr: &std::net::SocketAddr) -> Family {
        if addr.ip().to_canonical().is_ipv4() {
            Family::V4
        } else {
            Family::V6
        }
    }

    /// The name shown to the user.
    pub fn label(self) -> &'static str {
        match self {
            Family::V4 => "IPv4",
            Family::V6 => "IPv6",
        }
    }
}

/// Which rung of the ladder produced the path in use: a method, and for every
/// method but the ssh tunnel the address family it ran in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Rung {
    /// Both ends' own addresses, with no NAT in the way that needs help. This
    /// includes an IPv4 path over a LAN or VPN.
    Direct(Family),
    /// An address the router was asked to open (UPnP, NAT-PMP, PCP).
    PortMapped(Family),
    /// A STUN-learned address, reached by simultaneous probing.
    Punched(Family),
    /// The birthday blast against a symmetric NAT.
    Birthday(Family),
    /// `SshTunnel` is not merely slower: a session on it runs its transport
    /// inside the SSH connection, so it can never daemonize and can never be
    /// reattached.
    SshTunnel,
}

impl Rung {
    /// Every rung in ladder order: per method, IPv6 before IPv4.
    pub const ALL: [Rung; 9] = [
        Rung::Direct(Family::V6),
        Rung::Direct(Family::V4),
        Rung::PortMapped(Family::V6),
        Rung::PortMapped(Family::V4),
        Rung::Punched(Family::V6),
        Rung::Punched(Family::V4),
        Rung::Birthday(Family::V6),
        Rung::Birthday(Family::V4),
        Rung::SshTunnel,
    ];

    /// The address family of the rung, or `None` for the ssh tunnel.
    pub fn family(self) -> Option<Family> {
        match self {
            Rung::Direct(f) | Rung::PortMapped(f) | Rung::Punched(f) | Rung::Birthday(f) => Some(f),
            Rung::SshTunnel => None,
        }
    }

    /// The number of the method, shared by both families: direct 0, port
    /// mapped 1, punched 2, birthday 3, ssh tunnel 4.
    pub fn method_number(self) -> u8 {
        match self {
            Rung::Direct(_) => 0,
            Rung::PortMapped(_) => 1,
            Rung::Punched(_) => 2,
            Rung::Birthday(_) => 3,
            Rung::SshTunnel => 4,
        }
    }

    /// The rung a nominated remote candidate belongs to. This is the single
    /// classifier: ICE labels the path it nominates with it, and the ladder
    /// uses it to tell which rung a candidate pair belongs to.
    ///
    /// A `Host` candidate is a direct path in either family: an IPv4 host
    /// address is a LAN or VPN address, and reaching it needs no punching.
    pub fn of_candidate(kind: CandidateKind, remote: std::net::SocketAddr) -> Rung {
        let f = Family::of(&remote);
        match kind {
            CandidateKind::PortMapped => Rung::PortMapped(f),
            CandidateKind::Host => Rung::Direct(f),
            CandidateKind::ServerReflexive | CandidateKind::PeerReflexive => Rung::Punched(f),
        }
    }
}

impl std::fmt::Display for Rung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let method = match self {
            Rung::Direct(_) => "direct",
            Rung::PortMapped(_) => "port mapped",
            Rung::Punched(_) => "punched",
            Rung::Birthday(_) => "birthday",
            Rung::SshTunnel => return f.pad("SSH tunnel"),
        };
        // `pad`, not `write!`, so a caller's width and fill (`{:<20}`) apply.
        match self.family() {
            Some(fam) => f.pad(&format!("{} {method}", fam.label())),
            None => f.pad(method),
        }
    }
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
    /// The terminal is worth sending OSC 8 hyperlinks to. A terminal without
    /// them ignores the sequence, so only `dumb` is off.
    pub osc8: bool,
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
    /// The NAT of the nominated rung's family, or `Unknown` for the ssh
    /// tunnel, which crosses none. Not a merged verdict: the other family's
    /// NAT was not on this path.
    pub nat_type: NatType,
    pub rtt_ms: u32,
    pub mtu: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report lines rung names up in a column, so width and fill must apply.
    #[test]
    fn a_rung_honours_the_callers_width() {
        assert_eq!(
            format!("{:<14}|", Rung::Birthday(Family::V4)),
            "IPv4 birthday |"
        );
        assert_eq!(format!("{:>12}|", Rung::SshTunnel), "  SSH tunnel|");
    }

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
    fn the_nat_type_travels_per_family() {
        let n = FamilyNat {
            v4: NatType::Symmetric,
            v6: NatType::None,
        };
        assert_eq!(
            serde_json::to_string(&n).unwrap(),
            r#"{"v4":"Symmetric","v6":"None"}"#
        );
        assert_eq!(n.get(Family::V4), NatType::Symmetric);
        assert_eq!(n.get(Family::V6), NatType::None);
        assert_eq!(FamilyNat::UNKNOWN.get(Family::V6), NatType::Unknown);
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
            serde_json::to_string(&Rung::Direct(Family::V6)).unwrap(),
            r#"{"Direct":"V6"}"#
        );
        assert_eq!(
            serde_json::to_string(&Rung::Punched(Family::V4)).unwrap(),
            r#"{"Punched":"V4"}"#
        );
        assert_eq!(
            serde_json::to_string(&Rung::SshTunnel).unwrap(),
            r#""SshTunnel""#
        );
    }

    #[test]
    fn a_host_candidate_is_direct_in_either_family() {
        let v4: std::net::SocketAddr = "192.168.0.11:443".parse().unwrap();
        let v6: std::net::SocketAddr = "[2001:db8::2]:443".parse().unwrap();
        assert_eq!(
            Rung::of_candidate(CandidateKind::Host, v4),
            Rung::Direct(Family::V4)
        );
        assert_eq!(
            Rung::of_candidate(CandidateKind::Host, v6),
            Rung::Direct(Family::V6)
        );
        assert_eq!(
            Rung::of_candidate(CandidateKind::PortMapped, v6),
            Rung::PortMapped(Family::V6)
        );
        assert_eq!(
            Rung::of_candidate(CandidateKind::ServerReflexive, v4),
            Rung::Punched(Family::V4)
        );
        assert_eq!(
            Rung::of_candidate(CandidateKind::PeerReflexive, v6),
            Rung::Punched(Family::V6)
        );
    }

    #[test]
    fn a_v4_mapped_address_is_ipv4() {
        let mapped: std::net::SocketAddr = "[::ffff:192.0.2.7]:443".parse().unwrap();
        assert_eq!(Family::of(&mapped), Family::V4);
        assert_eq!(
            Rung::of_candidate(CandidateKind::PeerReflexive, mapped),
            Rung::Punched(Family::V4)
        );
    }

    #[test]
    fn every_rung_is_labelled_with_family_and_method_in_ladder_order() {
        let labels: Vec<String> = Rung::ALL.iter().map(ToString::to_string).collect();
        assert_eq!(
            labels,
            [
                "IPv6 direct",
                "IPv4 direct",
                "IPv6 port mapped",
                "IPv4 port mapped",
                "IPv6 punched",
                "IPv4 punched",
                "IPv6 birthday",
                "IPv4 birthday",
                "SSH tunnel",
            ]
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
            osc8: true,
            term_name: "xterm-256color".to_string(),
        };
        let back: TerminalCaps =
            serde_json::from_str(&serde_json::to_string(&caps).expect("encode")).expect("decode");
        assert_eq!(back.colors, 16_777_216);
        assert_eq!(back.term_name, "xterm-256color");
        assert!(
            back.truecolor && back.bracketed_paste && back.mouse_sgr && back.osc52 && back.osc8
        );
    }

    #[test]
    fn a_path_description_round_trips() {
        let p = PathDescription {
            rung: Rung::Birthday(Family::V4),
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

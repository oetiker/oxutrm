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
}

impl TextField {
    /// The ceiling this field is held to.
    pub const fn max_bytes(self) -> usize {
        match self {
            TextField::CellText => MAX_CELL_TEXT,
            TextField::Title => MAX_TITLE,
        }
    }
}

impl std::fmt::Display for TextField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TextField::CellText => f.write_str("cell text"),
            TextField::Title => f.write_str("title"),
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

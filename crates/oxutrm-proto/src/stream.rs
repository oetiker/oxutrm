//! Messages carried on QUIC streams rather than datagrams.
//!
//! Separate streams matter: a 50 000-line scrollback fetch must never delay a
//! keystroke.

use serde::{Deserialize, Serialize};

use crate::{PathDescription, TerminalCaps};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ControlMsg {
    SessionInfo {
        session_id: String,
        shell: String,
        created_unix: u64,
    },
    /// Carried for diagnosis and for the client's own down-conversion. It never
    /// reaches the child shell's environment (spec §9.4).
    CapsUpdate(TerminalCaps),
    StatusRequest,
    StatusReply(PathDescription),
}

/// Scrollback lines `[from_line, to_line)` — half open.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScrollbackReq {
    pub from_line: u64,
    pub to_line: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NatType, PathDescription, Rung, TerminalCaps};

    fn caps() -> TerminalCaps {
        TerminalCaps {
            truecolor: false,
            colors: 256,
            bracketed_paste: true,
            mouse_sgr: true,
            osc52: false,
            term_name: "screen-256color".to_string(),
        }
    }

    fn round_trip(m: &ControlMsg) -> ControlMsg {
        let bytes = postcard::to_stdvec(m).expect("encode");
        postcard::from_bytes(&bytes).expect("decode")
    }

    #[test]
    fn session_info_round_trips() {
        match round_trip(&ControlMsg::SessionInfo {
            session_id: "00112233445566778899aabbccddeeff".to_string(),
            shell: "/bin/bash".to_string(),
            created_unix: 1_774_000_000,
        }) {
            ControlMsg::SessionInfo {
                session_id,
                shell,
                created_unix,
            } => {
                assert_eq!(session_id, "00112233445566778899aabbccddeeff");
                assert_eq!(shell, "/bin/bash");
                assert_eq!(created_unix, 1_774_000_000);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn a_caps_update_round_trips() {
        match round_trip(&ControlMsg::CapsUpdate(caps())) {
            ControlMsg::CapsUpdate(c) => {
                assert_eq!(c.colors, 256);
                assert_eq!(c.term_name, "screen-256color");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn a_status_request_is_one_byte() {
        let bytes = postcard::to_stdvec(&ControlMsg::StatusRequest).expect("encode");
        assert_eq!(bytes, vec![0x02], "the variant index and nothing else");
    }

    #[test]
    fn a_status_reply_carries_the_path() {
        match round_trip(&ControlMsg::StatusReply(PathDescription {
            rung: Rung::PortMapped,
            local: "0.0.0.0:443".parse().unwrap(),
            remote: "198.51.100.1:60000".parse().unwrap(),
            probes_sent: 0,
            nat_type: NatType::EndpointIndependent,
            rtt_ms: 38,
            mtu: 1392,
        })) {
            ControlMsg::StatusReply(p) => {
                assert_eq!(p.rung, Rung::PortMapped);
                assert_eq!(p.rtt_ms, 38);
                assert_eq!(p.mtu, 1392);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn a_scrollback_request_is_a_half_open_range() {
        let req = ScrollbackReq {
            from_line: 0,
            to_line: 50_000,
        };
        let back: ScrollbackReq =
            postcard::from_bytes(&postcard::to_stdvec(&req).expect("encode")).expect("decode");
        assert_eq!(back.from_line, 0);
        assert_eq!(back.to_line, 50_000);
    }
}

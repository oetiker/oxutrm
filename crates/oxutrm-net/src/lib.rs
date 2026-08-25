#![forbid(unsafe_code)]

//! Getting a datagram from one end to the other when both ends are behind NAT.
//!
//! The five-rung ladder lives here — IPv6 direct, router port mapping, STUN
//! hole punching, the birthday-paradox blast, and the SSH tunnel of last
//! resort — along with the QUIC connection that runs over whichever socket the
//! ladder managed to punch.
//!
//! # One socket, from the first probe to the last datagram
//!
//! The crate revolves around a **single** UDP socket. It is bound once
//! ([`bind_socket`]), used for STUN discovery, used again for ICE connectivity
//! checks, and finally handed to `quinn`. That is not tidiness: NAT mappings
//! are per-socket, so an address learned on any other socket describes a hole
//! that our traffic will never come out of.
//!
//! Two consequences shape everything here.
//!
//! **STUN and QUIC share the wire.** `quinn` owns the socket's receive loop —
//! `Endpoint::new` runs its own `recv` — so STUN and QUIC cannot both call
//! `recv` on the same socket without racing and stealing each other's packets.
//! The answer is a `quinn::AsyncUdpSocket` wrapper that peels STUN off the
//! front, installed with `Endpoint::new_with_abstract_socket`. [`is_stun`] is
//! the discriminator that makes it possible, and [`bind_socket`] returns a
//! plain `std::net::UdpSocket` precisely so that handing it over later is a
//! move rather than a rewrite.
//!
//! **`stunclient` is for pre-QUIC discovery only.** Its API offers no
//! `MESSAGE-INTEGRITY` at all, so it can ask a public server "what is my
//! address" and nothing more. Every ICE connectivity check and keepalive is
//! built directly on `stun_codec` + `hmac`, because an unauthenticated probe
//! would let a stranger advance our state machine and would make oxutrm usable
//! as a reflector.

mod birthday;
mod candidates;
mod config;
mod demux;
mod demuxsock;
mod der;
mod discover;
mod ice;
mod mapping;
mod quic;
mod socketfam;
mod stunmsg;
mod stunserver;
mod tls;

pub use birthday::{BirthdayResult, birthday_blast, guessed_ports};
pub use candidates::{ice_priority, is_link_local, local_candidates, local_candidates_filtered};
pub use config::NetConfig;
pub use demux::{STUN_HEADER_LEN, STUN_MAGIC_COOKIE, is_stun};
pub use demuxsock::{StunDemuxSocket, StunRx};
pub use der::{spki_der, spki_sha256};
pub use discover::{Probe, classify, stun_discover};
pub use ice::{IceAgent, IceEvent};
pub use mapping::{PortMapping, default_gateway};
pub use quic::{ALPN, quic_client, quic_server};
pub use socketfam::{bind_socket, to_socket_family, unmap, unmap_ip};
pub use stunmsg::{
    Check, CheckKind, Direction, IceCredentials, IceRole, build_check_request,
    build_check_response, build_nomination, parse_check, random_transaction_id,
};
pub use stunserver::{MappingBehaviour, StunResponder};
pub use tls::{CERT_NAME, PinnedSpki, generate_cert, install_crypto_provider, provider};

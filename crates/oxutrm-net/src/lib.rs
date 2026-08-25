#![forbid(unsafe_code)]

//! Getting a datagram from one end to the other when both ends are behind NAT.
//!
//! The five-rung ladder lives here — IPv6 direct, router port mapping, STUN
//! hole punching, the birthday-paradox blast, and the SSH tunnel of last
//! resort — along with the QUIC connection that runs over whichever socket the
//! ladder managed to punch.

//! Keeping a standby off the primary's path (spec §3.3).
//!
//! A standby exists to survive what kills the primary, so a standby that
//! leaves this machine the same way the primary does is worthless: it is the
//! same wire with a second connection on it. "The same way" is decided by
//! the kernel's own answer to "which local address would you send this from",
//! which `route_source` asks without sending anything.
//!
//! **What this actually compares, and what it does not guarantee.** The
//! comparison is the primary's *source address*, not its interface. A
//! dual-stack or multi-address NIC can hand two different routes two
//! different source addresses while both leave over the very same wire --
//! so "different source address" is not proof of "different interface".
//! On a single uplink with several addresses (say an IPv4 and an IPv6
//! address on the same NIC) a standby admitted by this filter can still
//! share the primary's wire. If that wire dies, the standby dies with it.
//! That is a known limit, not a bug: the standby simply behaves as if it
//! had never been found, which is the same outcome as never finding one.
//!
//! This filter is consulted synchronously from `IceAgent`'s poll loop, so it
//! must never panic and never block beyond a nonblocking UDP bind+connect --
//! exactly what `route_source` does and nothing more.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::net::{IpAddr, SocketAddr};

use oxutrm_net::RemoteFilter;

/// A filter admitting only remotes this machine would reach from a different
/// local address than it reaches `primary_remote` from.
///
/// `None` when the primary's own route cannot be read. A standby search is
/// then pointless rather than dangerous (it could end up on the primary's own
/// path), so the caller skips it and says there is no standby.
pub(crate) fn avoiding_source(primary_remote: SocketAddr) -> Option<RemoteFilter> {
    let baseline = crate::roam::route_source(primary_remote).ok()?;
    Some(std::sync::Arc::new(move |remote| {
        admits(baseline, crate::roam::route_source(remote))
    }))
}

/// The decision, without the kernel.
///
/// `seen` is `route_source`'s answer for the candidate remote: `Ok` is the
/// source address the kernel would use, `Err` means the kernel could not
/// answer at all (the remote is unroutable right now). An `Err` is mapped to
/// "admit" -- a definite decision, never an unwrap -- because refusing would
/// need a second opinion on a route the kernel has already declined to give,
/// and ICE will not validate an unroutable candidate anyway: admitting it
/// costs a few wasted probes, nothing more.
fn admits(baseline: IpAddr, seen: std::io::Result<IpAddr>) -> bool {
    seen.map_or(true, |ip| ip != baseline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_reached_from_the_primarys_source_is_refused() {
        let primary: IpAddr = "10.8.0.2".parse().unwrap();
        assert!(!admits(primary, Ok(primary)));
    }

    #[test]
    fn a_remote_reached_from_another_source_is_admitted() {
        let primary: IpAddr = "10.8.0.2".parse().unwrap();
        let other: IpAddr = "192.168.1.20".parse().unwrap();
        assert!(admits(primary, Ok(other)));
    }

    #[test]
    fn an_unroutable_remote_is_admitted_because_it_cannot_validate_anyway() {
        let primary: IpAddr = "10.8.0.2".parse().unwrap();
        let err = std::io::Error::from(std::io::ErrorKind::NetworkUnreachable);
        assert!(admits(primary, Err(err)));
    }

    /// The whole function on this machine's real routing table: loopback is
    /// reached from loopback, so a loopback primary refuses a loopback remote.
    #[test]
    fn on_loopback_the_filter_refuses_loopback() {
        let f = avoiding_source("127.0.0.1:4433".parse().unwrap()).expect("loopback routes");
        assert!(!f("127.0.0.1:9".parse().unwrap()));
    }
}

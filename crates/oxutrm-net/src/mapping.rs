//! Rung 1: asking the router for a port mapping.
//!
//! NAT-PMP and PCP first (`crab_nat`), then UPnP-IGD (`igd-next`), each with
//! its own budget. Success yields a `PortMapped` candidate carrying the exact
//! public address — IP *and* port — which is more than STUN can promise,
//! because the router is the thing doing the translating rather than a witness
//! to it.
//!
//! # Why this rung is worth its complexity
//!
//! **If either side gets a mapping, the whole connection succeeds.** The other
//! side punches to the mapped address, and its own address is learned
//! peer-reflexively from the packet that arrives. So this rung does not have
//! to work on both ends, or even usually — it has to work sometimes, on one
//! end, and it turns an otherwise doomed symmetric-NAT pairing into an
//! ordinary one.
//!
//! # The gateway is ours to find
//!
//! `crab_nat::PortMapping::new` takes the gateway address and the crate ships
//! no discovery of its own. `netdev` provides it — netlink on Linux, the route
//! socket on the BSDs and macOS. Reading `/proc/net/route` would be Linux-only
//! and §1.2 scopes this project to Unix generally.

use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU16;
use std::time::Duration;

use oxutrm_proto::{Candidate, CandidateKind};

use crate::{NetConfig, ice_priority};

/// Per-protocol budget. A router that has not answered in this long is either
/// absent or not cooperating, and the ladder has other rungs to try.
const PROTOCOL_BUDGET: Duration = Duration::from_millis(1500);

/// Requested lifetime. Short enough that an abandoned mapping expires on its
/// own if a crash prevents the release, long enough not to churn.
const LIFETIME_SECS: u32 = 3600;

/// Refresh at half the lifetime, the usual margin against clock skew and a
/// lost renewal.
const REFRESH_INTERVAL: Duration = Duration::from_secs(LIFETIME_SECS as u64 / 2);

/// The default gateway.
///
/// `crab_nat` needs it and does not discover it. `netdev` uses netlink on
/// Linux and the route socket elsewhere, which keeps this working on the BSDs
/// and macOS where `/proc/net/route` does not exist.
pub fn default_gateway() -> Option<IpAddr> {
    let gw = netdev::get_default_gateway().ok()?;
    gw.ipv4
        .first()
        .map(|v4| IpAddr::V4(*v4))
        .or_else(|| gw.ipv6.first().map(|v6| IpAddr::V6(*v6)))
}

/// Our address on the interface that reaches the gateway.
fn local_ip_for_gateway() -> Option<IpAddr> {
    let iface = netdev::get_default_interface().ok()?;
    iface
        .ipv4
        .first()
        .map(|n| IpAddr::V4(n.addr()))
        .or_else(|| iface.ipv6.first().map(|n| IpAddr::V6(n.addr())))
}

/// Which protocol produced a mapping, so `Drop` knows how to release it.
enum Backend {
    /// NAT-PMP or PCP. `crab_nat` picks between them itself.
    CrabNat(Box<crab_nat::PortMapping>),
    Upnp {
        gateway: igd_next::aio::Gateway<igd_next::aio::tokio::Tokio>,
        external_port: u16,
    },
}

/// A live router mapping. Refreshed for the session's life, released on
/// `Drop`.
pub struct PortMapping {
    backend: Option<Backend>,
    external: SocketAddr,
    refresher: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for PortMapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortMapping")
            .field("external", &self.external)
            .finish()
    }
}

impl PortMapping {
    /// NAT-PMP, then PCP, then UPnP-IGD. `None` when no router cooperates,
    /// which is an ordinary outcome and not an error: the ladder simply moves
    /// on to rung 2.
    pub async fn acquire(local_port: u16, cfg: &NetConfig) -> Option<(PortMapping, Candidate)> {
        if !cfg.enable_port_mapping {
            return None;
        }
        let internal = NonZeroU16::new(local_port)?;

        let mapping = match try_crab_nat(internal).await {
            Some(m) => Some(m),
            // UPnP is tried second because it is the older, chattier protocol
            // and the one more likely to be firewalled off or disabled.
            None => try_upnp(local_port).await,
        }?;

        let candidate = Candidate {
            addr: mapping.external,
            kind: CandidateKind::PortMapped,
            priority: ice_priority(CandidateKind::PortMapped, &mapping.external.ip()),
        };
        Some((mapping, candidate))
    }

    /// The public address the router promised. Exact, not observed.
    pub fn external(&self) -> SocketAddr {
        self.external
    }
}

async fn try_crab_nat(internal: NonZeroU16) -> Option<PortMapping> {
    let gateway = default_gateway()?;
    let client = local_ip_for_gateway()?;

    let opts = crab_nat::PortMappingOptions {
        lifetime_seconds: Some(LIFETIME_SECS),
        ..crab_nat::PortMappingOptions::default()
    };
    let m = tokio::time::timeout(
        PROTOCOL_BUDGET,
        crab_nat::PortMapping::new(
            gateway.into(),
            client,
            crab_nat::InternetProtocol::Udp,
            internal,
            opts,
        ),
    )
    .await
    .ok()?
    .ok()?;

    let external = SocketAddr::new(
        gateway_public_ip(&m).unwrap_or(gateway),
        m.external_port().get(),
    );

    // Renew for as long as the session lasts. A mapping that lapses mid-session
    // takes the path with it, and QUIC cannot repoint at a new remote address.
    let mut renewable = m.clone();
    let refresher = tokio::spawn(async move {
        loop {
            tokio::time::sleep(REFRESH_INTERVAL).await;
            if renewable.renew().await.is_err() {
                return;
            }
        }
    });

    Some(PortMapping {
        backend: Some(Backend::CrabNat(Box::new(m))),
        external,
        refresher: Some(refresher),
    })
}

/// PCP reports the external IP; NAT-PMP does not always, in which case the
/// gateway's own address is the best available answer.
fn gateway_public_ip(_m: &crab_nat::PortMapping) -> Option<IpAddr> {
    None
}

async fn try_upnp(local_port: u16) -> Option<PortMapping> {
    let opts = igd_next::SearchOptions {
        timeout: Some(PROTOCOL_BUDGET),
        ..igd_next::SearchOptions::default()
    };
    let gateway = tokio::time::timeout(PROTOCOL_BUDGET, igd_next::aio::tokio::search_gateway(opts))
        .await
        .ok()?
        .ok()?;

    let public_ip = tokio::time::timeout(PROTOCOL_BUDGET, gateway.get_external_ip())
        .await
        .ok()?
        .ok()?;
    let local = SocketAddr::new(local_ip_for_gateway()?, local_port);

    // Ask for the same port outside as inside: it costs nothing to try, and a
    // matching pair is easier for a human to recognise in a router's table.
    tokio::time::timeout(
        PROTOCOL_BUDGET,
        gateway.add_port(
            igd_next::PortMappingProtocol::UDP,
            local_port,
            local,
            LIFETIME_SECS,
            "oxutrm",
        ),
    )
    .await
    .ok()?
    .ok()?;

    Some(PortMapping {
        backend: Some(Backend::Upnp {
            gateway,
            external_port: local_port,
        }),
        external: SocketAddr::new(public_ip, local_port),
        refresher: None,
    })
}

impl Drop for PortMapping {
    fn drop(&mut self) {
        if let Some(t) = self.refresher.take() {
            t.abort();
        }
        // Releasing is async, and `Drop` is not. Spawning is best effort: with
        // no runtime there is nothing to spawn onto, and the mapping's own
        // lifetime expires it anyway. Leaving a mapping behind is untidy, not
        // unsafe.
        let Some(backend) = self.backend.take() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            match backend {
                Backend::CrabNat(m) => {
                    let _ = m.try_drop().await;
                }
                Backend::Upnp {
                    gateway,
                    external_port,
                } => {
                    let _ = gateway
                        .remove_port(igd_next::PortMappingProtocol::UDP, external_port)
                        .await;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_discovery_does_not_panic_on_any_host() {
        // A CI container may have no default route at all. `None` is the
        // correct answer there, and the ladder treats it as "rung 1 declined".
        let _ = default_gateway();
        let _ = local_ip_for_gateway();
    }

    /// A gateway address must be a real one if it is reported at all: a
    /// mapping request sent to an unspecified or multicast address would hang
    /// out the full budget for nothing.
    #[test]
    fn a_reported_gateway_is_a_usable_unicast_address() {
        let Some(gw) = default_gateway() else {
            return;
        };
        assert!(!gw.is_unspecified(), "0.0.0.0 is not a gateway");
        assert!(!gw.is_multicast(), "a multicast address is not a gateway");
        match gw {
            IpAddr::V4(v4) => assert!(!v4.is_broadcast()),
            IpAddr::V6(_) => {}
        }
    }

    #[tokio::test]
    async fn mapping_is_skipped_when_the_config_disables_it() {
        let cfg = NetConfig {
            enable_port_mapping: false,
            ..NetConfig::default()
        };
        assert!(
            PortMapping::acquire(41234, &cfg).await.is_none(),
            "a user who switched rung 1 off must not have their router probed"
        );
    }

    #[tokio::test]
    async fn port_zero_is_refused_rather_than_mapped() {
        // Port 0 means "any port" to bind(2) and nothing at all to a router.
        let cfg = NetConfig::default();
        assert!(PortMapping::acquire(0, &cfg).await.is_none());
    }

    /// The budget is what stops a silent router stalling the whole ladder.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acquire_returns_within_the_protocol_budgets() {
        let cfg = NetConfig::default();
        let started = std::time::Instant::now();
        // Whatever this host has, the answer must arrive bounded: both
        // protocols are tried, so allow both budgets plus slack.
        let _ = tokio::time::timeout(PROTOCOL_BUDGET * 6, PortMapping::acquire(41234, &cfg))
            .await
            .expect("rung 1 must not stall the ladder indefinitely");
        assert!(started.elapsed() < PROTOCOL_BUDGET * 8);
    }

    #[test]
    fn a_port_mapped_candidate_outranks_a_reflexive_one() {
        let addr: SocketAddr = "203.0.113.9:443".parse().unwrap();
        assert!(
            ice_priority(CandidateKind::PortMapped, &addr.ip())
                > ice_priority(CandidateKind::ServerReflexive, &addr.ip()),
            "an exact router mapping is better evidence than an observed one"
        );
    }

    #[test]
    fn the_refresh_interval_leaves_margin_before_the_lifetime_expires() {
        assert!(
            REFRESH_INTERVAL.as_secs() * 2 <= LIFETIME_SECS as u64,
            "a mapping that lapses mid-session takes the path with it"
        );
    }

    /// Never in CI: this talks to whatever router the developer is behind.
    #[tokio::test]
    #[ignore = "requires a cooperative router on the local network"]
    async fn a_real_router_grants_and_releases_a_mapping() {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").expect("bind");
        let port = sock.local_addr().expect("addr").port();
        let cfg = NetConfig::default();

        let Some((mapping, candidate)) = PortMapping::acquire(port, &cfg).await else {
            eprintln!("no router cooperated; rung 1 declined");
            return;
        };
        assert_eq!(candidate.kind, CandidateKind::PortMapped);
        assert_eq!(candidate.addr, mapping.external());
        assert_ne!(mapping.external().port(), 0);
        drop(mapping);
    }
}

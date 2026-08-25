//! Everything about the network layer a user might reasonably want to change.

use std::time::Duration;

#[derive(Clone, Debug)]
pub struct NetConfig {
    /// `host:port` strings. Resolved lazily, in parallel. An entry that does
    /// not resolve is skipped rather than fatal: a STUN server list is a list
    /// of hopes, not of requirements.
    ///
    /// Two things are read out of the shape of this list, not just its
    /// contents (see [`crate::stun_discover`]):
    ///
    /// - **entries whose IPs differ** are what makes NAT typing possible at
    ///   all, so at least two distinct operators belong here;
    /// - **two entries with the same resolved IP and different ports** declare
    ///   that the server answers on both, which is the only way to separate
    ///   `AddressDependent` from `Symmetric`. The defaults below cannot do
    ///   this — no public server in the list publishes a second port — so the
    ///   two verdicts stay merged. Adding the pair is an operator's call, and
    ///   the classifier never guesses one.
    pub stun_servers: Vec<String>,
    /// The port to try first. UDP/443 (spec §5.6), because a network that
    /// blocks it breaks HTTP/3 for every browser on it.
    pub prefer_port: u16,
    /// Rung 1: ask the router for a port mapping.
    pub enable_port_mapping: bool,
    /// Rung 3: the birthday blast. Deliberately noisy, so a user may switch
    /// it off.
    pub enable_birthday: bool,
    /// Rung 3: how many extra sockets the blast opens.
    pub birthday_sockets: u16,
    /// Rung 3: how many ports each socket guesses at.
    pub birthday_ports: u16,
    /// Rung 3: a hard wall-clock cap on the whole blast.
    pub birthday_budget: Duration,
    /// How long candidate gathering and connectivity checks may take.
    pub gather_timeout: Duration,
}

impl Default for NetConfig {
    fn default() -> NetConfig {
        NetConfig {
            stun_servers: vec![
                "stun.cloudflare.com:3478".to_owned(),
                "stun.l.google.com:19302".to_owned(),
                // Useful where 3478 is blocked outright.
                "stun.nextcloud.com:443".to_owned(),
                "stun.sipgate.net:3478".to_owned(),
            ],
            prefer_port: 443,
            enable_port_mapping: true,
            enable_birthday: true,
            birthday_sockets: 256,
            birthday_ports: 256,
            birthday_budget: Duration::from_secs(6),
            gather_timeout: Duration::from_secs(3),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_ones_the_spec_names() {
        let c = NetConfig::default();
        assert_eq!(c.prefer_port, 443, "spec §5.6: UDP/443 is tried first");
        assert!(c.enable_port_mapping);
        assert!(c.enable_birthday);
        assert_eq!(c.birthday_sockets, 256);
        assert_eq!(c.birthday_ports, 256);
    }

    /// ~65k combinations against an ephemeral range of similar size is the
    /// whole basis of rung 3. Shrink either number and the collision stops
    /// being likely.
    #[test]
    fn the_birthday_search_space_is_around_65k() {
        let c = NetConfig::default();
        let space = u32::from(c.birthday_sockets) * u32::from(c.birthday_ports);
        assert_eq!(space, 65_536);
    }

    #[test]
    fn the_default_stun_servers_are_diverse_and_include_a_443_fallback() {
        let c = NetConfig::default();
        assert!(
            c.stun_servers.len() >= 3,
            "NAT typing needs several servers"
        );

        // Every entry must parse as host:port, or it can never resolve.
        for s in &c.stun_servers {
            let (host, port) = s
                .rsplit_once(':')
                .unwrap_or_else(|| panic!("no port in {s:?}"));
            assert!(!host.is_empty(), "empty host in {s:?}");
            port.parse::<u16>()
                .unwrap_or_else(|_| panic!("bad port in {s:?}"));
        }

        // Distinct operators: comparing the mapped port from two servers run by
        // the same operator can share a front end and reveal nothing.
        //
        // This also pins the fact the classifier is built around: no two
        // default entries share a host, so the default list declares NO
        // alternate port, so `AddressDependent` and `Symmetric` are merged out
        // of the box. Anything that assumes an alternate port exists here is
        // assuming something this assertion forbids.
        let hosts: std::collections::HashSet<_> = c
            .stun_servers
            .iter()
            .map(|s| s.rsplit_once(':').unwrap().0)
            .collect();
        assert_eq!(hosts.len(), c.stun_servers.len(), "duplicate STUN hosts");

        assert!(
            c.stun_servers.iter().any(|s| s.ends_with(":443")),
            "at least one server must be reachable where 3478 is blocked"
        );
    }

    #[test]
    fn the_budgets_are_finite_and_the_blast_may_take_longer_than_gathering() {
        let c = NetConfig::default();
        assert!(c.gather_timeout > Duration::ZERO);
        assert!(c.birthday_budget > Duration::ZERO);
        assert!(
            c.birthday_budget >= c.gather_timeout,
            "rung 3 fires after gathering has already failed, so it needs at \
             least as long"
        );
    }
}

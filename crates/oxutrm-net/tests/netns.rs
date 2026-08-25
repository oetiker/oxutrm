//! The honest proof that NAT traversal works.
//!
//! Every other test in this crate runs on loopback, where **there is no NAT to
//! traverse**. Rungs 1-3 could be subtly broken and all of them would still
//! pass. These build real Linux network namespaces with real `nftables` NAT
//! between them.
//!
//! # Skipping is not passing
//!
//! A machine without unprivileged user namespaces, or without `ip` and `nft`,
//! cannot run these. Each test then **skips with a printed reason** rather than
//! quietly succeeding: a skipped test that reads as green is worse than a
//! missing one, because it makes an unearned claim.
//!
//! Run `cargo test --test netns -- --nocapture` to see which ran and which
//! skipped, and why.

use std::process::Command;

/// Why the harness cannot run here, or `None` if it can.
fn unsupported() -> Option<String> {
    for tool in ["ip", "nft", "unshare", "nsenter"] {
        if Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {tool}"))
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return Some(format!("`{tool}` is not installed"));
        }
    }
    // The real gate: can this kernel give an unprivileged process a user and
    // network namespace at all?
    match Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "true"])
        .output()
    {
        Ok(o) if o.status.success() => None,
        Ok(o) => Some(format!(
            "unprivileged user+net namespaces unavailable: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Some(format!("could not run unshare: {e}")),
    }
}

fn helper_binary() -> String {
    env!("CARGO_BIN_EXE_oxutrm-netns-peer").to_string()
}

fn script() -> String {
    format!("{}/tests/netns/topology.sh", env!("CARGO_MANIFEST_DIR"))
}

/// Run `args` inside `topology`, returning combined stdout+stderr.
fn in_topology(topology: &str, args: &[&str]) -> Result<String, String> {
    in_topology_with_peer(topology, args, None, None)
}

/// The same, with a peer process launched on the internet side first.
fn in_topology_with_peer(
    topology: &str,
    args: &[&str],
    peer_cmd: Option<&str>,
    peer_log: Option<&str>,
) -> Result<String, String> {
    let mut cmd = Command::new(script());
    if let Some(p) = peer_cmd {
        cmd.env("OXUTRM_PEER_CMD", p);
    }
    if let Some(l) = peer_log {
        cmd.env("OXUTRM_PEER_LOG", l);
    }
    let out = cmd
        .arg(topology)
        .arg("--")
        .args(args)
        .env("OXUTRM_NETNS_PEER", helper_binary())
        .output()
        .map_err(|e| format!("could not run the harness: {e}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() {
        Ok(text)
    } else {
        Err(text)
    }
}

/// Print why we skipped, and return true, so the caller can bail out.
macro_rules! skip_unless_supported {
    ($name:literal) => {
        if let Some(why) = unsupported() {
            eprintln!("SKIP {}: {}", $name, why);
            return;
        }
    };
}

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.split_whitespace()
        .find_map(|tok| tok.strip_prefix(&format!("{key}=")))
}

/// A plain `masquerade`: Linux conntrack's default is endpoint-independent
/// mapping with address-and-port-dependent filtering, which is precisely a
/// port-restricted cone. Rungs 0-2 must work here.
#[test]
fn a_cone_nat_is_classified_and_traversed() {
    skip_unless_supported!("cone");

    let stun = "10.0.2.2:3478,10.0.2.3:3478";
    let out = match in_topology("cone", &[&helper_binary(), "discover", "--stun", stun]) {
        Ok(t) => t,
        Err(t) => panic!("the cone topology could not be built or run:\n{t}"),
    };
    eprintln!("{out}");

    let nat = field(&out, "nat").expect("the helper printed no nat= field");
    let mapped = field(&out, "mapped").expect("no mapped= field");

    assert_ne!(
        mapped, "none",
        "no STUN server answered inside the topology"
    );
    assert_eq!(
        nat, "EndpointIndependent",
        "a plain masquerade must classify as a cone, not {nat}"
    );

    // Assert the PREMISE, not only the conclusion. A cone reuses one external
    // port across destinations; if this ever read `same_port=false` the
    // topology would not be a cone and the classification above would be
    // right for the wrong reason.
    let pair = in_topology(
        "cone",
        &[
            &helper_binary(),
            "probe-pair",
            "--stun",
            "10.0.2.2:3478",
            "--stun2",
            "10.0.2.3:3478",
        ],
    )
    .expect("probe-pair in the cone topology");
    eprintln!("{pair}");
    assert_eq!(
        field(&pair, "same_port"),
        Some("true"),
        "a cone NAT gave two different ports to two destinations: {pair}"
    );
    // The mapped address must be the NAT's outside address, not our own.
    assert!(
        mapped.starts_with("10.0.2."),
        "the mapping did not come from the NAT's outside interface: {mapped}"
    );
}

/// `masquerade fully-random` varies the external port per destination.
///
/// **This is an approximation.** Linux cannot reproduce every commercial NAT.
/// It exercises the same failure mode and the same rung-3 recovery, which is
/// what is being proved — not universal compatibility.
#[test]
fn a_symmetric_nat_is_recognised_as_symmetric() {
    skip_unless_supported!("symmetric");

    let stun = "10.0.2.2:3478,10.0.2.3:3478";
    let out = match in_topology("symmetric", &[&helper_binary(), "discover", "--stun", stun]) {
        Ok(t) => t,
        Err(t) => panic!("the symmetric topology could not be built or run:\n{t}"),
    };
    eprintln!("{out}");

    let nat = field(&out, "nat").expect("no nat= field");
    // Assert the TOPOLOGY first. `random-fully` is the iptables spelling and
    // nft rejects it; get it wrong and this is an ordinary cone, rung 3 never
    // runs, and a test that asserted only "the blast succeeded" would pass
    // while proving nothing.
    assert_eq!(
        nat, "Symmetric",
        "the topology is not actually symmetric ({nat}), so rung 3 is untested here"
    );
    assert_ne!(field(&out, "mapped"), Some("none"));

    // The premise the classifier infers from: a DIFFERENT external port per
    // destination. Without this the `Symmetric` above could be right for some
    // other reason, and rung 3 would go untested by the test that exists to
    // test it.
    let pair = in_topology(
        "symmetric",
        &[
            &helper_binary(),
            "probe-pair",
            "--stun",
            "10.0.2.2:3478",
            "--stun2",
            "10.0.2.3:3478",
        ],
    )
    .expect("probe-pair in the symmetric topology");
    eprintln!("{pair}");
    assert_eq!(
        field(&pair, "same_port"),
        Some("false"),
        "the two destinations saw the SAME external port, so this NAT is not \
         symmetric — check the nft rule says `fully-random`, not the iptables \
         spelling `random-fully`: {pair}"
    );
    let first = field(&pair, "first").expect("first=");
    let second = field(&pair, "second").expect("second=");
    assert_ne!(first, second, "identical mappings: {pair}");
}

/// Rung 3 itself, through a genuinely symmetric NAT.
///
/// The blast is told a base three ports below where the peer actually listens,
/// so it has to search outward rather than hitting it on the first guess. If
/// rung 2 could quietly have succeeded here, the topology would not be
/// symmetric and the premise assertion above would have failed first.
#[test]
fn the_birthday_blast_punches_through_a_symmetric_nat() {
    skip_unless_supported!("blast");

    let dir = std::env::temp_dir().join(format!("oxutrm-blast-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let peer_log = dir.join("peer.log");

    let psk = "5a".repeat(32);
    let bin = helper_binary();
    // A peer that answers authenticated checks. Its own remote is a dead
    // address; all it has to do here is reply to what the blast sends.
    let peer_cmd = format!(
        "{bin} ice --role controlled --bind 10.0.2.2:46000 --remote 10.0.2.9:1 --psk {psk}"
    );

    let out = in_topology_with_peer(
        "symmetric",
        &[
            &bin,
            "blast",
            "--peer",
            "10.0.2.2:45997",
            "--ports",
            "32",
            "--sockets",
            "4",
            "--psk",
            &psk,
        ],
        Some(&peer_cmd),
        peer_log.to_str(),
    );
    let out = match out {
        Ok(t) => t,
        Err(t) => panic!("the blast found no hole through the symmetric NAT:\n{t}"),
    };
    eprintln!("{out}");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        field(&out, "remote"),
        Some("10.0.2.2:46000"),
        "the blast reported a hole somewhere other than the listening peer"
    );
    let probes: u32 = field(&out, "probes")
        .expect("no probes= field")
        .parse()
        .expect("probes is not a number");
    assert!(
        probes > 0,
        "a hole was reported without any probe being sent"
    );
    // Told :45997, found :46000 — three ports out, so the search really ran.
    assert!(
        probes > 4,
        "only {probes} probes: the peer was found too easily for the search to \
         have been exercised"
    );
}

/// Two nested NATs. The point is the FALLBACK: no router will map a port for
/// us through two layers, so rung 1 must fail cleanly and rung 2 take over.
#[test]
fn a_double_nat_falls_back_to_stun_punching() {
    skip_unless_supported!("double");

    let stun = "10.0.2.2:3478,10.0.2.3:3478";
    let out = match in_topology("double", &[&helper_binary(), "discover", "--stun", stun]) {
        Ok(t) => t,
        Err(t) => panic!("the double-NAT topology could not be built or run:\n{t}"),
    };
    eprintln!("{out}");

    let mapped = field(&out, "mapped").expect("no mapped= field");
    assert_ne!(
        mapped, "none",
        "rung 2 did not take over after rung 1 could not map through two NATs"
    );
    // Behind two layers, the address the world sees is the OUTER NAT's.
    assert!(
        mapped.starts_with("10.0.2."),
        "the mapping is not the outer NAT's address: {mapped}"
    );
}

/// The harness itself must be sound: an unknown topology fails loudly rather
/// than silently producing an unNATted path that every assertion would pass.
#[test]
fn an_unknown_topology_is_rejected() {
    skip_unless_supported!("harness");
    assert!(
        in_topology(
            "not-a-topology",
            &[&helper_binary(), "discover", "--stun", "10.0.2.2:3478"]
        )
        .is_err(),
        "the harness accepted a topology it does not know how to build"
    );
}

/// The product claim, end to end: a peer behind a NAT and a peer outside it
/// complete an ICE nomination through the NAT.
///
/// The internet-side namespace has **no route into the private LAN**, so the
/// far peer cannot dial the client directly. The only way in is the mapping
/// the client's own outbound packet creates — which is exactly what NAT
/// traversal means. With a return route present this test would pass while
/// proving only that routing works.
#[test]
fn an_ice_nomination_crosses_a_real_nat() {
    skip_unless_supported!("ice-traversal");

    let dir = std::env::temp_dir().join(format!("oxutrm-netns-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let cli_addr = dir.join("cli.addr");
    let peer_log = dir.join("peer.log");
    let _ = std::fs::remove_file(&cli_addr);

    let psk = "5a".repeat(32);
    let bin = helper_binary();
    // The peer learns the client's address only from what the client publishes,
    // which is the job SSH signalling does in the real product.
    let peer_cmd = format!(
        "{bin} ice --role controlled --bind 10.0.2.2:45000 --remote-file {} --psk {psk}",
        cli_addr.display()
    );

    let out = in_topology_with_peer(
        "cone",
        &[
            &bin,
            "ice",
            "--role",
            "controlling",
            "--bind",
            "0.0.0.0:45001",
            "--stun",
            "10.0.2.2:3478,10.0.2.3:3478",
            "--publish",
            cli_addr.to_str().expect("utf-8 path"),
            "--remote",
            "10.0.2.2:45000",
            "--psk",
            &psk,
        ],
        Some(&peer_cmd),
        peer_log.to_str(),
    );
    let out = match out {
        Ok(t) => t,
        Err(t) => panic!("traversal failed:\n{t}"),
    };
    let peer_side = std::fs::read_to_string(&peer_log).unwrap_or_default();
    eprintln!("--- client ---\n{out}\n--- peer ---\n{peer_side}");
    let _ = std::fs::remove_dir_all(&dir);

    // The client nominated.
    assert_eq!(
        field(&out, "remote"),
        Some("10.0.2.2:45000"),
        "the client did not nominate the far peer"
    );
    // And so did the peer, through the NAT.
    assert!(
        peer_side.contains("nominated"),
        "the far peer never nominated, so nothing crossed the NAT"
    );

    // The decisive assertion: the address the peer reached us on is the NAT's
    // OUTSIDE address, not our private one. If this ever reads 10.0.1.x the
    // topology has a route it should not have and the NAT was bypassed.
    let peer_remote = field(&peer_side, "remote").expect("the peer printed no remote=");
    assert!(
        peer_remote.starts_with("10.0.2.1:"),
        "the peer reached a private address ({peer_remote}), so the NAT was \
         bypassed rather than traversed"
    );
}

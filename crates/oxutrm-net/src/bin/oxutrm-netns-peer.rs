//! Test helper: runs one oxutrm network role inside a network namespace.
//!
//! The netns harness cannot call library functions directly — each role has to
//! run as a process inside its own namespace — so this binary is the bridge.
//! It exists only for `tests/netns.rs` and is never installed.
//!
//! Every role prints one machine-readable line to stdout so the harness can
//! assert on it, and nothing else.

use std::net::SocketAddr;

use anyhow::Context;
use oxutrm_net::{IceAgent, IceEvent, IceRole, MappingBehaviour, NetConfig, StunResponder};

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let role = args.first().map(String::as_str).unwrap_or("");

    match role {
        // A STUN server on the simulated internet segment.
        "stun" => {
            let bind: SocketAddr = arg(&args, "--bind")
                .context("stun needs --bind")?
                .parse()
                .context("--bind is not an address")?;
            let responder = StunResponder::start_on(bind, MappingBehaviour::Truthful).await?;
            println!("ready addr={}", responder.addr());
            // Held open until the harness kills us.
            std::future::pending::<()>().await;
            Ok(())
        }

        // Discover our mapped address and classify the NAT in front of us.
        "discover" => {
            let servers: Vec<String> = arg(&args, "--stun")
                .context("discover needs --stun")?
                .split(',')
                .map(str::to_owned)
                .collect();
            let cfg = NetConfig {
                stun_servers: servers,
                gather_timeout: std::time::Duration::from_secs(3),
                ..NetConfig::default()
            };
            let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
            let local = socket.local_addr()?;
            let (candidates, nat) = oxutrm_net::stun_discover(&socket, &cfg).await;
            let mapped = candidates
                .first()
                .map(|c| c.addr.to_string())
                .unwrap_or_else(|| "none".to_string());
            println!(
                "discover local={local} mapped={mapped} nat={nat:?} candidates={}",
                candidates.len()
            );
            Ok(())
        }

        // Run ICE against a peer whose address we were told, then prove the
        // path carries a QUIC datagram both ways.
        "ice" => {
            let ice_role = match arg(&args, "--role").as_deref() {
                Some("controlled") => IceRole::Controlled,
                _ => IceRole::Controlling,
            };
            let bind = arg(&args, "--bind").unwrap_or_else(|| "0.0.0.0:0".to_string());
            let psk = psk_from(&arg(&args, "--psk").unwrap_or_else(|| "0".repeat(64)))?;

            let socket = std::sync::Arc::new(tokio::net::UdpSocket::bind(&bind).await?);
            let local = socket.local_addr()?;
            println!("ice-local addr={local}");

            // Discover our own mapped address ON THE ICE SOCKET, because a NAT
            // mapping belongs to one socket and describes no other. This is
            // the address the peer must be given.
            let mut advertised = local;
            if let Some(servers) = arg(&args, "--stun") {
                let cfg = NetConfig {
                    stun_servers: servers.split(',').map(str::to_owned).collect(),
                    gather_timeout: std::time::Duration::from_secs(3),
                    ..NetConfig::default()
                };
                let (cands, nat) = oxutrm_net::stun_discover(&socket, &cfg).await;
                if let Some(c) = cands.first() {
                    advertised = c.addr;
                }
                println!("mapped addr={advertised} nat={nat:?}");
            }
            // Publish it the way SSH signalling would in the real product.
            if let Some(path) = arg(&args, "--publish") {
                std::fs::write(&path, advertised.to_string())
                    .with_context(|| format!("publishing to {path}"))?;
            }

            // And learn the peer's, either directly or from its published file.
            let remote: SocketAddr = match arg(&args, "--remote") {
                Some(r) => r.parse().context("--remote is not an address")?,
                None => {
                    let path = arg(&args, "--remote-file")
                        .context("ice needs --remote or --remote-file")?;
                    read_published(&path).await?
                }
            };
            println!("peer addr={remote}");

            let mut agent = IceAgent::new(psk, ice_role, NetConfig::default());
            agent.add_local(oxutrm_proto::Candidate {
                addr: local,
                kind: oxutrm_proto::CandidateKind::Host,
                priority: oxutrm_net::ice_priority(oxutrm_proto::CandidateKind::Host, &local.ip()),
            });
            agent.add_remote(oxutrm_proto::Candidate {
                addr: remote,
                kind: oxutrm_proto::CandidateKind::Host,
                priority: oxutrm_net::ice_priority(oxutrm_proto::CandidateKind::Host, &remote.ip()),
            });

            for _ in 0..8 {
                match agent.run(socket.clone()).await {
                    IceEvent::Nominated {
                        local,
                        remote,
                        rung,
                        probes,
                    } => {
                        println!(
                            "nominated local={local} remote={remote} rung={rung:?} probes={probes}"
                        );
                        return Ok(());
                    }
                    IceEvent::Failed(why) => {
                        println!("failed reason={why}");
                        std::process::exit(1);
                    }
                    IceEvent::NewLocalCandidate(c) => {
                        println!("reflexive addr={}", c.addr);
                    }
                }
            }
            println!("failed reason=no terminal event");
            std::process::exit(1);
        }

        // Ask TWO different server IPs, from ONE socket, what address they
        // saw. This asserts the PREMISE of NAT typing rather than its
        // conclusion: a symmetric NAT allocates a different external port per
        // destination, and a cone reuses one.
        "probe-pair" => {
            let a: SocketAddr = arg(&args, "--stun")
                .context("probe-pair needs --stun")?
                .parse()?;
            let b: SocketAddr = arg(&args, "--stun2")
                .context("probe-pair needs --stun2")?
                .parse()?;
            let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
            let one = query_one(&socket, a).await;
            let two = query_one(&socket, b).await;
            match (one, two) {
                (Some(x), Some(y)) => println!(
                    "probe-pair local={} first={x} second={y} same_port={}",
                    socket.local_addr()?,
                    x.port() == y.port()
                ),
                _ => println!(
                    "probe-pair local={} first=none second=none",
                    socket.local_addr()?
                ),
            }
            Ok(())
        }

        // Rung 3, through whatever NAT is in front of us.
        "blast" => {
            let base: SocketAddr = arg(&args, "--peer")
                .context("blast needs --peer")?
                .parse()?;
            let psk = psk_from(&arg(&args, "--psk").unwrap_or_else(|| "0".repeat(64)))?;
            let cfg = NetConfig {
                birthday_sockets: arg(&args, "--sockets")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(4),
                birthday_ports: arg(&args, "--ports")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(32),
                birthday_budget: std::time::Duration::from_secs(8),
                ..NetConfig::default()
            };
            match oxutrm_net::birthday_blast(psk, IceRole::Controlling, base, &cfg).await? {
                Some(r) => println!("blast found remote={} probes={}", r.remote, r.probes),
                None => {
                    println!("blast none");
                    std::process::exit(1);
                }
            }
            Ok(())
        }

        other => anyhow::bail!("unknown role {other:?}"),
    }
}

/// One STUN Binding query, reporting the address that server saw.
async fn query_one(socket: &tokio::net::UdpSocket, server: SocketAddr) -> Option<SocketAddr> {
    let mut client = stunclient::StunClient::new(server);
    client.set_timeout(std::time::Duration::from_secs(2));
    client.query_external_address_async(socket).await.ok()
}

/// Wait for the peer to publish its address. Stands in for the SSH signalling
/// channel, which is what carries candidates in the real product.
async fn read_published(path: &str) -> anyhow::Result<SocketAddr> {
    for _ in 0..300 {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(addr) = text.trim().parse()
        {
            return Ok(addr);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("the peer never published an address to {path}")
}

fn psk_from(hex: &str) -> anyhow::Result<[u8; 32]> {
    anyhow::ensure!(hex.len() == 64, "a psk is 64 hex characters");
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).context("psk is not hex")?;
    }
    Ok(out)
}

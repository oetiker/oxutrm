//! Throwaway measuring tool for one design decision: what does it cost to keep
//! a QUIC path warm, and how long do real NATs keep an idle UDP mapping open?
//!
//! It is an example, so it is never built by `cargo build`, never installed and
//! never shipped. It carries nothing but counters and prints no addresses, so
//! its output can be pasted into a public issue.
//!
//! # `idle`: what does a quiet connection cost?
//!
//! ```text
//! path-cost idle --secs 60 [--ping-hz 0,0.2,0.5,1,2]
//! ```
//!
//! For each ping rate it builds a real oxutrm QUIC pair on loopback, using
//! `quic_server` and `quic_client` and therefore the shipped transport config
//! (10 s keepalive). The connection is otherwise idle: the only traffic is
//! 16-byte QUIC datagram pings sent by the client and echoed by the server.
//! The UDP counters from `Connection::stats()` are read on both ends before and
//! after (starting 3 s after the handshake, so its tail is not counted), and each line reports datagrams per second and bytes per second in
//! each direction, as UDP payload and as on-wire bytes (28 B of IPv4 or 48 B
//! of IPv6 header per datagram). The 0 Hz line is the keepalive alone.
//!
//! The keepalive interval is deliberately not a knob here: its cost is linear
//! in its rate, so the 10 s figure scales. The ping rates stand in for the
//! measurement traffic of a later stage.
//!
//! # `hold`: how long does a NAT keep an idle mapping?
//!
//! ```text
//! path-cost hold --role listen|send --bind ADDR --peer ADDR --holds 15,30,60,120,240,480
//! ```
//!
//! This measures the NAT in front of the **listener**. Run it on two machines
//! that are each behind a NAT, with `--bind` the local address (its port is the
//! base port) and `--peer` the other side's public address and base port. One
//! UDP socket is used per hold time, on ports `bind.port + i`, so the peer
//! address must be reachable at `peer.port + i` too (a port-preserving NAT
//! or a port range forwarded to the machine is needed).
//!
//! 1. Both ends punch: they send a datagram every 200 ms until they hear the
//!    other side, then five more.
//! 2. The listener then goes silent after sending one `silent` datagram.
//! 3. The sender, on hearing `silent` on socket `i`, waits `holds[i]` seconds
//!    and sends three datagrams 100 ms apart.
//! 4. The listener prints `hold 60s: arrived` if one of them got through, and
//!    `hold 60s: lost` if nothing came within `holds[i] + 10 s`.
//!
//! Start the listener first, then the sender, within a few seconds of each
//! other. Run it a second time with the roles swapped to measure the other
//! NAT.
//!
//! ```text
//! machine A$ path-cost hold --role listen --bind 0.0.0.0:47000 --peer <B>:48000
//! machine B$ path-cost hold --role send   --bind 0.0.0.0:48000 --peer <A>:47000
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use oxutrm_net::{generate_cert, quic_client, quic_server};
use oxutrm_proto::{ClientSpki, HostSpki};
use tokio::net::UdpSocket;
use tokio::time::{Instant, interval, sleep, timeout};

/// UDP + IP header bytes added to every datagram on the wire.
const HEADER_V4: u64 = 28;
const HEADER_V6: u64 = 48;

/// Quiet time after the handshake before counting starts.
const SETTLE: Duration = Duration::from_secs(3);

/// The size of one measurement ping.
const PING_LEN: usize = 16;

/// How often an end of `hold` sends a punch datagram while opening the path.
const PUNCH_EVERY: Duration = Duration::from_millis(200);

/// How many punches follow the first one heard from the peer.
const PUNCHES_AFTER_HEARD: usize = 5;

/// Slack the listener waits beyond a hold time before calling it lost.
const HOLD_SLACK: Duration = Duration::from_secs(10);

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn list<T: std::str::FromStr>(raw: &str, what: &str) -> anyhow::Result<Vec<T>> {
    raw.split(',')
        .map(|s| {
            s.trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("{what}: cannot parse {s:?}"))
        })
        .collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str).unwrap_or("") {
        "idle" => {
            let secs: u64 = arg(&args, "--secs")
                .unwrap_or_else(|| "60".into())
                .parse()
                .context("--secs is not a number")?;
            let rates: Vec<f64> = list(
                &arg(&args, "--ping-hz").unwrap_or_else(|| "0,0.2,0.5,1,2".into()),
                "--ping-hz",
            )?;
            idle(secs, &rates).await
        }
        "hold" => {
            let role = arg(&args, "--role").context("hold needs --role listen|send")?;
            let listen = match role.as_str() {
                "listen" => true,
                "send" => false,
                other => bail!("--role must be listen or send, not {other:?}"),
            };
            let bind: SocketAddr = arg(&args, "--bind")
                .context("hold needs --bind")?
                .parse()
                .context("--bind is not an address")?;
            let peer: SocketAddr = arg(&args, "--peer")
                .context("hold needs --peer")?
                .parse()
                .context("--peer is not an address")?;
            let holds: Vec<u64> = list(
                &arg(&args, "--holds").unwrap_or_else(|| "15,30,60,120,240,480".into()),
                "--holds",
            )?;
            hold(listen, bind, peer, &holds).await
        }
        _ => bail!("usage: path-cost idle|hold ... (see the module documentation)"),
    }
}

/// UDP counters of one connection end, as a difference between two readings.
#[derive(Clone, Copy)]
struct Counters {
    tx_bytes: u64,
    tx_dgrams: u64,
}

fn read(conn: &quinn::Connection) -> Counters {
    let s = conn.stats();
    Counters {
        tx_bytes: s.udp_tx.bytes,
        tx_dgrams: s.udp_tx.datagrams,
    }
}

/// One direction of one line: `payload` is UDP payload, `wire` adds headers.
fn direction(name: &str, dgrams: u64, bytes: u64, secs: f64) -> String {
    let per = |n: u64| n as f64 / secs;
    format!(
        "{name} {:.2} dgram/s, {:.1} B/s payload, {:.1} B/s on wire v4, {:.1} B/s on wire v6",
        per(dgrams),
        per(bytes),
        per(bytes + dgrams * HEADER_V4),
        per(bytes + dgrams * HEADER_V6),
    )
}

async fn idle(secs: u64, rates: &[f64]) -> anyhow::Result<()> {
    for &hz in rates {
        let (cert, key, host_fp) = generate_cert()?;
        let (ccert, ckey, client_fp) = generate_cert()?;
        let server_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let server_addr = server_sock.local_addr()?;
        let (endpoint, _permit, _stun) =
            quic_server(&server_sock, cert, key, ClientSpki::new(client_fp)).await?;

        // The server accepts one connection and echoes every datagram.
        let server = tokio::spawn(async move {
            let incoming = endpoint.accept().await.context("endpoint closed")?;
            let conn = incoming.await?;
            let echo = conn.clone();
            tokio::spawn(async move {
                while let Ok(d) = echo.read_datagram().await {
                    if echo.send_datagram(d).is_err() {
                        break;
                    }
                }
            });
            anyhow::Ok(conn)
        });

        let client_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await?);
        let (client, _ep, _stun) = quic_client(
            &client_sock,
            server_addr,
            HostSpki::new(host_fp),
            ccert,
            ckey,
        )
        .await?;
        let server = server.await??;

        // Drain the echoes so the receive queue does not hold them.
        let drain = client.clone();
        let drainer = tokio::spawn(async move { while drain.read_datagram().await.is_ok() {} });

        // Let the handshake's tail (tokens, acks, path validation) drain, so
        // the window below sees steady-state traffic only.
        sleep(SETTLE).await;
        let (c0, s0) = (read(&client), read(&server));
        let start = Instant::now();
        let deadline = start + Duration::from_secs(secs);
        if hz > 0.0 {
            let mut tick = interval(Duration::from_secs_f64(1.0 / hz));
            while Instant::now() < deadline {
                tick.tick().await;
                client.send_datagram(vec![0u8; PING_LEN].into())?;
            }
        } else {
            sleep(deadline - Instant::now()).await;
        }
        let elapsed = start.elapsed().as_secs_f64();
        let (c1, s1) = (read(&client), read(&server));
        drainer.abort();

        println!(
            "ping {hz} Hz over {secs}s: {}; {}",
            direction(
                "client->server",
                c1.tx_dgrams - c0.tx_dgrams,
                c1.tx_bytes - c0.tx_bytes,
                elapsed
            ),
            direction(
                "server->client",
                s1.tx_dgrams - s0.tx_dgrams,
                s1.tx_bytes - s0.tx_bytes,
                elapsed
            ),
        );
        client.close(0u32.into(), b"done");
    }
    Ok(())
}

async fn hold(
    listen: bool,
    bind: SocketAddr,
    peer: SocketAddr,
    holds: &[u64],
) -> anyhow::Result<()> {
    let mut tasks = Vec::new();
    for (i, &secs) in holds.iter().enumerate() {
        let off = u16::try_from(i)?;
        let mut local = bind;
        local.set_port(
            bind.port()
                .checked_add(off)
                .context("port range overflows")?,
        );
        let mut remote = peer;
        remote.set_port(
            peer.port()
                .checked_add(off)
                .context("port range overflows")?,
        );
        let sock = UdpSocket::bind(local)
            .await
            .with_context(|| format!("binding the socket for hold {secs}s"))?;
        tasks.push(tokio::spawn(hold_one(listen, sock, remote, secs)));
    }
    for t in tasks {
        t.await??;
    }
    Ok(())
}

/// Open the path, then run one hold measurement on this socket.
async fn hold_one(
    listen: bool,
    sock: UdpSocket,
    remote: SocketAddr,
    secs: u64,
) -> anyhow::Result<()> {
    // Punch until the peer is heard, then five more times.
    let mut tick = interval(PUNCH_EVERY);
    let mut buf = [0u8; 64];
    let mut heard = false;
    // The listener can finish punching first, so its `silent` may arrive while
    // this end is still punching; it must not be mistaken for noise.
    let mut silent = false;
    let mut left = PUNCHES_AFTER_HEARD;
    while !heard || left > 0 {
        tokio::select! {
            _ = tick.tick() => {
                sock.send_to(b"punch", remote).await?;
                if heard {
                    left -= 1;
                }
            }
            got = sock.recv_from(&mut buf) => {
                let (n, _) = got?;
                match &buf[..n] {
                    b"punch" => heard = true,
                    b"silent" => {
                        heard = true;
                        silent = true;
                    }
                    _ => {}
                }
            }
        }
    }

    if listen {
        sock.send_to(b"silent", remote).await?;
        // Silent from here: only listen, ignoring leftover punches.
        let wait = Duration::from_secs(secs) + HOLD_SLACK;
        let arrived = timeout(wait, async {
            loop {
                let (n, _) = sock.recv_from(&mut buf).await?;
                if &buf[..n] == b"late" {
                    return anyhow::Ok(());
                }
            }
        })
        .await;
        match arrived {
            Ok(res) => {
                res?;
                println!("hold {secs}s: arrived");
            }
            Err(_) => println!("hold {secs}s: lost"),
        }
    } else {
        // Wait for the listener to go silent, then sit idle for the hold time.
        while !silent {
            let (n, _) = sock.recv_from(&mut buf).await?;
            silent = &buf[..n] == b"silent";
        }
        sleep(Duration::from_secs(secs)).await;
        for _ in 0..3 {
            sock.send_to(b"late", remote).await?;
            sleep(Duration::from_millis(100)).await;
        }
    }
    Ok(())
}

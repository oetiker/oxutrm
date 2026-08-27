//! `oxutrm <ssh-target>`: the local half, from ssh to a painted screen.

use std::sync::Arc;

use anyhow::{Context as _, Result};

use oxutrm_client::{RawGuard, terminal_size};
use oxutrm_host::ssh::{SshChannel, SshLauncher};
use oxutrm_net::{IceRole, NetConfig};
use oxutrm_proto::{
    Candidate, ClientSpki, HostSpki, NatType, PROTO_VERSION, PathDescription, Psk, Signal,
};
use oxutrm_term::detect_caps;

use crate::candidates::{inbound_candidates, outbound_candidates};
use crate::ladder::nominate;
use crate::link::Link;
use crate::session::ClientSession;

/// `oxutrm <ssh-target>`: L1 to L14.
///
/// L1 and L2, and then the whole of it inside one runtime. **The terminal is
/// deliberately not in raw mode here.** `ssh` may still have to ask for a
/// passphrase or a host-key confirmation, and raw mode would corrupt the
/// prompt it asks with; [`RawGuard`] goes on at L11, after every prompt ssh
/// could possibly have shown.
pub fn run_connect(args: &[String]) -> Result<()> {
    let Some(target) = args.first() else {
        anyhow::bail!("oxutrm needs an ssh target. Try `oxutrm --help`.");
    };

    // L2. The local side never forks, so a runtime here is free of the
    // constraint that shapes `host --serve`.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the runtime")?;
    let outcome = runtime.block_on(connect(target));

    // Same reasoning as `host --serve`: the ssh channel's reader may be parked
    // on a pipe the far end is in no hurry to close, and waiting for a read we
    // have stopped caring about is not a shutdown.
    runtime.shutdown_background();

    let code = outcome?;
    // L14. The guard is already gone by here -- see `connect` -- so this line
    // lands on a terminal that has been given back to the user.
    println!("oxutrm: the shell exited ({code}).");
    std::process::exit(code);
}

/// L3 to L13.
async fn connect(target: &str) -> Result<i32> {
    let cfg = NetConfig::default();

    // L3. Spawns `ssh <target> oxutrm host --serve` and drains its stderr
    // continuously -- an undrained stderr is a deadlock, not an inconvenience.
    let mut channel = SshChannel::open(&SshLauncher::ssh(), target)
        .await
        .with_context(|| format!("starting a session on {target}"))?;

    // L4. One socket for STUN, ICE and QUIC.
    let bound = oxutrm_net::bind_socket(&cfg).context("binding a UDP socket")?;
    let mut candidates = oxutrm_net::local_candidates(&bound);
    let socket = crate::ladder::adopt(bound).context("handing the socket to the runtime")?;

    // L5. Banner and motd are skipped inside `recv`; version skew fails loudly.
    let host = host_facts(channel.recv().await.context("reading the host's offer")?)?;

    // L6.
    let (reflexive, nat) = oxutrm_net::stun_discover(&socket, &cfg).await;
    candidates.extend(reflexive);

    // L7. Our own throwaway certificate, whose fingerprint the host pins in
    // its `ClientCertVerifier`. Without it the PSK would be the only thing
    // gating the punched socket, and the PSK never reaches TLS.
    let (cert, key, our_spki) =
        oxutrm_net::generate_cert().context("generating this attach's certificate")?;
    let size = terminal_size().context("oxutrm needs a real terminal to connect from")?;
    channel
        .send(&Signal::ClientHello {
            proto: PROTO_VERSION,
            cert_spki_sha256: ClientSpki::new(our_spki),
            candidates: candidates.clone(),
            nat_type: nat,
            caps: detect_caps(),
            size,
        })
        .await
        .context("answering the host's offer")?;

    // L8. This side is `Controlling`: only the controlling side nominates.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel(32);
    let (learned_tx, mut learned_rx) = tokio::sync::mpsc::channel(32);
    let (reader, writer) = channel.halves();

    // Pinned rather than spawned, and NOT cancelled when the ladder finishes:
    // this same future goes on to deliver `Established` at L10. Cancelling a
    // `read_line` that had already buffered part of a line would eat those
    // bytes, and the next read would start mid-message.
    let mut from_host = std::pin::pin!(inbound_candidates(reader, &in_tx));

    let nomination = {
        let race = async {
            let outcome = nominate(
                Arc::clone(&socket),
                crate::ladder::Ladder {
                    psk: &host.psk,
                    role: IceRole::Controlling,
                    nat,
                    cfg: &cfg,
                    local: candidates,
                    remote: host.candidates,
                },
                &mut in_rx,
                &learned_tx,
            )
            .await;
            // Closing the channel is what lets the outbound pump *finish*
            // rather than be cancelled, so no truncated `CandidateUpdate` can
            // sit in front of whatever we write next.
            drop(learned_tx);
            outcome
        };
        let to_host = outbound_candidates(writer, &mut learned_rx);

        tokio::select! {
            (raced, sent) = async { tokio::join!(race, to_host) } => {
                sent.context("sending our candidates to the host")?;
                raced
            }
            // The host can give up while we are still racing -- its own ladder
            // may have run out of rungs first. Its reason is better than ours.
            early = &mut from_host => {
                return Err(established_path(early.context("reading from the host")?)
                    .expect_err("the host cannot declare a path before we nominated one"));
            }
        }
    };

    let nomination = nomination.map_err(|report| {
        anyhow::Error::new(report).context("no rung of the ladder reached the host")
    })?;

    // L9. The remote address is fixed here for the whole attach: QUIC
    // migration is local-address only, so a better path found later belongs to
    // the next attach and not to this one.
    let (connection, endpoint, _stun_rx) = oxutrm_net::quic_client(
        &nomination.socket,
        nomination.remote,
        host.host_spki,
        cert,
        key,
    )
    .await
    .context("bringing up QUIC over the nominated path")?;

    // L10. The last signalling message. After this nothing reads the ssh
    // channel again: the host is about to sever, and the EOF that follows is
    // expected rather than an error.
    let path = established_path(from_host.await.context("waiting for the host's verdict")?)?;

    // L11. Late, deliberately: after every prompt ssh could have shown, and
    // after the last thing that could have failed with a message worth
    // reading on an ordinary terminal.
    let raw = RawGuard::enter().context("putting the terminal into raw mode")?;

    let mut session = ClientSession::new(
        size,
        detect_caps(),
        Link::new(connection, endpoint, nomination.socket),
    )
    .context("preparing the client session")?;

    // L12. One line, and then silence.
    let mut stdout = std::io::stdout();
    session
        .announce(&path, &mut stdout)
        .context("announcing the path")?;

    // L13.
    let code = session.run(&mut stdout).await;

    // L14's first half. The guard comes off before anything a human should
    // read is printed, and before the error below is rendered -- a backtrace
    // on a terminal still in raw mode climbs diagonally down the screen.
    drop(raw);
    code
}

/// What the host offered, once its hello arrived.
struct HostFacts {
    psk: Psk,
    /// The fingerprint the client pins. [`HostSpki`] and not the bare
    /// encoding: `ClientHello` carries a field of the same name and shape
    /// pointing the other way, and while both were `String` the swap
    /// type-checked.
    host_spki: HostSpki,
    candidates: Vec<Candidate>,
    nat: NatType,
}

impl std::fmt::Debug for HostFacts {
    /// Hand-written, and the `psk` is not in it.
    ///
    /// [`Psk`]'s own `Debug` redacts, so a derived one would be safe *today*.
    /// It would also be a standing invitation: the next field added here gets
    /// printed automatically, and the one thing this struct must never print
    /// is already inside it. Naming the three fields that may be shown is the
    /// version that cannot drift.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostFacts")
            .field("host_spki", &self.host_spki)
            .field("candidates", &self.candidates)
            .field("nat", &self.nat)
            .finish_non_exhaustive()
    }
}

/// L5: read the host's offer, or say why there is not going to be one.
fn host_facts(signal: Signal) -> Result<HostFacts> {
    match signal {
        Signal::HostHello {
            psk,
            cert_spki_sha256,
            candidates,
            nat_type,
            ..
        } => Ok(HostFacts {
            psk,
            host_spki: cert_spki_sha256,
            candidates,
            nat: nat_type,
        }),
        // The host's own words. It is the only explanation there is for why
        // this connection is not going to happen, and it is the sentence the
        // user is looking at.
        Signal::Failed { reason } => Err(anyhow::anyhow!("the host gave up: {reason}")),
        other => Err(anyhow::anyhow!(
            "the host opened with {other:?} instead of its hello"
        )),
    }
}

/// L10: the host's last signalling message, which is where the path comes from.
///
/// The path is the **host's** description, not one computed here. Only the host
/// has both live numbers at that moment, and two ends deriving a status line
/// separately is two status lines that can disagree.
fn established_path(signal: Signal) -> Result<PathDescription> {
    match signal {
        Signal::Established { path } => Ok(path),
        Signal::Failed { reason } => Err(anyhow::anyhow!("the host gave up: {reason}")),
        other => Err(anyhow::anyhow!(
            "the host sent {other:?} where the link should have been declared up"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::{CandidateKind, Rung};

    fn a_host_hello() -> Signal {
        Signal::HostHello {
            proto: PROTO_VERSION,
            session_id: "f00d".to_owned(),
            attach_id: 3,
            cert_spki_sha256: HostSpki::new([1u8; 32]),
            psk: Psk::new([2u8; 32]),
            candidates: vec![Candidate {
                addr: "203.0.113.4:5000".parse().expect("a test address"),
                kind: CandidateKind::ServerReflexive,
                priority: 7,
            }],
            nat_type: NatType::AddressDependent,
            bound_port: 5000,
            detachable: true,
        }
    }

    #[test]
    fn the_offer_yields_the_key_material_and_the_peers_candidates() {
        let facts = host_facts(a_host_hello()).expect("a hello is an offer");
        assert_eq!(facts.host_spki, HostSpki::new([1u8; 32]));
        assert_eq!(facts.nat, NatType::AddressDependent);
        assert_eq!(facts.candidates.len(), 1);
    }

    /// A host that gives up says why, and the reason is the only explanation
    /// anybody has. Reporting "unexpected message" would throw it away — and
    /// this is the reason the user is staring at, so it has to survive.
    #[test]
    fn a_host_that_gives_up_is_reported_with_its_own_reason() {
        let error = host_facts(Signal::Failed {
            reason: "no usable path to the host".to_owned(),
        })
        .expect_err("a refusal is not an offer");
        assert!(
            format!("{error:#}").contains("no usable path to the host"),
            "the host's own reason was thrown away: {error:#}"
        );
    }

    /// Anything else is a protocol error and must not be mistaken for one.
    #[test]
    fn a_message_that_is_not_an_offer_is_not_treated_as_one() {
        let error = host_facts(Signal::CandidateUpdate { candidates: vec![] })
            .expect_err("an update is not an offer");
        assert!(format!("{error:#}").contains("CandidateUpdate"));
    }

    #[test]
    fn the_path_comes_from_the_hosts_established() {
        let path = PathDescription {
            rung: Rung::StunPunch,
            local: "192.0.2.1:1".parse().expect("a test address"),
            remote: "198.51.100.1:2".parse().expect("a test address"),
            probes_sent: 4,
            nat_type: NatType::EndpointIndependent,
            rtt_ms: 11,
            mtu: 1452,
        };
        let got = established_path(Signal::Established { path: path.clone() })
            .expect("an Established carries the path");
        assert_eq!(got.rung, path.rung);
        assert_eq!(got.mtu, 1452);
    }

    /// The host can still give up *after* our ladder nominated — its own may
    /// not have, or its accept may have timed out. That reason is the user's
    /// only clue and must not become "unexpected message" either.
    #[test]
    fn a_host_that_gives_up_after_nomination_is_still_reported_with_its_reason() {
        let error = established_path(Signal::Failed {
            reason: "no client completed a QUIC handshake".to_owned(),
        })
        .expect_err("a refusal is not an established link");
        assert!(
            format!("{error:#}").contains("no client completed a QUIC handshake"),
            "the host's own reason was thrown away: {error:#}"
        );
    }
}

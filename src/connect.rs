//! `oxutrm <ssh-target>`: the local half, from ssh to a painted screen.

use std::sync::Arc;

use anyhow::{Context as _, Result};

use oxutrm_client::{RawGuard, terminal_size};
use oxutrm_host::signalling::{read_signal_async, write_signal_async};
use oxutrm_host::ssh::{SshChannel, SshLauncher};
use oxutrm_net::{IceRole, NetConfig};
use oxutrm_proto::{
    Candidate, ClientSpki, HostSpki, NatType, PROTO_VERSION, PathDescription, Psk, SessionSummary,
    Signal, TermSize,
};
use oxutrm_term::detect_caps;

use crate::candidates::{inbound_candidates, outbound_candidates};
use crate::choose::{Decision, decide, pick};
use crate::ladder::nominate;
use crate::link::Link;
use crate::rebuild::Rebuild;
use crate::session::ClientSession;

/// `oxutrm <ssh-target>`: L1 to L14.
///
/// L1 and L2, and then the whole of it inside one runtime. **The terminal is
/// deliberately not in raw mode here.** `ssh` may still have to ask for a
/// passphrase or a host-key confirmation, and raw mode would corrupt the
/// prompt it asks with; [`RawGuard`] goes on at L11, after every prompt ssh
/// could possibly have shown.
pub fn run_connect(args: &[String]) -> Result<()> {
    // `--attach <id>` and `--new` come before the target, in either order.
    //
    // Every usage mistake here exits 2, the same convention `dispatch`'s own
    // "unknown option" and `run_host`'s missing-subcommand cases use: a typo
    // in the invocation is not a connection failure, and it is reported and
    // exited before anything -- ssh included -- has been started.
    let mut attach: Option<String> = None;
    let mut new = false;
    let mut rest = args;
    loop {
        match rest.first().map(String::as_str) {
            Some("--attach") => {
                let Some(id) = rest.get(1) else {
                    eprintln!("oxutrm: --attach needs a session id. Try `oxutrm --help`.");
                    std::process::exit(2);
                };
                attach = Some(id.clone());
                rest = &rest[2..];
            }
            Some("--new") => {
                new = true;
                rest = &rest[1..];
            }
            _ => break,
        }
    }

    if attach.is_some() && new {
        eprintln!("oxutrm: --attach and --new cannot both be given. Try `oxutrm --help`.");
        std::process::exit(2);
    }

    let Some(target) = rest.first() else {
        eprintln!("oxutrm: needs an ssh target. Try `oxutrm --help`.");
        std::process::exit(2);
    };

    // L2. The local side never forks, so a runtime here is free of the
    // constraint that shapes `host --serve`.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the runtime")?;
    let outcome = runtime.block_on(connect(target, attach.as_deref(), new));

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
async fn connect(target: &str, attach: Option<&str>, new: bool) -> Result<i32> {
    let cfg = NetConfig::default();

    // L3. Spawns `ssh <target> oxutrm host --connect` and drains its stderr
    // continuously -- an undrained stderr is a deadlock, not an inconvenience.
    let mut channel = SshChannel::open(&SshLauncher::ssh(), target)
        .await
        .with_context(|| format!("starting a session on {target}"))?;

    // The offer, decided against `--attach`/`--new`/nothing (`choose::decide`)
    // before anything else touches the terminal.
    //
    // This first read is also where a far end that is missing, too old, or
    // drowned in a chatty rc file is diagnosed: `SshChannel::recv` reaps the
    // child and turns an EOF into `RemoteBinaryMissing`, `SshFailed` or
    // `NoSignal`. `establish` reads a plain stream and cannot do that, which
    // is exactly why the offer is read here and not inside it.
    let offered = read_offer(&mut channel).await?;
    let choice = match decide(&offered, attach, new) {
        Decision::Chosen(choice) => choice,
        // Before raw mode: this is an ordinary line on an ordinary terminal,
        // the same as any other reason `connect` cannot go on.
        Decision::Refused(why) => {
            eprintln!("oxutrm: {why}");
            std::process::exit(2);
        }
        // More than one session is live and nothing else settled it. The
        // terminal is still ordinary here -- raw mode is L11, well below --
        // so the picker is plain line I/O on stdin and stdout.
        //
        // Run on tokio's blocking pool, not on this async task: `pick` waits
        // on a human with no bound on how long that takes, and `SshChannel`'s
        // own stderr drainer (`oxutrm_host::ssh::SshChannel::open`) is a
        // `tokio::spawn` task on this same multi-threaded runtime. On a
        // single-worker or cgroup-limited host, a human sitting at the `>`
        // prompt would monopolise the only worker, starving that drainer;
        // once its pipe buffer fills, `ssh` itself blocks on the write.
        // `spawn_blocking` keeps the wait off the async workers entirely.
        Decision::Ask => {
            let offered = offered.clone();
            let picked = tokio::task::spawn_blocking(move || {
                pick(
                    &offered,
                    &mut std::io::stdin().lock(),
                    &mut std::io::stdout(),
                )
            })
            .await
            .context("running the picker")??;
            match picked {
                Some(choice) => choice,
                // The user quit. Not an error: they were asked, and declined.
                None => std::process::exit(0),
            }
        }
    };
    channel
        .send(&Signal::Choose { choice })
        .await
        .context("telling the host which session we want")?;

    let size = terminal_size().context("oxutrm needs a real terminal to connect from")?;

    // L4 to L10.
    let (reader, writer) = channel.halves();
    let established = establish(reader, writer, size, &cfg).await?;

    // Before raw mode, on the ordinary terminal: the session id is the one
    // thing a user needs written down to `--attach` back into later, and this
    // is the only moment it is known -- `HostFacts::session_id` names
    // whichever session actually resulted, which for `Choice::New` is a fresh
    // id the client had no way to guess in advance.
    //
    // The attach generation used to be here too and is not any more. It is
    // internal bookkeeping -- which generation of the sync counters this is --
    // and unlike the id there is nothing a user can do with it.
    println!("oxutrm: session {}.", established.session_id);

    // L11. Late, deliberately: after every prompt ssh could have shown, and
    // after the last thing that could have failed with a message worth
    // reading on an ordinary terminal.
    let raw = RawGuard::enter().context("putting the terminal into raw mode")?;

    // The two identities a rebuild needs: the target the user typed, and
    // whichever session actually resulted -- which for `Choice::New` is an id
    // the client had no way to guess in advance.
    let rebuild = Rebuild::new(target.to_owned(), established.session_id.clone());
    let mut session = ClientSession::new(size, detect_caps(), established.link, Some(rebuild))
        .context("preparing the client session")?;

    // L12. The SECOND of the two lines a session opens with, and the last.
    //
    // It used to be the only one, and the comment here used to say so. Both
    // are kept, because they answer different questions and this branch is
    // what made the first one worth asking: a bare `oxutrm <target>` now
    // RESUMES a session rather than always starting one, so which session it
    // picked is exactly the kind of thing §10.3 forbids doing silently. The
    // line above says which; this one says how it is reached. After it,
    // silence -- `announce` prints nothing when called again with the same
    // path, and only a migration makes it speak.
    let mut stdout = std::io::stdout();
    session
        .announce(&established.path, &mut stdout)
        .context("announcing the path")?;

    // L13.
    let code = session.run(&mut stdout).await;

    // L14's first half. The guard comes off before anything a human should
    // read is printed, and before the error below is rendered -- a backtrace
    // on a terminal still in raw mode climbs diagonally down the screen.
    drop(raw);
    code
}

/// One completed client-side attach, and the two identities the rebuild loop
/// needs afterwards.
pub(crate) struct Established {
    pub link: Link,
    pub path: PathDescription,
    /// Kept, not discarded: a rebuild attaches to THIS session by name, and
    /// before this existed `host_facts` threw the id away with `..`. `connect`
    /// prints it to the user as the one thing worth writing down to `--attach`
    /// back into later.
    pub session_id: String,
    /// Which attach generation this is. Both `seq` counters restart at 1 per
    /// attach, so a rebuild has to name the one it is resuming from.
    pub attach_id: u64,
}

/// L4 to L10: one socket, the hello exchange, the ICE ladder and the QUIC
/// handshake, ending the moment the host declares the path up.
///
/// Generic over its signalling stream for the same reason the host's
/// `run_attach_exchange` is: the ssh channel is one caller, and a `duplex`
/// pair in a test is another. Until this was extracted, the only way to
/// exercise a reattach was to shadow the remote binary with a shim.
///
/// `Send` mirrors `run_attach_exchange`, so a caller that wants to spawn this
/// may. There is deliberately no `'static` alongside it: this side *pins* its
/// candidate pumps rather than spawning them (see the comment on `from_host`),
/// so nothing here outlives the call — and `connect` can only ever offer the
/// **borrowed** halves of a live [`SshChannel`], because the channel has to
/// survive the exchange to keep `diagnose` and its stderr buffer.
pub(crate) async fn establish<R, W>(
    reader: R,
    writer: W,
    size: TermSize,
    cfg: &NetConfig,
) -> Result<Established>
where
    R: tokio::io::AsyncBufRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    let mut reader = reader;
    let mut writer = writer;

    // L4. One socket for STUN, ICE and QUIC.
    let bound = oxutrm_net::bind_socket(cfg).context("binding a UDP socket")?;
    let mut candidates = oxutrm_net::local_candidates(&bound);
    let socket = crate::ladder::adopt(bound).context("handing the socket to the runtime")?;

    // L5. Banner and motd are skipped inside `read_signal_async`; version skew
    // fails loudly.
    let host = host_facts(
        read_signal_async(&mut reader)
            .await
            .context("reading the host's offer")?,
    )?;

    // L6.
    let (reflexive, nat) = oxutrm_net::stun_discover(&socket, cfg).await;
    candidates.extend(reflexive);

    // L7. Our own throwaway certificate, whose fingerprint the host pins in
    // its `ClientCertVerifier`. Without it the PSK would be the only thing
    // gating the punched socket, and the PSK never reaches TLS.
    let (cert, key, our_spki) =
        oxutrm_net::generate_cert().context("generating this attach's certificate")?;
    write_signal_async(
        &mut writer,
        &Signal::ClientHello {
            proto: PROTO_VERSION,
            cert_spki_sha256: ClientSpki::new(our_spki),
            candidates: candidates.clone(),
            nat_type: nat,
            caps: detect_caps(),
            size,
        },
    )
    .await
    .context("answering the host's offer")?;

    // L8. This side is `Controlling`: only the controlling side nominates.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel(32);
    let (learned_tx, mut learned_rx) = tokio::sync::mpsc::channel(32);

    // Pinned rather than spawned, and NOT cancelled when the ladder finishes:
    // this same future goes on to deliver `Established` at L10. Cancelling a
    // `read_line` that had already buffered part of a line would eat those
    // bytes, and the next read would start mid-message.
    let mut from_host = std::pin::pin!(inbound_candidates(&mut reader, &in_tx));

    let nomination = {
        let race = async {
            let outcome = nominate(
                Arc::clone(&socket),
                crate::ladder::Ladder {
                    psk: &host.psk,
                    role: IceRole::Controlling,
                    nat,
                    cfg,
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
        let to_host = outbound_candidates(&mut writer, &mut learned_rx);

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

    // L10. The last signalling message. After this nothing reads the
    // signalling stream again: the host is about to sever, and the EOF that
    // follows is expected rather than an error.
    let path = established_path(from_host.await.context("waiting for the host's verdict")?)?;

    Ok(Established {
        link: Link::new(connection, endpoint, nomination.socket),
        path,
        session_id: host.session_id,
        attach_id: host.attach_id,
    })
}

/// The far end refused, in its own words.
///
/// A type rather than a bare `anyhow!` because the rebuild loop has to tell
/// this apart from everything else that can go wrong (design spec §5.4): an
/// answer FROM the host ends the loop, and a transport failure never does.
/// Sniffing for a phrase in a formatted message would work until somebody
/// reworded it, at which point a client would quietly start retrying a session
/// that is gone every eight seconds for ever.
#[derive(Debug)]
pub(crate) struct HostRefused(pub String);

impl std::fmt::Display for HostRefused {
    /// Word for word what this used to be written as, so the messages every
    /// existing caller shows the user are unchanged.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the host gave up: {}", self.0)
    }
}

impl std::error::Error for HostRefused {}

/// The offer, read before either side has committed to a session.
///
/// It arrives ahead of `HostHello` and is the only message that a first
/// connect and a reattach do not share, which is what makes reattach reachable
/// from a bare `oxutrm <target>` without a second code path behind it. An
/// empty list is the ordinary first-connect case and is not an error.
///
/// The read itself stays on the concrete `&mut SshChannel` rather than a
/// generic stream: `SshChannel::recv` is what reaps the child and turns an
/// EOF into a diagnosed `RemoteBinaryMissing` or `SshFailed`, and a generic
/// read cannot do that. What the signal MEANS, once read, is split out into
/// [`sessions_offered`] so that part -- and only that part -- can be driven
/// without ssh in the test below. [`crate::rebuild::attempt`] reads its own
/// fresh channel's offer through this same function, and for the same reason:
/// the first read is where a missing binary or a dead ssh is diagnosed.
pub(crate) async fn read_offer(channel: &mut SshChannel) -> Result<Vec<SessionSummary>> {
    sessions_offered(
        channel
            .recv()
            .await
            .context("reading the host's list of live sessions")?,
    )
}

/// What a `Signal` means as the host's offer, once it has been read.
fn sessions_offered(signal: Signal) -> Result<Vec<SessionSummary>> {
    match signal {
        Signal::Sessions { sessions } => Ok(sessions),
        // The host's own words, exactly as at L5: it is the only explanation
        // there is for why this connection is not going to happen.
        Signal::Failed { reason } => Err(anyhow::Error::new(HostRefused(reason))),
        other => Err(anyhow::anyhow!(
            "the host opened with {other:?} instead of its list of sessions"
        )),
    }
}

/// What the host offered, once its hello arrived.
struct HostFacts {
    /// 32 lowercase hex characters. Kept because a rebuild attaches to THIS
    /// session by name; it used to be discarded by the `..` below.
    session_id: String,
    /// Which attach generation this is. Both `seq` counters restart at 1 per
    /// attach, so a rebuild has to know which one it is resuming from.
    attach_id: u64,
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
    /// is already inside it. Naming the fields that may be shown is the
    /// version that cannot drift.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostFacts")
            .field("session_id", &self.session_id)
            .field("attach_id", &self.attach_id)
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
            session_id,
            attach_id,
            psk,
            cert_spki_sha256,
            candidates,
            nat_type,
            ..
        } => Ok(HostFacts {
            session_id,
            attach_id,
            psk,
            host_spki: cert_spki_sha256,
            candidates,
            nat: nat_type,
        }),
        // The host's own words. It is the only explanation there is for why
        // this connection is not going to happen, and it is the sentence the
        // user is looking at.
        Signal::Failed { reason } => Err(anyhow::Error::new(HostRefused(reason))),
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
        Signal::Failed { reason } => Err(anyhow::Error::new(HostRefused(reason))),
        other => Err(anyhow::anyhow!(
            "the host sent {other:?} where the link should have been declared up"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::{CandidateKind, Choice, Rung};

    /// STUN-free, like every other test in this repo: a test that reaches STUN
    /// makes every timing in it non-deterministic, and an injected bug once
    /// passed because a real gather ate the ordering.
    fn test_config() -> oxutrm_net::NetConfig {
        oxutrm_net::NetConfig {
            stun_servers: vec![],
            enable_port_mapping: false,
            enable_birthday: false,
            ..Default::default()
        }
    }

    /// The offer step itself, end to end: read it, decide, answer it.
    ///
    /// Before this, `connect`'s own offer step had no automated coverage:
    /// `ssh_bootstrap`'s tests drive `SshChannel` directly and never reach the
    /// offer, and the subprocess tests in `tests/client_flags.rs` only reach
    /// the flag parsing in front of it. This drives the CLIENT half the same
    /// way `a_client_and_a_host_complete_an_exchange_over_a_pipe` below drives
    /// `establish`: two ends of a `tokio::io::duplex`, no ssh, no shim, paired
    /// with a task standing in for the host's side of the same exchange.
    ///
    /// It would catch a decode mismatch between what a real host sends and
    /// what `sessions_offered` expects, a `decide` that picks the wrong
    /// `Choice` for what was actually offered, or a `Choice` that fails to
    /// round-trip back onto the wire -- none of which the pure unit tests in
    /// `choose.rs` can see, because none of them touch the wire.
    #[tokio::test]
    async fn the_client_reads_the_offer_decides_and_answers_it() {
        let (host_side, client_side) = tokio::io::duplex(64 * 1024);
        let (host_read, mut host_write) = tokio::io::split(host_side);
        let (client_read, mut client_write) = tokio::io::split(client_side);
        let mut client_read = tokio::io::BufReader::new(client_read);

        let offered = vec![SessionSummary {
            session_id: "a".repeat(32),
            created_unix: 1_757_200_000,
            shell: "/bin/sh".to_owned(),
            size: TermSize { cols: 80, rows: 24 },
            detachable: true,
            attach_id: 1,
        }];

        // Stands in for `oxutrm host --connect`: sends the offer first, then
        // waits for the client's answer.
        let host = tokio::spawn({
            let offered = offered.clone();
            async move {
                write_signal_async(&mut host_write, &Signal::Sessions { sessions: offered })
                    .await
                    .expect("writing the offer");
                match read_signal_async(&mut tokio::io::BufReader::new(host_read))
                    .await
                    .expect("reading the choice")
                {
                    Signal::Choose { choice } => choice,
                    other => panic!("expected Choose, got {other:?}"),
                }
            }
        });

        // The client half `connect` actually runs: read the offer, decide,
        // send the choice back.
        let signal = read_signal_async(&mut client_read)
            .await
            .expect("reading the offer");
        let sessions = sessions_offered(signal).expect("the offer parses");
        let decision = decide(&sessions, None, false);
        let Decision::Chosen(choice) = decision else {
            panic!(
                "with exactly one detachable session and no flags, decide must choose: {decision:?}"
            );
        };
        write_signal_async(
            &mut client_write,
            &Signal::Choose {
                choice: choice.clone(),
            },
        )
        .await
        .expect("sending the choice");

        let got = host.await.expect("the host task joins");
        assert_eq!(
            got, choice,
            "the host must see exactly the choice the client's own decision made"
        );
        assert_eq!(
            choice,
            Choice::Attach { id: "a".repeat(32) },
            "the one live, detachable session must be resumed without asking"
        );
    }

    /// The composition the reattach path has never had a test for.
    ///
    /// Both halves of the handshake are generic over their signalling stream,
    /// so they join through a `tokio::io::duplex` pair: no ssh, no shim on the
    /// remote host, no Unix socket. The UDP path between them is real
    /// loopback, because what is under test is the handshake and not the
    /// transport.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_client_and_a_host_complete_an_exchange_over_a_pipe() {
        use oxutrm_proto::TermSize;

        let (host_side, client_side) = tokio::io::duplex(64 * 1024);
        let (host_read, host_write) = tokio::io::split(host_side);
        let (client_read, client_write) = tokio::io::split(client_side);

        let mut meta = oxutrm_host::SessionMeta {
            session_id: "b".repeat(32),
            attach_id: 0,
            pid: std::process::id(),
            created_unix: 0,
            shell: "/bin/sh".to_owned(),
            size: TermSize { cols: 80, rows: 24 },
            detachable: true,
            boot: oxutrm_host::boot_token(),
        };

        let cfg = test_config();
        let host = tokio::spawn({
            let cfg = cfg.clone();
            async move {
                crate::attach_exchange::run_attach_exchange(
                    tokio::io::BufReader::new(host_read),
                    host_write,
                    &mut meta,
                    &cfg,
                )
                .await
            }
        });

        let client = establish(
            tokio::io::BufReader::new(client_read),
            client_write,
            TermSize {
                cols: 120,
                rows: 40,
            },
            &cfg,
        )
        .await
        .expect("the client exchange completes");

        let attached = host
            .await
            .expect("the host task joins")
            .expect("the host exchange completes");

        // The side effects, not just "it returned Ok":
        assert_eq!(
            client.session_id,
            "b".repeat(32),
            "the client must keep the session id -- the rebuild loop cannot exist without it"
        );
        assert_eq!(
            client.attach_id, 1,
            "begin_attach bumps the generation, and the client must see the bumped one"
        );
        assert_eq!(
            attached.client_size,
            TermSize {
                cols: 120,
                rows: 40
            },
            "the host must carry the client's size out of the exchange, not the size the session already had"
        );
    }

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

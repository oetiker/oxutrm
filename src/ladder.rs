//! The connection ladder's **mechanism**: run the plan, and come back with a
//! socket or with every rung's reason.
//!
//! `oxutrm_host::ladder` decides *which* rungs are worth trying; this decides
//! nothing and tries them. It lives in the root binary because that is the only
//! crate depending on both halves, and because `oxutrm-host` must never see a
//! `quinn::Endpoint` or an `oxutrm_net::` type at all —
//! `crates/oxutrm-host/tests/no_net.rs` machine-checks that edge.
//!
//! # Four properties, and why each one is easy to lose
//!
//! The contract records these next to the rung-4 framing rules, because a
//! `RungRunner` trait that had none of them was written here once and deleted.
//! They are restated where the code is, not only where the contract is.
//!
//! **1. One socket.** Rungs 0 to 2 are not three attempts; they are three
//! candidate *classes* on one socket, and [`oxutrm_net::IceAgent`] already
//! races every pair on it and reports which [`Rung`] the winner belonged to.
//! Racing them as three futures means three concurrent receive loops on one
//! socket stealing each other's datagrams — the failure `StunDemuxSocket`
//! exists to prevent. NAT mappings are per-socket, so this is not tidiness: an
//! address learned on any other socket names a hole our traffic will never
//! emerge from.
//!
//! **2. The nomination returns the SOCKET, not merely an address.** Rung 3
//! punches with a fresh socket and `birthday_blast` hands it back, because the
//! mapping belongs to that socket and no other. [`Nomination`] therefore
//! carries the socket QUIC must adopt, and it is an *output* of the ladder
//! rather than a variable captured before it ran.
//!
//! **3. No MTU before the handshake.** Path MTU is a `quinn` property,
//! discovered after a handshake that is strictly after nomination ends. So
//! [`Nomination`] has no `mtu` field and no place to put a guess; the caller
//! fills `PathDescription::mtu` from the live connection. The deleted code
//! carried one, and a test asserted the fabricated value back.
//!
//! **4. Every rung's reason survives a total failure, and a SKIPPED rung reads
//! differently from a FAILED one.** "Connection failed" is useless. The rung
//! that got closest is the one worth reading, and which rungs were never
//! attempted is the first thing a bug report needs. [`LadderReport`] is that
//! answer, and it is the error type of [`nominate`] rather than a side channel,
//! so there is no way to fail without producing it.
//!
//! # What this module deliberately does not do
//!
//! It does not build a QUIC endpoint, print a status line, or decide
//! detachability. Nomination ends; the caller takes the socket from here.

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;

use oxutrm_host::LadderPlan;
use oxutrm_net::{IceAgent, IceEvent, IceRole, NetConfig, birthday_blast};
use oxutrm_proto::{Candidate, CandidateKind, NatType, Psk, Rung};

/// The path the ladder settled on, and the socket it belongs to.
///
/// **No `mtu`.** See property 3 in the module docs: at this point the number
/// does not exist yet, and a field for it can only hold a guess that renders as
/// a plausible status line.
#[derive(Debug)]
pub struct Nomination {
    /// The socket QUIC must adopt — property 2. For rungs 0 to 2 this is the
    /// very socket handed to [`nominate`]; for rung 3 it is the one the blast
    /// punched with, and the original is now useless.
    pub socket: Arc<tokio::net::UdpSocket>,
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub rung: Rung,
    /// Every check this attach sent, across all rungs, not only the winning
    /// one. The blast's guardrail is that its cost is reported rather than
    /// hidden, and a count that dropped the race before it would understate
    /// what the connection actually cost.
    pub probes: u32,
}

/// What became of one rung.
///
/// `Skipped` and `Failed` are distinct variants rather than two strings
/// because property 4 is precisely that they must not read alike: skipped means
/// a decision was taken in advance and the rung was never on the wire, failed
/// means it was tried and did not work. Conflating them turns "your NAT is
/// symmetric so punching cannot work" into "punching failed", which sends the
/// reader looking for a firewall.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Decided against before anything was sent, with the reason.
    Skipped(String),
    /// Attempted, and it did not produce a path.
    Failed(String),
    /// Not needed: an earlier rung won before this one was reached.
    NotReached,
    /// This is the rung that won.
    Won,
}

impl Verdict {
    /// The word the report prints. Fixed strings, because the whole point is
    /// that two of them are not the same word.
    fn word(&self) -> &'static str {
        match self {
            Verdict::Skipped(_) => "skipped",
            Verdict::Failed(_) => "failed",
            Verdict::NotReached => "not reached",
            Verdict::Won => "connected",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Verdict::Skipped(why) | Verdict::Failed(why) => why,
            Verdict::NotReached => "an earlier rung won",
            Verdict::Won => "this is the path in use",
        }
    }
}

/// Every rung of one ladder run, in order, with what became of it.
///
/// This is [`nominate`]'s error type. A caller cannot report a failure without
/// having this in hand, which is what stops "connection failed" from being
/// written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LadderReport {
    entries: Vec<(Rung, Verdict)>,
}

/// Every rung, in ladder order. The report walks this rather than the plan, so
/// a rung the plan never mentioned still appears — with `NotReached` — instead
/// of vanishing from the account.
const EVERY_RUNG: [Rung; 5] = [
    Rung::Ipv6Direct,
    Rung::PortMapped,
    Rung::StunPunch,
    Rung::Birthday,
    Rung::SshTunnel,
];

/// Which number the user sees. The spec numbers the ladder from zero and the
/// status line, the docs and the bug reports all use those numbers.
fn rung_number(rung: Rung) -> u8 {
    match rung {
        Rung::Ipv6Direct => 0,
        Rung::PortMapped => 1,
        Rung::StunPunch => 2,
        Rung::Birthday => 3,
        Rung::SshTunnel => 4,
    }
}

fn rung_name(rung: Rung) -> &'static str {
    match rung {
        Rung::Ipv6Direct => "IPv6 direct",
        Rung::PortMapped => "router port mapping",
        Rung::StunPunch => "STUN punch",
        Rung::Birthday => "birthday blast",
        Rung::SshTunnel => "SSH tunnel",
    }
}

impl LadderReport {
    fn new() -> LadderReport {
        LadderReport {
            entries: EVERY_RUNG
                .iter()
                .map(|r| (*r, Verdict::NotReached))
                .collect(),
        }
    }

    fn set(&mut self, rung: Rung, verdict: Verdict) {
        for (r, v) in &mut self.entries {
            if *r == rung {
                *v = verdict;
                return;
            }
        }
    }

    /// What became of one rung. `None` is impossible for a `Rung`, so this
    /// returns the verdict directly.
    #[must_use]
    pub fn verdict(&self, rung: Rung) -> &Verdict {
        self.entries
            .iter()
            .find(|(r, _)| *r == rung)
            .map(|(_, v)| v)
            .expect("every rung is seeded in `new`")
    }

    /// Every rung and its verdict, in ladder order.
    pub fn entries(&self) -> impl Iterator<Item = (Rung, &Verdict)> {
        self.entries.iter().map(|(r, v)| (*r, v))
    }
}

impl fmt::Display for LadderReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "no usable path to the host. Every rung, in order:")?;
        for (rung, verdict) in &self.entries {
            writeln!(
                f,
                "  rung {}  {:<20}  {:<11}  {}",
                rung_number(*rung),
                rung_name(*rung),
                verdict.word(),
                verdict.detail()
            )?;
        }
        write!(
            f,
            "A rung marked `skipped` was never put on the wire; one marked \
             `failed` was tried and did not answer."
        )
    }
}

impl std::error::Error for LadderReport {}

/// Everything the ladder needs that is not the socket.
///
/// A struct rather than seven positional arguments because four of them are
/// candidate lists and address types that would otherwise be swappable at the
/// call site.
pub struct Ladder<'a> {
    /// The per-attach key. Every check on every rung is authenticated with it,
    /// which is what stops the blast being usable as a reflector.
    pub psk: &'a Psk,
    /// The client is always `Controlling`; the host is always `Controlled`.
    /// Only the controlling side nominates — see `oxutrm_net::ice`.
    pub role: IceRole,
    /// What STUN typing concluded about *our* NAT. It selects the plan.
    pub nat: NatType,
    pub cfg: &'a NetConfig,
    /// Our own candidates, as offered to the peer.
    pub local: Vec<Candidate>,
    /// The peer's candidates, as they arrived in the hello.
    pub remote: Vec<Candidate>,
}

/// Which rung a remote candidate belongs to.
///
/// **This mirrors `IceAgent::nominated_event`** (`crates/oxutrm-net/src/ice.rs`,
/// the `match kind` deciding the `Rung`), which is private. The mirror is what
/// lets the report say "rung 0 was skipped because the peer offered no IPv6
/// host candidate" instead of blaming a rung that never had anything to try.
/// If the two ever disagree, the report lies about a rung — so
/// `the_rung_classification_matches_the_one_ice_nominates_by` pins the rule.
fn rung_of(remote: &Candidate) -> Rung {
    match remote.kind {
        CandidateKind::PortMapped => Rung::PortMapped,
        CandidateKind::Host if remote.addr.is_ipv6() => Rung::Ipv6Direct,
        _ => Rung::StunPunch,
    }
}

/// The peer address the blast guesses around.
///
/// A server-reflexive candidate is the only useful base: it is a real external
/// mapping a STUN server observed, and rung 3's whole premise is that a
/// symmetric NAT's next allocation lands *near* the last one. A host candidate
/// is an interface address behind the NAT and names no mapping at all, so
/// guessing ports around it aims 65k packets at the wrong machine.
fn blast_base(remote: &[Candidate]) -> Option<SocketAddr> {
    remote
        .iter()
        .filter(|c| matches!(c.kind, CandidateKind::ServerReflexive))
        .max_by_key(|c| c.priority)
        .map(|c| c.addr)
}

/// Run the ladder on `socket` until a rung wins or every one is accounted for.
///
/// `inbound` carries candidates that arrive from the peer *during* the race —
/// a `CandidateUpdate` over signalling — and `learned` carries our own
/// peer-reflexive discoveries back out to be sent to the peer. Neither is
/// optional: the agent emits `NewLocalCandidate` whether or not anyone is
/// listening, and dropping it on the floor is how one-sided port mapping
/// silently stops working.
///
/// On success the caller gets a socket. On failure it gets [`LadderReport`],
/// which names all five rungs.
pub async fn nominate(
    socket: Arc<tokio::net::UdpSocket>,
    ladder: Ladder<'_>,
    inbound: &mut tokio::sync::mpsc::Receiver<Candidate>,
    learned: &tokio::sync::mpsc::Sender<Candidate>,
) -> Result<Nomination, LadderReport> {
    let plan = LadderPlan::for_nat(ladder.nat);
    let mut report = LadderReport::new();

    // The plan's own skips first: these are decisions taken from the NAT type
    // before a packet exists, and they carry the reason with them.
    for (rung, why) in &plan.skipped {
        report.set(*rung, Verdict::Skipped(why.clone()));
    }

    // A raced rung with no remote candidate of its class was never attempted,
    // and saying "failed" about it would send the reader hunting for a
    // firewall. This is the difference property 4 is about.
    for rung in &plan.raced {
        if matches!(report.verdict(*rung), Verdict::Skipped(_)) {
            continue;
        }
        if !ladder.remote.iter().any(|c| rung_of(c) == *rung) {
            report.set(
                *rung,
                Verdict::Skipped(format!(
                    "the peer offered no candidate of this class ({} of theirs, none on rung {})",
                    ladder.remote.len(),
                    rung_number(*rung)
                )),
            );
        }
    }

    let raced: Vec<Rung> = plan
        .raced
        .iter()
        .copied()
        .filter(|r| !matches!(report.verdict(*r), Verdict::Skipped(_)))
        .collect();

    let mut probes = 0u32;
    if raced.is_empty() {
        // Nothing to race. The per-rung reasons are already recorded above, so
        // there is nothing to add here — and no agent is built, because an
        // agent with no pairs would report a failure that is not the truth.
    } else {
        match race(&socket, &ladder, inbound, learned).await {
            RaceOutcome::Nominated {
                local,
                remote,
                rung,
                probes: sent,
            } => {
                report.set(rung, Verdict::Won);
                return Ok(Nomination {
                    socket,
                    local,
                    remote,
                    rung,
                    probes: sent,
                });
            }
            RaceOutcome::Failed { why, probes: sent } => {
                probes = sent;
                // One reason for the whole group, because there was one race.
                // Inventing a separate sentence per rung would describe an
                // attempt that did not happen: the agent checks every pair on
                // one socket and reports once.
                for rung in &raced {
                    report.set(*rung, Verdict::Failed(why.clone()));
                }
            }
        }
    }

    // Rung 3. Reached either because the race produced nothing, or because the
    // NAT is symmetric and the plan sent us straight here.
    if plan.sequential.contains(&Rung::Birthday) {
        match blast(&ladder, probes).await {
            Ok(nomination) => {
                report.set(Rung::Birthday, Verdict::Won);
                return Ok(nomination);
            }
            Err(verdict) => report.set(Rung::Birthday, verdict),
        }
    }

    // Rung 4 has no implementation anywhere in the tree: `oxutrm_net::ice`
    // never nominates `Rung::SshTunnel`, and the contract records the framing
    // rules it must carry when it is built. Saying "failed" would claim an
    // attempt that no code could have made.
    report.set(
        Rung::SshTunnel,
        Verdict::Skipped(
            "the ssh tunnel is not implemented in this build; the contract records \
             the framing it must carry"
                .to_string(),
        ),
    );

    Err(report)
}

/// What one race produced. `IceEvent` is not this type because
/// `NewLocalCandidate` is not an outcome — it is handled inside the loop.
enum RaceOutcome {
    Nominated {
        local: SocketAddr,
        remote: SocketAddr,
        rung: Rung,
        probes: u32,
    },
    Failed {
        why: String,
        probes: u32,
    },
}

/// Rungs 0 to 2, raced by one agent on one socket — property 1.
async fn race(
    socket: &Arc<tokio::net::UdpSocket>,
    ladder: &Ladder<'_>,
    inbound: &mut tokio::sync::mpsc::Receiver<Candidate>,
    learned: &tokio::sync::mpsc::Sender<Candidate>,
) -> RaceOutcome {
    let mut agent = IceAgent::new(ladder.psk, ladder.role, ladder.cfg.clone());
    for c in &ladder.local {
        agent.add_local(c.clone());
    }
    for c in &ladder.remote {
        agent.add_remote(c.clone());
    }

    // `recv` cannot be selected against a live `agent.run` future without
    // dropping it: `run` borrows the agent mutably for the future's whole life,
    // and `add_remote` needs that borrow back. So the losing future IS
    // cancelled here, deliberately. The cost is bounded and known: `run`'s
    // await points are a `send_to`, a `recv_from` and a response `send_to`, and
    // it records a probe only *after* the send returns, so a cancelled poll
    // loses at most one in-flight check — which ICE re-sends within its
    // 200 ms retry interval. Losing a candidate update for up to the whole
    // gather timeout, which is what not selecting would cost, is worse.
    let mut updates_open = true;
    loop {
        enum Step {
            Event(IceEvent),
            Remote(Candidate),
            Closed,
        }
        let step = if updates_open {
            tokio::select! {
                ev = agent.run(socket.clone()) => Step::Event(ev),
                c = inbound.recv() => match c {
                    Some(c) => Step::Remote(c),
                    None => Step::Closed,
                },
            }
        } else {
            Step::Event(agent.run(socket.clone()).await)
        };

        // Nothing above touches `self`-like state inside the select arms; every
        // mutation is here, after the expression ended and the losing future
        // dropped. Same rule the client's event loop follows.
        match step {
            Step::Closed => updates_open = false,
            Step::Remote(c) => agent.add_remote(c),
            Step::Event(IceEvent::NewLocalCandidate(c)) => {
                // A closed receiver is not a ladder failure: the peer may
                // already have everything it needs.
                let _ = learned.send(c).await;
            }
            Step::Event(IceEvent::Nominated {
                local,
                remote,
                rung,
                probes,
            }) => {
                return RaceOutcome::Nominated {
                    local,
                    remote,
                    rung,
                    probes,
                };
            }
            Step::Event(IceEvent::Failed(why)) => {
                return RaceOutcome::Failed {
                    why,
                    probes: agent.probes_sent(),
                };
            }
        }
    }
}

/// Rung 3. `Err` carries the verdict to record, because every way this rung
/// ends short of a path is a reason somebody has to read.
async fn blast(ladder: &Ladder<'_>, already_sent: u32) -> Result<Nomination, Verdict> {
    if !ladder.cfg.enable_birthday {
        return Err(Verdict::Skipped(
            "the birthday blast is switched off in this configuration".to_string(),
        ));
    }
    let Some(base) = blast_base(&ladder.remote) else {
        return Err(Verdict::Skipped(
            "the peer offered no server-reflexive candidate, so there is no \
             observed external port to guess around"
                .to_string(),
        ));
    };

    let result = birthday_blast(ladder.psk, ladder.role, base, ladder.cfg).await;
    let found = match result {
        Ok(Some(found)) => found,
        Ok(None) => {
            return Err(Verdict::Failed(format!(
                "no hole found around {base} within {:?}",
                ladder.cfg.birthday_budget
            )));
        }
        Err(e) => return Err(Verdict::Failed(format!("the blast could not start: {e}"))),
    };

    // Property 2 in the concrete: the mapping belongs to THIS socket, so this
    // is the one QUIC adopts. The socket the race used is now useless — its
    // mapping names a hole the peer never reached.
    let probes = already_sent.saturating_add(found.probes);
    let remote = found.remote;
    let socket = match adopt(found.socket) {
        Ok(s) => s,
        Err(e) => {
            return Err(Verdict::Failed(format!(
                "the blast punched a hole but its socket could not be adopted: {e}"
            )));
        }
    };
    let local = match socket.local_addr() {
        Ok(a) => a,
        Err(e) => {
            return Err(Verdict::Failed(format!(
                "the blast's socket has no local address: {e}"
            )));
        }
    };

    Ok(Nomination {
        socket,
        local,
        remote,
        rung: Rung::Birthday,
        probes,
    })
}

/// Hand the blast's socket to the runtime.
///
/// `set_nonblocking` first because `from_std` requires it and rejects a
/// blocking socket at run time rather than at compile time. It is already true
/// in practice — the blast dups a tokio socket's descriptor — but "already
/// true in practice" is a property of someone else's code.
fn adopt(socket: std::net::UdpSocket) -> std::io::Result<Arc<tokio::net::UdpSocket>> {
    socket.set_nonblocking(true)?;
    Ok(Arc::new(tokio::net::UdpSocket::from_std(socket)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_net::ice_priority;
    use std::time::Duration;

    const PSK: Psk = Psk::new([0x5a; 32]);

    fn cfg(gather_ms: u64) -> NetConfig {
        NetConfig {
            gather_timeout: Duration::from_millis(gather_ms),
            // The blast is off by default in these tests: the ones that want it
            // switch it on, and the ones that do not would otherwise spend six
            // seconds firing 65k packets at a loopback port.
            enable_birthday: false,
            enable_port_mapping: false,
            ..NetConfig::default()
        }
    }

    fn candidate(addr: &str, kind: CandidateKind) -> Candidate {
        let addr: SocketAddr = addr.parse().expect("a literal address");
        Candidate {
            priority: ice_priority(kind, &addr.ip()),
            addr,
            kind,
        }
    }

    async fn socket() -> Arc<tokio::net::UdpSocket> {
        Arc::new(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind a loopback socket"),
        )
    }

    fn channels() -> (
        tokio::sync::mpsc::Receiver<Candidate>,
        tokio::sync::mpsc::Sender<Candidate>,
    ) {
        let (tx_in, rx_in) = tokio::sync::mpsc::channel(8);
        let (tx_out, _rx_out) = tokio::sync::mpsc::channel(8);
        // The inbound sender is dropped here on purpose in the tests that do
        // not use it: a closed update channel must not spin the race.
        drop(tx_in);
        (rx_in, tx_out)
    }

    // ------------------------------------------------------ property 4

    /// The property the deleted `RungRunner` was the only thing testing.
    #[tokio::test]
    async fn a_total_failure_still_accounts_for_every_rung() {
        let sock = socket().await;
        let (mut rx, tx) = channels();
        // A peer address nothing is listening on: the race can only time out.
        let report = nominate(
            sock,
            Ladder {
                psk: &PSK,
                role: IceRole::Controlling,
                nat: NatType::EndpointIndependent,
                cfg: &cfg(120),
                local: vec![candidate("127.0.0.1:1", CandidateKind::Host)],
                remote: vec![candidate("127.0.0.1:9", CandidateKind::ServerReflexive)],
            },
            &mut rx,
            &tx,
        )
        .await
        .expect_err("nothing is listening, so nothing can be nominated");

        for rung in EVERY_RUNG {
            assert!(
                !matches!(report.verdict(rung), Verdict::NotReached),
                "rung {} has no reason recorded: {report}",
                rung_number(rung)
            );
        }
    }

    /// Skipped and failed must not read alike — that is the whole property.
    ///
    /// The fixture has to produce **one of each**, and the assertions have to
    /// look at the two rungs' own lines. An earlier version asserted
    /// `rendered.contains("failed")` over the whole report and passed on a
    /// report in which nothing failed at all: the closing sentence explaining
    /// the difference between the two words contains both of them. Found by
    /// printing the report and reading it.
    #[tokio::test]
    async fn a_skipped_rung_reads_differently_from_a_failed_one() {
        let sock = socket().await;
        let (mut rx, tx) = channels();
        let report = nominate(
            sock,
            Ladder {
                psk: &PSK,
                role: IceRole::Controlling,
                nat: NatType::Symmetric,
                cfg: &cfg(120),
                local: vec![candidate("127.0.0.1:1", CandidateKind::Host)],
                // An IPv6 host candidate gives rung 0 something of its class to
                // try, so it is genuinely ATTEMPTED and genuinely fails --
                // while rung 2 is skipped by policy before anything is sent.
                remote: vec![candidate("[::1]:9", CandidateKind::Host)],
            },
            &mut rx,
            &tx,
        )
        .await
        .expect_err("nothing is listening");

        // Symmetric NAT: the plan skips rung 2 before a packet exists.
        let punch = report.verdict(Rung::StunPunch).clone();
        assert!(
            matches!(&punch, Verdict::Skipped(why) if why.contains("symmetric")),
            "expected a policy skip naming the NAT, got {punch:?}"
        );
        // Rung 0 had a candidate, was put on the wire, and did not answer.
        let direct = report.verdict(Rung::Ipv6Direct).clone();
        assert!(
            matches!(&direct, Verdict::Failed(_)),
            "rung 0 had an IPv6 candidate and must read as attempted, got {direct:?}"
        );

        // And the two are told apart in the rendering, on their own lines.
        let rendered = report.to_string();
        let lines: Vec<&str> = rendered.lines().collect();
        let line_for = |n: u8| {
            *lines
                .iter()
                .find(|l| l.trim_start().starts_with(&format!("rung {n} ")))
                .unwrap_or_else(|| panic!("no line for rung {n} in {lines:?}"))
        };
        assert!(line_for(0).contains("failed"), "{}", line_for(0));
        assert!(!line_for(0).contains("skipped"), "{}", line_for(0));
        assert!(line_for(2).contains("skipped"), "{}", line_for(2));
        assert!(!line_for(2).contains("failed"), "{}", line_for(2));
    }

    /// A rung with nothing of its class to try is skipped, not failed. Blaming
    /// a rung that never had a candidate sends the reader hunting a firewall.
    #[tokio::test]
    async fn a_rung_with_no_candidate_of_its_class_is_skipped_not_failed() {
        let sock = socket().await;
        let (mut rx, tx) = channels();
        let report = nominate(
            sock,
            Ladder {
                psk: &PSK,
                role: IceRole::Controlling,
                nat: NatType::EndpointIndependent,
                cfg: &cfg(120),
                local: vec![candidate("127.0.0.1:1", CandidateKind::Host)],
                // IPv4 server-reflexive only: no IPv6 host candidate, and no
                // port-mapped one.
                remote: vec![candidate("127.0.0.1:9", CandidateKind::ServerReflexive)],
            },
            &mut rx,
            &tx,
        )
        .await
        .expect_err("nothing is listening");

        assert!(
            matches!(report.verdict(Rung::Ipv6Direct), Verdict::Skipped(_)),
            "rung 0 had no IPv6 candidate to try: {report}"
        );
        assert!(
            matches!(report.verdict(Rung::PortMapped), Verdict::Skipped(_)),
            "rung 1 had no port-mapped candidate to try: {report}"
        );
        assert!(
            matches!(report.verdict(Rung::StunPunch), Verdict::Failed(_)),
            "rung 2 had a candidate and was actually tried: {report}"
        );
    }

    /// Rung 4 is not built. It must say so rather than claim an attempt.
    #[tokio::test]
    async fn rung_four_is_reported_as_unbuilt_rather_than_failed() {
        let sock = socket().await;
        let (mut rx, tx) = channels();
        let report = nominate(
            sock,
            Ladder {
                psk: &PSK,
                role: IceRole::Controlling,
                nat: NatType::EndpointIndependent,
                cfg: &cfg(120),
                local: vec![candidate("127.0.0.1:1", CandidateKind::Host)],
                remote: vec![candidate("127.0.0.1:9", CandidateKind::ServerReflexive)],
            },
            &mut rx,
            &tx,
        )
        .await
        .expect_err("nothing is listening");

        let v = report.verdict(Rung::SshTunnel).clone();
        assert!(
            matches!(&v, Verdict::Skipped(why) if why.contains("not implemented")),
            "got {v:?}"
        );
    }

    #[test]
    fn the_report_names_every_rung_by_number_and_by_name() {
        let rendered = LadderReport::new().to_string();
        for rung in EVERY_RUNG {
            assert!(
                rendered.contains(&format!("rung {}", rung_number(rung))),
                "{rendered}"
            );
            assert!(rendered.contains(rung_name(rung)), "{rendered}");
        }
    }

    // ------------------------------------------------------ property 1

    /// Rungs 0 to 2 hand back the SAME socket they were given. A driver that
    /// raced them as separate futures would have to hand back a different one,
    /// and its mapping would name a hole our traffic never emerges from.
    #[tokio::test]
    async fn a_race_win_returns_the_very_socket_it_was_given() {
        let (client_sock, host_sock) = (socket().await, socket().await);
        let client_addr = client_sock.local_addr().expect("local addr");
        let host_addr = host_sock.local_addr().expect("local addr");
        let want = Arc::clone(&client_sock);

        let host = tokio::spawn({
            let cfg = cfg(4000);
            async move {
                let (mut rx, tx) = channels();
                nominate(
                    host_sock,
                    Ladder {
                        psk: &PSK,
                        role: IceRole::Controlled,
                        nat: NatType::EndpointIndependent,
                        cfg: &cfg,
                        local: vec![],
                        remote: vec![candidate(
                            &client_addr.to_string(),
                            CandidateKind::ServerReflexive,
                        )],
                    },
                    &mut rx,
                    &tx,
                )
                .await
            }
        });

        let (mut rx, tx) = channels();
        let got = nominate(
            client_sock,
            Ladder {
                psk: &PSK,
                role: IceRole::Controlling,
                nat: NatType::EndpointIndependent,
                cfg: &cfg(4000),
                local: vec![],
                remote: vec![candidate(
                    &host_addr.to_string(),
                    CandidateKind::ServerReflexive,
                )],
            },
            &mut rx,
            &tx,
        )
        .await
        .expect("two live peers on loopback must nominate");

        assert!(
            Arc::ptr_eq(&got.socket, &want),
            "the nomination must hand back the socket the ladder ran on, not a new one"
        );
        assert_eq!(got.remote, host_addr);
        assert_eq!(got.rung, Rung::StunPunch);
        let _ = host.await;
    }

    // ------------------------------------------------------ property 2

    /// A peer that answers authenticated checks on one known port: the
    /// deterministic stand-in for the single hole a symmetric NAT leaves open.
    /// Same fixture `oxutrm_net::birthday`'s own tests use, because on loopback
    /// there is no NAT to allocate a port near the advertised base.
    async fn lurking_peer() -> (Arc<tokio::net::UdpSocket>, SocketAddr) {
        use oxutrm_net::{CheckKind, IceCredentials, build_check_response, parse_check};

        let sock = Arc::new(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("bind the lurking peer"),
        );
        let addr = sock.local_addr().expect("addr");
        let s = Arc::clone(&sock);
        tokio::spawn(async move {
            let creds = IceCredentials::derive(PSK.as_bytes());
            // It plays the host, so it verifies and signs with the credential
            // a Controlling client's requests carry.
            let d = IceCredentials::inbound(IceRole::Controlled);
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = s.recv_from(&mut buf).await else {
                    return;
                };
                let Some(c) = parse_check(&creds, d, &buf[..n]) else {
                    continue;
                };
                if c.kind != CheckKind::Request {
                    continue;
                }
                if let Ok(reply) = build_check_response(&creds, d, c.tid, from) {
                    let _ = s.send_to(&reply, from).await;
                }
            }
        });
        (sock, addr)
    }

    /// Rung 3 hands back a DIFFERENT socket, and that is the whole point.
    ///
    /// The blast punches from sockets it binds itself, and the mapping belongs
    /// to the one that found the hole and to no other. A driver that returned
    /// only addresses — or kept using the socket the race ran on — would leave
    /// QUIC talking from a socket whose mapping the peer has never seen. So the
    /// socket is an OUTPUT of the ladder, not a variable captured before it ran.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_blast_win_hands_back_the_socket_that_punched_the_hole() {
        let (_peer, peer_addr) = lurking_peer().await;
        let raced_on = socket().await;
        let raced_on_addr = raced_on.local_addr().expect("local addr");
        let (mut rx, tx) = channels();

        let got = nominate(
            Arc::clone(&raced_on),
            Ladder {
                psk: &PSK,
                role: IceRole::Controlling,
                // Symmetric sends the plan straight past rung 2, and the only
                // candidate is server-reflexive, so rungs 0 and 1 have nothing
                // of their class. The race is therefore never entered at all.
                nat: NatType::Symmetric,
                cfg: &NetConfig {
                    enable_birthday: true,
                    birthday_sockets: 4,
                    birthday_ports: 8,
                    birthday_budget: Duration::from_millis(3000),
                    ..cfg(120)
                },
                local: vec![],
                remote: vec![candidate(
                    &peer_addr.to_string(),
                    CandidateKind::ServerReflexive,
                )],
            },
            &mut rx,
            &tx,
        )
        .await
        .expect("the lurking peer sits exactly on the guessed base");

        assert_eq!(got.rung, Rung::Birthday);
        assert_eq!(got.remote, peer_addr);
        assert!(
            !Arc::ptr_eq(&got.socket, &raced_on),
            "rung 3 must hand back the socket it punched with, not the one the \
             ladder started on"
        );
        assert_ne!(
            got.local, raced_on_addr,
            "a different socket means a different local port"
        );
        assert_eq!(
            got.local,
            got.socket
                .local_addr()
                .expect("the adopted socket has an address"),
            "the reported local address must be the adopted socket's own"
        );
        // And it is a live socket the runtime owns, not a descriptor that was
        // closed when the blast's other sockets were dropped.
        assert!(
            got.socket.send_to(b"", peer_addr).await.is_ok(),
            "the adopted socket must still be usable"
        );
        assert!(
            got.probes > 0,
            "a win with no probes reported hides the cost"
        );
    }

    // ------------------------------------------------------ property 3

    /// The MTU is a quinn property discovered after the handshake, which is
    /// strictly after nomination ends. A field for it here could only hold a
    /// guess, and the deleted code had one with a test asserting it back.
    #[test]
    fn the_nomination_type_has_no_mtu_field() {
        const SOURCE: &str = include_str!("ladder.rs");
        let start = SOURCE
            .find("pub struct Nomination {")
            .expect("the struct is declared in this file");
        let body = &SOURCE[start..];
        let end = body.find("\n}").expect("the struct has a closing brace");
        let body = &body[..end];
        assert!(
            !body.contains("mtu"),
            "`Nomination` grew an mtu field. Path MTU is not knowable at \
             nomination: it is a quinn property discovered after the handshake, \
             which happens strictly later. Fill PathDescription::mtu from the \
             live connection instead.\n{body}"
        );
        // And the guard is not vacuous: it can see the fields that ARE there.
        assert!(body.contains("pub rung: Rung"), "{body}");
    }

    // ------------------------------------------------------ classification

    /// The report's per-rung skips are only true if this classification agrees
    /// with the one `IceAgent` nominates by. That function is private, so the
    /// rule is pinned here instead of inferred.
    #[test]
    fn the_rung_classification_matches_the_one_ice_nominates_by() {
        assert_eq!(
            rung_of(&candidate("203.0.113.7:443", CandidateKind::PortMapped)),
            Rung::PortMapped
        );
        assert_eq!(
            rung_of(&candidate("[2001:db8::2]:443", CandidateKind::PortMapped)),
            Rung::PortMapped,
            "a port mapping is rung 1 in either address family"
        );
        assert_eq!(
            rung_of(&candidate("[2001:db8::2]:443", CandidateKind::Host)),
            Rung::Ipv6Direct,
            "an IPv6 host candidate is rung 0"
        );
        assert_eq!(
            rung_of(&candidate("203.0.113.7:443", CandidateKind::Host)),
            Rung::StunPunch,
            "an IPv4 host candidate is NOT rung 0; ice.rs sends it to rung 2"
        );
        assert_eq!(
            rung_of(&candidate(
                "203.0.113.7:443",
                CandidateKind::ServerReflexive
            )),
            Rung::StunPunch
        );
        assert_eq!(
            rung_of(&candidate("203.0.113.7:443", CandidateKind::PeerReflexive)),
            Rung::StunPunch
        );
    }

    #[test]
    fn the_blast_guesses_around_a_server_reflexive_address_only() {
        // A host candidate is an interface address behind the NAT. Guessing
        // ports around it aims the blast at the wrong machine.
        assert_eq!(
            blast_base(&[candidate("192.168.1.5:443", CandidateKind::Host)]),
            None
        );
        assert_eq!(
            blast_base(&[
                candidate("192.168.1.5:443", CandidateKind::Host),
                candidate("203.0.113.7:51234", CandidateKind::ServerReflexive),
            ]),
            Some("203.0.113.7:51234".parse().expect("a literal address"))
        );
    }

    #[tokio::test]
    async fn the_blast_is_skipped_with_a_reason_when_there_is_no_base_to_guess_around() {
        let sock = socket().await;
        let (mut rx, tx) = channels();
        let report = nominate(
            sock,
            Ladder {
                psk: &PSK,
                role: IceRole::Controlling,
                nat: NatType::Symmetric,
                cfg: &NetConfig {
                    enable_birthday: true,
                    ..cfg(120)
                },
                local: vec![],
                // Host candidates only: no observed external port anywhere.
                remote: vec![candidate("127.0.0.1:9", CandidateKind::Host)],
            },
            &mut rx,
            &tx,
        )
        .await
        .expect_err("nothing is listening");

        let v = report.verdict(Rung::Birthday).clone();
        assert!(
            matches!(&v, Verdict::Skipped(why) if why.contains("server-reflexive")),
            "got {v:?}"
        );
    }

    // ------------------------------------------------------ candidate updates

    /// A candidate that arrives mid-race is what makes the race winnable.
    ///
    /// **This fixture is built the way it is because the obvious one proves
    /// nothing.** Point two live peers at each other on loopback and the
    /// update channel is irrelevant: the peer's own checks arrive at our
    /// socket, `IceAgent` learns its address peer-reflexively for free, and the
    /// pair validates whether or not anything was ever delivered on the
    /// channel. Measured — with `add_remote` deleted from the loop the naive
    /// version still passed, in 0.00s, before the update was even sent.
    ///
    /// So neither side is told a true address to start with. Each is given a
    /// dead port, so the host's checks never reach the client and there is no
    /// peer-reflexive rescue to hide behind. The client learns the host's real
    /// address only from the channel; its first check is what then teaches the
    /// host where the client is. Delete the `add_remote` and nothing is ever
    /// sent to a live socket by either end.
    #[tokio::test]
    async fn a_candidate_arriving_mid_race_is_what_makes_the_race_winnable() {
        let (client_sock, host_sock) = (socket().await, socket().await);
        let host_addr = host_sock.local_addr().expect("local addr");

        let host = tokio::spawn({
            let cfg = cfg(4000);
            async move {
                let (mut rx, tx) = channels();
                nominate(
                    host_sock,
                    Ladder {
                        psk: &PSK,
                        role: IceRole::Controlled,
                        nat: NatType::EndpointIndependent,
                        cfg: &cfg,
                        local: vec![],
                        // Port 9 (discard) on loopback: the host's checks go
                        // nowhere, so the client learns nothing from them.
                        remote: vec![candidate("127.0.0.1:9", CandidateKind::ServerReflexive)],
                    },
                    &mut rx,
                    &tx,
                )
                .await
            }
        });

        let (tx_in, mut rx_in) = tokio::sync::mpsc::channel(8);
        let (tx_out, _rx_out) = tokio::sync::mpsc::channel(8);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let _ = tx_in
                .send(candidate(
                    &host_addr.to_string(),
                    CandidateKind::ServerReflexive,
                ))
                .await;
        });

        let got = nominate(
            client_sock,
            Ladder {
                psk: &PSK,
                role: IceRole::Controlling,
                nat: NatType::EndpointIndependent,
                cfg: &cfg(4000),
                local: vec![],
                remote: vec![candidate("127.0.0.1:9", CandidateKind::ServerReflexive)],
            },
            &mut rx_in,
            &tx_out,
        )
        .await
        .expect("the updated candidate is the only one that can answer");

        assert_eq!(
            got.remote, host_addr,
            "the nominated peer must be the one that arrived mid-race"
        );
        let _ = host.await;
    }

    /// A closed update channel must not turn the race into a busy loop.
    ///
    /// An `mpsc::Receiver` whose senders are gone returns `None` immediately
    /// and forever, so a `select!` that keeps offering it a place is ready on
    /// every poll. The `updates_open` flag is what retires that arm.
    ///
    /// **Measured in CPU time, not wall clock, and that is the whole point.**
    /// The spin does not make the race take longer: `IceAgent`'s deadline is
    /// wall-clock, so a spinning loop still returns after the gather timeout,
    /// having burnt a core to get there. An elapsed-time assertion is blind to
    /// it — verified by deleting the flag, which left the wall-clock version
    /// green. This is the same shape as the `next_due()` busy loop the wiring
    /// decisions rejected: 1950 ms of CPU per 2000 ms of wall clock.
    #[tokio::test]
    async fn a_closed_update_channel_does_not_spin_the_race() {
        let sock = socket().await;
        let (mut rx, tx) = channels();

        let before = cpu_time();
        let started = std::time::Instant::now();
        let _ = nominate(
            sock,
            Ladder {
                psk: &PSK,
                role: IceRole::Controlling,
                nat: NatType::EndpointIndependent,
                cfg: &cfg(300),
                local: vec![],
                remote: vec![candidate("127.0.0.1:9", CandidateKind::ServerReflexive)],
            },
            &mut rx,
            &tx,
        )
        .await;
        let cpu = cpu_time() - before;
        let elapsed = started.elapsed();

        // A race that WAITS spends single-digit milliseconds of CPU over a
        // 300 ms gather; a race that spins spends nearly all of it. Half is a
        // wide margin either way, which is what keeps this from being a
        // timing-sensitive test on a shared machine: the clock below counts
        // only this thread's own CPU, so other tests and other tenants cannot
        // move it.
        assert!(
            cpu < elapsed / 2,
            "the race burnt {cpu:?} of CPU over {elapsed:?}: it span rather than waited"
        );
    }

    /// This thread's own CPU time. Process-wide would be polluted by the other
    /// tests running beside it; `#[tokio::test]` builds a current-thread
    /// runtime, so a spin in the race is a spin on this thread.
    fn cpu_time() -> Duration {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
        Duration::new(
            t.tv_sec
                .try_into()
                .expect("a non-negative CPU seconds count"),
            t.tv_nsec.try_into().expect("a nanosecond field below 1e9"),
        )
    }
}

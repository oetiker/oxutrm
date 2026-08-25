//! ICE: connectivity checks, one-sided nomination, peer-reflexive learning.
//!
//! This is the mechanism that makes "both ends behind NAT" work. Both sides
//! send authenticated checks to every candidate the other offered,
//! simultaneously; the first pair that answers in **both** directions is the
//! one QUIC gets.
//!
//! # Four decisions that are not negotiable
//!
//! **1. Nomination completes before QUIC starts.** QUIC connection migration
//! lets an endpoint change its own **local** address and nothing else. There
//! is no mechanism in RFC 9000 and no `quinn` API to repoint an established
//! connection at a different **remote** address. So a better path discovered
//! after nomination is **lost for that attach**; it is found again by the next
//! one, which re-runs this whole exchange. Do not design for late upgrades.
//!
//! **2. Only the controlling side nominates.** With both sides free to choose,
//! asymmetric loss — a pair validated at one end and not yet at the other —
//! makes them pick different pairs, and there is no agreed path at all. The
//! client is always [`IceRole::Controlling`]. Deterministic role assignment is
//! what removes tie-breaking entirely, rather than making it work.
//!
//! **3. Direction-labelled credentials everywhere.** Inbound requests and
//! nominations are verified with [`IceCredentials::inbound`]; inbound
//! responses with [`IceCredentials::outbound`], because a response is signed
//! with the credential of the request it answers. A reflected copy of our own
//! packet is then always checked against the wrong key.
//!
//! **4. Peer-reflexive learning is free, so it is used.** The
//! `XOR-MAPPED-ADDRESS` in a check response *is* our public address as that
//! peer sees it — no reflector needed. It is also why the ladder works when
//! only one side got a router port mapping: the other punches to it, and the
//! far side reads the arriving packet's source address.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxutrm_proto::{Candidate, CandidateKind, Rung};

use crate::{
    CheckKind, IceCredentials, IceRole, NetConfig, build_check_request, build_check_response,
    build_nomination, ice_priority, parse_check, random_transaction_id, to_socket_family, unmap,
};

/// How often a check is re-sent to a pair that has not answered.
const RETRY_INTERVAL: Duration = Duration::from_millis(200);
/// How long a single receive waits before the loop reconsiders its timers.
const POLL_SLICE: Duration = Duration::from_millis(25);

#[derive(Clone, Debug)]
pub enum IceEvent {
    /// We learned one of our own addresses from a peer's answer.
    NewLocalCandidate(Candidate),
    Nominated {
        local: SocketAddr,
        remote: SocketAddr,
        rung: Rung,
        probes: u32,
    },
    Failed(String),
}

#[derive(Default)]
struct PairState {
    /// The peer answered a check of ours: we can reach them.
    outbound_ok: bool,
    /// A valid check arrived from them: they can reach us.
    inbound_ok: bool,
    priority: u32,
    kind: Option<CandidateKind>,
    last_sent: Option<Instant>,
}

impl PairState {
    /// Validated means **both** directions, which is the only thing that
    /// proves a two-way path exists.
    fn validated(&self) -> bool {
        self.outbound_ok && self.inbound_ok
    }
}

pub struct IceAgent {
    creds: IceCredentials,
    role: IceRole,
    cfg: NetConfig,
    locals: Vec<Candidate>,
    /// Keyed by remote address, so a peer-reflexive discovery merges with an
    /// offered candidate at the same address instead of duplicating it.
    pairs: BTreeMap<SocketAddr, PairState>,
    /// Outstanding requests, so a response can be matched to the pair that
    /// caused it and timed.
    outstanding: HashMap<[u8; 12], (SocketAddr, Instant)>,
    reflexive_seen: HashSet<SocketAddr>,
    probes: u32,
    last_rtt: Option<Duration>,
    deadline: Option<Instant>,
    nominated: Option<SocketAddr>,
    done: bool,
}

impl IceAgent {
    pub fn new(psk: [u8; 32], role: IceRole, cfg: NetConfig) -> IceAgent {
        IceAgent {
            creds: IceCredentials::derive(&psk),
            role,
            cfg,
            locals: Vec::new(),
            pairs: BTreeMap::new(),
            outstanding: HashMap::new(),
            reflexive_seen: HashSet::new(),
            probes: 0,
            last_rtt: None,
            deadline: None,
            nominated: None,
            done: false,
        }
    }

    pub fn add_local(&mut self, c: Candidate) {
        if !self.locals.iter().any(|x| x.addr == c.addr) {
            self.locals.push(c);
        }
    }

    pub fn add_remote(&mut self, c: Candidate) {
        let e = self.pairs.entry(unmap(c.addr)).or_default();
        // Keep the better priority if the same address arrives twice.
        if c.priority > e.priority {
            e.priority = c.priority;
            e.kind = Some(c.kind);
        }
    }

    pub fn role(&self) -> IceRole {
        self.role
    }

    pub fn probes_sent(&self) -> u32 {
        self.probes
    }

    pub fn last_rtt(&self) -> Option<Duration> {
        self.last_rtt
    }

    pub fn remote_count(&self) -> usize {
        self.pairs.len()
    }

    pub fn local_count(&self) -> usize {
        self.locals.len()
    }

    /// Step until there is something to report. Call it in a loop.
    ///
    /// State persists across calls, so a caller that stops looping after
    /// `Nominated` leaves a usable agent behind rather than a half-run one.
    pub async fn run(&mut self, socket: Arc<tokio::net::UdpSocket>) -> IceEvent {
        let local_addr = match socket.local_addr() {
            Ok(a) => a,
            Err(e) => return IceEvent::Failed(format!("socket has no local address: {e}")),
        };
        let deadline = *self
            .deadline
            .get_or_insert_with(|| Instant::now() + self.cfg.gather_timeout);

        if self.pairs.is_empty() {
            return IceEvent::Failed("no remote candidates to check".to_string());
        }

        let mut buf = vec![0u8; 2048];
        loop {
            if self.done {
                return IceEvent::Failed("ICE already finished".to_string());
            }
            if Instant::now() >= deadline {
                self.done = true;
                return IceEvent::Failed(format!(
                    "no validated path after {:?} and {} probes",
                    self.cfg.gather_timeout, self.probes
                ));
            }

            self.send_due_checks(&socket, local_addr).await;

            // The controlling side nominates as soon as a pair is validated in
            // both directions. The controlled side waits to be told.
            if self.role == IceRole::Controlling
                && self.nominated.is_none()
                && let Some(remote) = self.best_validated()
            {
                let d = IceCredentials::outbound(self.role);
                if let Ok(msg) = build_nomination(&self.creds, d, random_transaction_id()) {
                    let dst = to_socket_family(&local_addr, remote);
                    // Sent more than once: an Indication draws no response, so
                    // a single loss would strand the controlled side.
                    for _ in 0..3 {
                        let _ = socket.send_to(&msg, dst).await;
                    }
                }
                self.nominated = Some(remote);
                self.done = true;
                return self.nominated_event(local_addr, remote);
            }

            let recv = tokio::time::timeout(POLL_SLICE, socket.recv_from(&mut buf)).await;
            let (n, from) = match recv {
                Ok(Ok(v)) => v,
                // A send to a dead address can surface as an ICMP-driven error
                // on the next recv. It says nothing about the other pairs.
                Ok(Err(_)) => continue,
                Err(_) => continue,
            };
            let from = unmap(from);

            if let Some(ev) = self.on_datagram(&socket, local_addr, from, &buf[..n]).await {
                return ev;
            }
        }
    }

    /// Send a check to every pair that is due one.
    async fn send_due_checks(&mut self, socket: &tokio::net::UdpSocket, local: SocketAddr) {
        let now = Instant::now();
        let d = IceCredentials::outbound(self.role);
        let due: Vec<SocketAddr> = self
            .pairs
            .iter()
            .filter(|(_, s)| !s.outbound_ok)
            .filter(|(_, s)| {
                s.last_sent
                    .is_none_or(|t| now.duration_since(t) >= RETRY_INTERVAL)
            })
            .map(|(a, _)| *a)
            .collect();

        for remote in due {
            let tid = random_transaction_id();
            let Ok(msg) = build_check_request(&self.creds, d, tid) else {
                continue;
            };
            let dst = to_socket_family(&local, remote);
            if socket.send_to(&msg, dst).await.is_ok() {
                self.probes += 1;
                self.outstanding.insert(tid_bytes(&tid), (remote, now));
                if let Some(s) = self.pairs.get_mut(&remote) {
                    s.last_sent = Some(now);
                }
            }
        }
    }

    /// Classify one arriving datagram. Returns an event worth reporting.
    async fn on_datagram(
        &mut self,
        socket: &tokio::net::UdpSocket,
        local: SocketAddr,
        from: SocketAddr,
        datagram: &[u8],
    ) -> Option<IceEvent> {
        let inbound = IceCredentials::inbound(self.role);
        let outbound = IceCredentials::outbound(self.role);

        // A request or a nomination from the peer, verified with THEIR
        // credential. Our own reflected packet fails here, which is the whole
        // reason the credentials are direction-labelled.
        if let Some(check) = parse_check(&self.creds, inbound, datagram) {
            match check.kind {
                CheckKind::Request => {
                    // Answer it, telling them the address we saw — that is
                    // their peer-reflexive discovery, free.
                    if let Ok(reply) = build_check_response(&self.creds, inbound, check.tid, from) {
                        let _ = socket.send_to(&reply, to_socket_family(&local, from)).await;
                    }
                    // They reached us, so this direction is proven. A source
                    // we never heard of becomes a peer-reflexive pair.
                    let e = self.pairs.entry(from).or_insert_with(|| PairState {
                        priority: ice_priority(CandidateKind::PeerReflexive, &from.ip()),
                        kind: Some(CandidateKind::PeerReflexive),
                        ..PairState::default()
                    });
                    e.inbound_ok = true;
                    return None;
                }
                CheckKind::Nomination => {
                    // Only the controlling side nominates, so a nomination
                    // arriving at the controlling side is not ours to obey.
                    if self.role == IceRole::Controlled {
                        self.nominated = Some(from);
                        self.done = true;
                        return Some(self.nominated_event(local, from));
                    }
                    return None;
                }
                CheckKind::SuccessResponse => return None,
            }
        }

        // A response to one of OUR requests, verified with our own credential.
        if let Some(check) = parse_check(&self.creds, outbound, datagram)
            && check.kind == CheckKind::SuccessResponse
            && let Some((remote, sent_at)) = self.outstanding.remove(&tid_bytes(&check.tid))
        {
            self.last_rtt = Some(Instant::now().duration_since(sent_at));
            if let Some(s) = self.pairs.get_mut(&remote) {
                s.outbound_ok = true;
            }
            // Peer-reflexive: our own address, as this peer sees it.
            if let Some(seen) = check.reflexive
                && self.reflexive_seen.insert(seen)
                && !self.locals.iter().any(|c| c.addr == seen)
            {
                let cand = Candidate {
                    addr: seen,
                    kind: CandidateKind::ServerReflexive,
                    priority: ice_priority(CandidateKind::ServerReflexive, &seen.ip()),
                };
                self.locals.push(cand.clone());
                return Some(IceEvent::NewLocalCandidate(cand));
            }
            return None;
        }

        // Anything else — a stranger, a forgery, our own echo — is dropped
        // without touching a single piece of state.
        None
    }

    /// The highest-priority pair validated in both directions.
    fn best_validated(&self) -> Option<SocketAddr> {
        self.pairs
            .iter()
            .filter(|(_, s)| s.validated())
            .max_by_key(|(a, s)| (s.priority, std::cmp::Reverse(**a)))
            .map(|(a, _)| *a)
    }

    fn nominated_event(&self, local: SocketAddr, remote: SocketAddr) -> IceEvent {
        let kind = self.pairs.get(&remote).and_then(|s| s.kind);
        let rung = match kind {
            Some(CandidateKind::PortMapped) => Rung::PortMapped,
            Some(CandidateKind::Host) if remote.is_ipv6() => Rung::Ipv6Direct,
            _ => Rung::StunPunch,
        };
        IceEvent::Nominated {
            local: unmap(local),
            remote,
            rung,
            probes: self.probes,
        }
    }
}

/// `TransactionId` is not `Hash`, but its twelve bytes are.
fn tid_bytes(tid: &stun_codec::TransactionId) -> [u8; 12] {
    let mut out = [0u8; 12];
    out.copy_from_slice(tid.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;

    const PSK: [u8; 32] = [0x5A; 32];

    fn cfg(budget_ms: u64) -> NetConfig {
        NetConfig {
            gather_timeout: Duration::from_millis(budget_ms),
            ..NetConfig::default()
        }
    }

    fn host_candidate(addr: SocketAddr) -> Candidate {
        Candidate {
            addr,
            kind: CandidateKind::Host,
            priority: ice_priority(CandidateKind::Host, &addr.ip()),
        }
    }

    async fn sock() -> Arc<UdpSocket> {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.expect("bind"))
    }

    /// Both agents, wired to each other, run to nomination.
    async fn run_pair(
        client_extra_remotes: Vec<SocketAddr>,
    ) -> (Vec<IceEvent>, Vec<IceEvent>, SocketAddr, SocketAddr) {
        let cs = sock().await;
        let hs = sock().await;
        let ca = cs.local_addr().unwrap();
        let ha = hs.local_addr().unwrap();

        let mut client = IceAgent::new(PSK, IceRole::Controlling, cfg(4000));
        client.add_local(host_candidate(ca));
        client.add_remote(host_candidate(ha));
        for extra in client_extra_remotes {
            client.add_remote(host_candidate(extra));
        }

        let mut host = IceAgent::new(PSK, IceRole::Controlled, cfg(4000));
        host.add_local(host_candidate(ha));
        host.add_remote(host_candidate(ca));

        let c = tokio::spawn(async move {
            let mut out = Vec::new();
            for _ in 0..8 {
                let ev = client.run(cs.clone()).await;
                let stop = matches!(ev, IceEvent::Nominated { .. } | IceEvent::Failed(_));
                out.push(ev);
                if stop {
                    break;
                }
            }
            out
        });
        let h = tokio::spawn(async move {
            let mut out = Vec::new();
            for _ in 0..8 {
                let ev = host.run(hs.clone()).await;
                let stop = matches!(ev, IceEvent::Nominated { .. } | IceEvent::Failed(_));
                out.push(ev);
                if stop {
                    break;
                }
            }
            out
        });

        let (cev, hev) = tokio::join!(c, h);
        (cev.expect("client task"), hev.expect("host task"), ca, ha)
    }

    fn nominated(events: &[IceEvent]) -> Option<(SocketAddr, SocketAddr, Rung, u32)> {
        events.iter().find_map(|e| match e {
            IceEvent::Nominated {
                local,
                remote,
                rung,
                probes,
            } => Some((*local, *remote, *rung, *probes)),
            _ => None,
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_agents_validate_a_pair_and_both_report_it() {
        let (cev, hev, ca, ha) = run_pair(Vec::new()).await;

        let (cl, cr, _, probes) = nominated(&cev).expect("the client nominated");
        let (hl, hr, _, _) = nominated(&hev).expect("the host was told");

        assert_eq!(cl, ca);
        assert_eq!(cr, ha);
        assert_eq!(hl, ha);
        assert_eq!(hr, ca);
        assert!(probes > 0, "nomination without ever probing");
    }

    /// Only the controlling side chooses. With both free to choose,
    /// asymmetric loss makes them pick different pairs and agree on nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn both_sides_converge_on_the_same_pair_when_a_candidate_is_dead() {
        // A candidate nobody is listening on: checks to it are simply lost,
        // which is asymmetric loss in its purest form.
        let dead = {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
            let a = s.local_addr().expect("addr");
            drop(s);
            a
        };
        let (cev, hev, ca, ha) = run_pair(vec![dead]).await;

        let (_, cr, _, _) = nominated(&cev).expect("the client nominated");
        let (_, hr, _, _) = nominated(&hev).expect("the host was told");
        assert_eq!(cr, ha, "the client nominated the dead candidate");
        assert_eq!(hr, ca, "the two sides disagree about the path");
    }

    /// Genuine ONE-DIRECTIONAL loss, which a dead candidate cannot produce.
    ///
    /// A relay forwards client -> host intact but drops the first few
    /// host -> client datagrams. The host therefore validates its inbound
    /// direction almost immediately while the client's outbound direction
    /// lags — a pair validated at one end and not the other, which is exactly
    /// the state one-sided nomination exists to survive. With both sides free
    /// to nominate they could commit to different pairs here; with only the
    /// controlling side choosing, they cannot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_directional_loss_still_leaves_both_sides_on_the_same_path() {
        /// How many host -> client datagrams the relay eats.
        const DROPS: usize = 6;

        let cs = sock().await;
        let hs = sock().await;
        let ca = cs.local_addr().unwrap();
        let ha = hs.local_addr().unwrap();

        // Client-facing and host-facing halves of the lossy middlebox.
        let front = sock().await;
        let back = sock().await;
        let front_addr = front.local_addr().unwrap();
        let back_addr = back.local_addr().unwrap();

        let relay = tokio::spawn({
            let (front, back) = (front.clone(), back.clone());
            async move {
                let mut fbuf = vec![0u8; 2048];
                let mut bbuf = vec![0u8; 2048];
                let mut client_seen: Option<SocketAddr> = None;
                let mut dropped = 0usize;
                loop {
                    tokio::select! {
                        r = front.recv_from(&mut fbuf) => {
                            let Ok((n, from)) = r else { return };
                            client_seen = Some(from);
                            // client -> host: always delivered.
                            let _ = back.send_to(&fbuf[..n], ha).await;
                        }
                        r = back.recv_from(&mut bbuf) => {
                            let Ok((n, _)) = r else { return };
                            if dropped < DROPS {
                                dropped += 1;
                                continue;
                            }
                            if let Some(c) = client_seen {
                                let _ = front.send_to(&bbuf[..n], c).await;
                            }
                        }
                    }
                }
            }
        });

        // A dead candidate as well, so there is a real choice to converge on.
        let dead = {
            let d = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
            let a = d.local_addr().expect("addr");
            drop(d);
            a
        };

        let mut client = IceAgent::new(PSK, IceRole::Controlling, cfg(6000));
        client.add_local(host_candidate(ca));
        client.add_remote(host_candidate(front_addr));
        client.add_remote(host_candidate(dead));

        let mut host = IceAgent::new(PSK, IceRole::Controlled, cfg(6000));
        host.add_local(host_candidate(ha));
        host.add_remote(host_candidate(back_addr));

        let c = tokio::spawn(async move {
            let mut out = Vec::new();
            for _ in 0..12 {
                let ev = client.run(cs.clone()).await;
                let stop = matches!(ev, IceEvent::Nominated { .. } | IceEvent::Failed(_));
                out.push(ev);
                if stop {
                    break;
                }
            }
            out
        });
        let h = tokio::spawn(async move {
            let mut out = Vec::new();
            for _ in 0..12 {
                let ev = host.run(hs.clone()).await;
                let stop = matches!(ev, IceEvent::Nominated { .. } | IceEvent::Failed(_));
                out.push(ev);
                if stop {
                    break;
                }
            }
            out
        });

        let (cev, hev) = tokio::join!(c, h);
        relay.abort();
        let (cev, hev) = (cev.expect("client task"), hev.expect("host task"));

        let (_, cr, _, _) =
            nominated(&cev).expect("the client never nominated despite a working path");
        let (_, hr, _, _) =
            nominated(&hev).expect("the host was never told, so the two sides disagree");

        assert_eq!(
            cr, front_addr,
            "the client nominated the dead candidate rather than the lossy but live one"
        );
        assert_eq!(
            hr, back_addr,
            "the host settled on a path the client is not using"
        );
    }

    /// The `XOR-MAPPED-ADDRESS` in a response is our own address as the peer
    /// sees it — a reflector is not needed to learn it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_response_teaches_us_our_own_address() {
        let cs = sock().await;
        let hs = sock().await;
        let ca = cs.local_addr().unwrap();
        let ha = hs.local_addr().unwrap();

        // The client is told nothing about its own address, so the only way
        // it can learn one is from the peer's answer.
        let mut client = IceAgent::new(PSK, IceRole::Controlling, cfg(4000));
        client.add_remote(host_candidate(ha));
        let mut host = IceAgent::new(PSK, IceRole::Controlled, cfg(4000));
        host.add_remote(host_candidate(ca));

        let h = tokio::spawn(async move {
            for _ in 0..8 {
                if matches!(
                    host.run(hs.clone()).await,
                    IceEvent::Nominated { .. } | IceEvent::Failed(_)
                ) {
                    break;
                }
            }
        });

        // The FIRST event must be the discovery: the response that validates
        // our outbound direction is the same response that carries our
        // address, so learning it cannot lag nomination.
        let first = client.run(cs.clone()).await;
        h.abort();

        let learned = match first {
            IceEvent::NewLocalCandidate(c) => c,
            other => panic!("expected a peer-reflexive candidate first, got {other:?}"),
        };
        assert_eq!(
            learned.addr, ca,
            "the peer reported an address that is not ours"
        );
        assert_eq!(learned.kind, CandidateKind::ServerReflexive);
    }

    /// A stranger's check must not advance any state, however well-formed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_check_signed_with_the_wrong_psk_advances_nothing() {
        let victim = sock().await;
        let va = victim.local_addr().unwrap();
        let attacker = sock().await;

        let mut agent = IceAgent::new(PSK, IceRole::Controlling, cfg(600));
        agent.add_local(host_candidate(va));
        agent.add_remote(host_candidate(attacker.local_addr().unwrap()));

        // The attacker answers every check, but with the wrong key.
        let wrong = IceCredentials::derive(&[0xEE; 32]);
        let atk = attacker.clone();
        let forger = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = atk.recv_from(&mut buf).await else {
                    return;
                };
                let _ = n;
                let bogus = build_check_response(
                    &wrong,
                    crate::Direction::ClientToHost,
                    random_transaction_id(),
                    from,
                )
                .expect("build");
                let _ = atk.send_to(&bogus, from).await;
            }
        });

        let ev = agent.run(victim).await;
        forger.abort();
        assert!(
            matches!(ev, IceEvent::Failed(_)),
            "a forged response advanced the state machine: {ev:?}"
        );
        assert!(agent.last_rtt().is_none(), "a forged response was timed");
    }

    /// The reflection case the direction-labelled credentials exist for: our
    /// own check, echoed straight back, must not validate anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn our_own_reflected_check_never_satisfies_nomination() {
        let victim = sock().await;
        let va = victim.local_addr().unwrap();
        let mirror = sock().await;

        let mut agent = IceAgent::new(PSK, IceRole::Controlling, cfg(600));
        agent.add_local(host_candidate(va));
        agent.add_remote(host_candidate(mirror.local_addr().unwrap()));

        // A perfect echo: exactly our bytes, back at us.
        let m = mirror.clone();
        let echo = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = m.recv_from(&mut buf).await else {
                    return;
                };
                let _ = m.send_to(&buf[..n], from).await;
            }
        });

        let ev = agent.run(victim).await;
        echo.abort();
        assert!(
            matches!(ev, IceEvent::Failed(_)),
            "a reflected check nominated a path to ourselves: {ev:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_silent_peer_fails_within_the_budget_rather_than_hanging() {
        let s = sock().await;
        let dead = {
            let d = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
            let a = d.local_addr().expect("addr");
            drop(d);
            a
        };
        let mut agent = IceAgent::new(PSK, IceRole::Controlling, cfg(400));
        agent.add_local(host_candidate(s.local_addr().unwrap()));
        agent.add_remote(host_candidate(dead));

        let started = Instant::now();
        let ev = tokio::time::timeout(Duration::from_secs(10), agent.run(s))
            .await
            .expect("run must respect its budget, not hang");
        assert!(matches!(ev, IceEvent::Failed(_)), "got {ev:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the budget was not honoured"
        );
        assert!(agent.probes_sent() > 0, "gave up without probing");
    }

    #[tokio::test]
    async fn an_agent_with_no_remote_candidates_fails_immediately() {
        let s = sock().await;
        let mut agent = IceAgent::new(PSK, IceRole::Controlling, cfg(5000));
        let ev = tokio::time::timeout(Duration::from_millis(500), agent.run(s))
            .await
            .expect("must not wait out the budget with nothing to check");
        assert!(matches!(ev, IceEvent::Failed(_)));
    }

    #[test]
    fn the_client_is_always_controlling_and_the_roles_do_not_collide() {
        let c = IceAgent::new(PSK, IceRole::Controlling, cfg(1000));
        let h = IceAgent::new(PSK, IceRole::Controlled, cfg(1000));
        assert_eq!(c.role(), IceRole::Controlling);
        assert_eq!(h.role(), IceRole::Controlled);
        assert_ne!(
            IceCredentials::outbound(c.role()),
            IceCredentials::outbound(h.role()),
            "the two sides must sign with different credentials"
        );
    }

    #[test]
    fn duplicate_candidates_are_merged_rather_than_probed_twice() {
        let mut a = IceAgent::new(PSK, IceRole::Controlling, cfg(1000));
        let addr: SocketAddr = "203.0.113.9:443".parse().unwrap();
        a.add_remote(host_candidate(addr));
        a.add_remote(host_candidate(addr));
        assert_eq!(a.remote_count(), 1);

        let l: SocketAddr = "10.0.0.2:443".parse().unwrap();
        a.add_local(host_candidate(l));
        a.add_local(host_candidate(l));
        assert_eq!(a.local_count(), 1);
    }

    /// An IPv6 host pair is rung 0, which is the whole reason the priorities
    /// put it first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_ipv6_host_pair_reports_rung_zero() {
        let Ok(cs) = UdpSocket::bind("[::1]:0").await else {
            return; // A host with IPv6 switched off; nothing to assert.
        };
        let Ok(hs) = UdpSocket::bind("[::1]:0").await else {
            return;
        };
        let (cs, hs) = (Arc::new(cs), Arc::new(hs));
        let ca = cs.local_addr().unwrap();
        let ha = hs.local_addr().unwrap();

        let mut client = IceAgent::new(PSK, IceRole::Controlling, cfg(4000));
        client.add_local(host_candidate(ca));
        client.add_remote(host_candidate(ha));
        let mut host = IceAgent::new(PSK, IceRole::Controlled, cfg(4000));
        host.add_local(host_candidate(ha));
        host.add_remote(host_candidate(ca));

        let h = tokio::spawn(async move {
            for _ in 0..8 {
                if matches!(
                    host.run(hs.clone()).await,
                    IceEvent::Nominated { .. } | IceEvent::Failed(_)
                ) {
                    break;
                }
            }
        });
        let mut got = None;
        for _ in 0..8 {
            match client.run(cs.clone()).await {
                IceEvent::Nominated { rung, .. } => {
                    got = Some(rung);
                    break;
                }
                IceEvent::Failed(e) => panic!("IPv6 loopback failed: {e}"),
                IceEvent::NewLocalCandidate(_) => continue,
            }
        }
        h.abort();
        assert_eq!(got, Some(Rung::Ipv6Direct));
    }
}

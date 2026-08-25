//! The connection ladder: which rungs to try, in what order, and what to do
//! when none of them works.
//!
//! Rungs run **concurrently where cheap and ordered where costly**. IPv6 direct
//! (rung 0), a router mapping (rung 1) and STUN punching (rung 2) are raced
//! together, Happy-Eyeballs style, because all three are cheap and the first
//! validated path wins. The birthday blast (rung 3) is deliberately noisy and
//! runs only when it has to. The ssh tunnel (rung 4) is last, and is announced
//! rather than taken quietly.
//!
//! # Nomination completes before QUIC starts
//!
//! There is no late upgrade. QUIC connection migration lets a client change its
//! own **local** address; it has no mechanism, and `quinn` has no API, for
//! repointing an established connection at a different **remote** one. So a
//! better path discovered after nomination is lost for that attach, and this
//! module does not pretend otherwise — it picks once.
//!
//! # Why the NAT type short-circuits
//!
//! Three STUN probes classify the NAT before any punching is attempted. When
//! they report `Symmetric`, ordinary punching cannot work: the external port is
//! different for every destination, so the address rung 2 learned describes a
//! mapping the peer will never reach. Attempting it anyway burns several
//! seconds to fail in a way already known in advance, so the plan skips it and
//! goes to rung 3.
//!
//! # What this module deliberately does not do
//!
//! It does not implement any rung. Rungs are supplied by a [`RungRunner`], so
//! the decision logic is testable without a network and the real
//! implementations drop in behind the same seam.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use oxutrm_proto::{NatType, PathDescription, Rung};

/// What one rung produced.
#[derive(Clone, Debug)]
pub enum RungResult {
    /// A validated path. `probes_sent` is carried because the status line shows
    /// it: a birthday blast that took 312 probes should say so, since the cost
    /// was real and the user paid it.
    Nominated {
        local: SocketAddr,
        remote: SocketAddr,
        probes_sent: u32,
        rtt_ms: u32,
        mtu: u16,
    },
    /// Tried and did not work.
    Failed(String),
    /// Not attempted, and why. A skipped rung is not a failed one, and the
    /// difference belongs in a bug report.
    Skipped(String),
}

/// Runs one rung. Implemented by the real network stack, and by tests.
///
/// Takes `Arc<Self>` so attempts can be raced on independent tasks, which is
/// what makes rungs 0 to 2 concurrent rather than merely interleaved.
pub trait RungRunner: Send + Sync + 'static {
    fn attempt(self: Arc<Self>, rung: Rung) -> Pin<Box<dyn Future<Output = RungResult> + Send>>;
}

/// Why no path could be established.
#[derive(Debug, thiserror::Error)]
pub enum LadderError {
    /// Every rung failed, the tunnel included. The per-rung reasons are kept,
    /// because "connection failed" is useless and the rung that got closest is
    /// the one worth reading.
    #[error("no path to the host could be established. {}", summarise(.attempts))]
    NoPath { attempts: Vec<(Rung, RungResult)> },
}

fn summarise(attempts: &[(Rung, RungResult)]) -> String {
    let mut parts = Vec::new();
    for (rung, result) in attempts {
        let text = match result {
            RungResult::Nominated { .. } => continue,
            RungResult::Failed(why) => format!("{rung:?} failed: {why}"),
            RungResult::Skipped(why) => format!("{rung:?} skipped: {why}"),
        };
        parts.push(text);
    }
    if parts.is_empty() {
        "No rung was even attempted.".to_string()
    } else {
        parts.join("; ")
    }
}

/// Which rungs to try, grouped by whether they can be raced.
///
/// A plan is decided from the NAT type before anything is attempted, so the
/// decision is inspectable and testable on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LadderPlan {
    /// Raced together. Cheap, and the first to validate wins.
    pub raced: Vec<Rung>,
    /// Tried in order, only if the race produced nothing. Costly or noisy.
    pub sequential: Vec<Rung>,
    /// Rungs deliberately not attempted, with the reason.
    pub skipped: Vec<(Rung, String)>,
}

impl LadderPlan {
    /// The plan for a given NAT classification.
    ///
    /// `NatType::Unknown` is treated as "might be anything", so nothing is
    /// skipped: guessing wrong towards skipping costs a connection, while
    /// guessing wrong towards attempting costs a few seconds.
    #[must_use]
    pub fn for_nat(nat: NatType) -> LadderPlan {
        let mut skipped = Vec::new();

        // Rung 2 cannot work behind a symmetric NAT: the external port differs
        // per destination, so the address it learned names a mapping the peer
        // will never reach. Failing to notice costs several seconds to discover
        // something the three-probe classification already established.
        let punch_is_hopeless = matches!(nat, NatType::Symmetric);
        if punch_is_hopeless {
            skipped.push((
                Rung::StunPunch,
                "the NAT is symmetric, so its external port differs per \
                 destination and ordinary punching cannot reach it"
                    .to_string(),
            ));
        }

        let mut raced = vec![Rung::Ipv6Direct, Rung::PortMapped];
        if !punch_is_hopeless {
            raced.push(Rung::StunPunch);
        }

        // The blast is deliberately noisy, so it never joins the race. It is
        // reached only when the cheap rungs have produced nothing -- or gone to
        // first when punching is already known to be hopeless.
        let sequential = vec![Rung::Birthday, Rung::SshTunnel];

        LadderPlan {
            raced,
            sequential,
            skipped,
        }
    }

    /// Every rung this plan will actually attempt, in order.
    #[must_use]
    pub fn attempted(&self) -> Vec<Rung> {
        self.raced
            .iter()
            .chain(self.sequential.iter())
            .copied()
            .collect()
    }
}

/// Run the ladder and nominate one path.
///
/// Returns a filled-in [`PathDescription`] — the thing the status line renders
/// and a user pastes into a bug report, so every field is populated from what
/// actually happened rather than left at a default.
///
/// The `nat_type` is carried into the result as well as used for the plan,
/// because "IPv4 punched behind a symmetric NAT" explains a later disconnection
/// that "IPv4 punched" does not.
pub async fn nominate(
    plan: &LadderPlan,
    nat_type: NatType,
    runner: Arc<dyn RungRunner>,
) -> Result<PathDescription, LadderError> {
    let mut attempts: Vec<(Rung, RungResult)> = plan
        .skipped
        .iter()
        .map(|(r, why)| (*r, RungResult::Skipped(why.clone())))
        .collect();

    // The cheap rungs, raced. First validated path wins; the rest are dropped
    // where they stand, because a better path arriving later cannot be adopted
    // anyway (QUIC migration moves our local address, never the peer's).
    if !plan.raced.is_empty() {
        let mut set = tokio::task::JoinSet::new();
        for rung in &plan.raced {
            let rung = *rung;
            let runner = Arc::clone(&runner);
            set.spawn(async move { (rung, runner.attempt(rung).await) });
        }
        while let Some(joined) = set.join_next().await {
            let Ok((rung, result)) = joined else {
                continue;
            };
            if let RungResult::Nominated { .. } = &result {
                // Abort the losers rather than letting them finish: a port
                // mapping nobody will use should be released, not left behind.
                set.abort_all();
                return Ok(describe(rung, nat_type, &result));
            }
            attempts.push((rung, result));
        }
    }

    // The costly ones, in order.
    for rung in &plan.sequential {
        let result = Arc::clone(&runner).attempt(*rung).await;
        if let RungResult::Nominated { .. } = &result {
            return Ok(describe(*rung, nat_type, &result));
        }
        attempts.push((*rung, result));
    }

    Err(LadderError::NoPath { attempts })
}

fn describe(rung: Rung, nat_type: NatType, result: &RungResult) -> PathDescription {
    match result {
        RungResult::Nominated {
            local,
            remote,
            probes_sent,
            rtt_ms,
            mtu,
        } => PathDescription {
            rung,
            local: *local,
            remote: *remote,
            probes_sent: *probes_sent,
            nat_type,
            rtt_ms: *rtt_ms,
            mtu: *mtu,
        },
        // Unreachable: `describe` is only called on the nominated arm. Written
        // as a panic rather than a default, because a `PathDescription` full of
        // zeroes would render as a plausible status line for a path that does
        // not exist.
        other => unreachable!("describe called on a non-nominated rung: {other:?}"),
    }
}

/// The one connect-time line, then silence.
///
/// A rung-4 session reads as a **warning**, because it has quietly lost two
/// properties the user is entitled to assume: it cannot survive an IP change,
/// and it cannot be reattached. Degrading silently would be worse than being
/// slow — the user finds out by closing their laptop.
#[must_use]
pub fn status_line(path: &PathDescription) -> String {
    let rung = match path.rung {
        Rung::Ipv6Direct => "IPv6 direct".to_string(),
        Rung::PortMapped => "IPv4 punched (router mapping)".to_string(),
        Rung::StunPunch => "IPv4 punched (STUN)".to_string(),
        Rung::Birthday => format!("IPv4 punched (birthday, {} probes)", path.probes_sent),
        Rung::SshTunnel => "SSH tunnel".to_string(),
    };

    if matches!(path.rung, Rung::SshTunnel) {
        return format!(
            "oxutrm  {rung} — no UDP path available  ·  {} ms  ·  \
             this session cannot roam and cannot be reattached      [warning]",
            path.rtt_ms
        );
    }

    let mut line = format!("oxutrm  {rung}  ·  {} ms  ·  mtu {}", path.rtt_ms, path.mtu);
    if matches!(path.nat_type, NatType::Symmetric) {
        // Worth saying: it explains why the connection took as long as it did,
        // and why it may be fragile.
        line.push_str("  ·  symmetric NAT");
    }
    line
}

//! The connection ladder's **policy**: which rungs are worth trying, in what
//! order, and which are hopeless before anything is attempted.
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
//! # This module is policy only, and the mechanism is not a seam
//!
//! There was once a `RungRunner` trait here, with a `nominate()` that raced one
//! `attempt(rung)` future per raced rung. It is gone, and it should not come
//! back in that shape. Three reasons, each of which outlives the code:
//!
//! **The race belongs to the candidate pairs, not to the rungs.** Rungs 0 to 2
//! are not three independent attempts. They are three *candidate classes* on
//! **one** socket, and `oxutrm_net::IceAgent` already races every pair on that
//! socket and reports which rung the winner belonged to. Racing them as three
//! futures means three concurrent receive loops on one socket, stealing each
//! other's datagrams — the very failure `StunDemuxSocket` exists to prevent.
//! NAT mappings are per-socket, so this is not a tidiness argument: an address
//! learned on any other socket names a hole our traffic will never emerge from.
//!
//! **A nomination has to hand back the socket, not just an address.** Rung 3
//! punches with a fresh socket and `birthday_blast` returns it, because the
//! mapping belongs to that socket and to no other — QUIC must take over that
//! exact one. A result type carrying only `SocketAddr`s silently drops it, and
//! what is left is an address describing a hole nothing owns any more. Whatever
//! drives the ladder must therefore return the socket itself.
//!
//! **The MTU is not knowable here.** Path MTU is a `quinn` property, discovered
//! after the handshake — which, by the rule above, is strictly after nomination
//! ends. Any per-rung result with an `mtu` field can only be guessed, and a
//! guess renders as a plausible status line for a number nobody measured.
//!
//! So the mechanism belongs with the socket, in the one place that already
//! depends on both this crate and `oxutrm-net` — the root binary — and it
//! belongs there as a concrete function rather than a trait: the trait's only
//! implementor in the whole tree was its own test double.
//!
//! **That driver has not been written yet.** It is not missing by oversight:
//! the contract records the four properties it must have, beside the rung-4
//! framing rules and for the same reason. Read them before writing it. Nothing
//! here is a seam waiting for an implementor — this module ends at the plan.

use oxutrm_proto::{NatType, Rung};

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

        // `raced` names the candidate classes the one ICE agent should gather
        // and check together, not three things to spawn. See the module note.
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

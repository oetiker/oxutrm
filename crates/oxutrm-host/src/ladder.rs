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
//! Three STUN probes classify the NAT before any punching is attempted, once
//! **per address family**: a dual-stack host often sits behind a symmetric
//! IPv4 NAT while its IPv6 path is open, and one verdict for both would either
//! throw the good path away or waste seconds on the bad one. When a family's
//! probes report `Symmetric`, ordinary punching cannot work in that family: the
//! external port is different for every destination, so the address rung 2
//! learned describes a mapping the peer will never reach. Attempting it anyway
//! burns several seconds to fail in a way already known in advance, so the plan
//! skips that family's punch rung and leaves the family to the blast.
//!
//! The plan reads **both ends'** facts, and that is deliberate. A family one
//! end has no address in is hopeless for every method, and the reason says
//! whose address is missing. The birthday blast is worth its noise only when
//! at least one end's NAT in that family is symmetric or unclassified; that
//! rule depends on the unordered pair of NATs, so client and host, each
//! planning from its own point of view, reach the same decision about which
//! family to blast. Were one end to blast while the other sat waiting, the
//! blast would be aimed at nobody.
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

use oxutrm_proto::{Candidate, Family, FamilyNat, NatType, Rung};

/// What one end of the connection knows about itself, as far as planning is
/// concerned.
///
/// Both ends build one: ours from our own classification and candidates, the
/// peer's from the NAT types in its hello and the candidates it offered. The
/// plan is a pure function of the pair, which is what lets the two ends agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndFacts {
    /// What STUN typing concluded about this end's NAT, per family.
    pub nat: FamilyNat,
    /// Whether this end has any IPv4 address to offer.
    pub v4: bool,
    /// Whether this end has any IPv6 address to offer.
    pub v6: bool,
}

impl EndFacts {
    /// Facts for an end offering `candidates`: a family is present when any
    /// candidate is in it.
    #[must_use]
    pub fn from_candidates(nat: FamilyNat, candidates: &[Candidate]) -> EndFacts {
        let has = |f: Family| candidates.iter().any(|c| Family::of(&c.addr) == f);
        EndFacts {
            nat,
            v4: has(Family::V4),
            v6: has(Family::V6),
        }
    }

    /// Whether this end has an address in `f`.
    #[must_use]
    pub fn has(&self, f: Family) -> bool {
        match f {
            Family::V4 => self.v4,
            Family::V6 => self.v6,
        }
    }
}

/// Which rungs to try, grouped by whether they can be raced.
///
/// A plan is decided from both ends' facts before anything is attempted, so
/// the decision is inspectable and testable on its own.
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
    /// The plan for two ends' facts, decided per address family.
    ///
    /// `NatType::Unknown` is treated as "might be anything", so nothing is
    /// skipped on its account: guessing wrong towards skipping costs a
    /// connection, while guessing wrong towards attempting costs a few seconds.
    #[must_use]
    pub fn for_ends(ours: &EndFacts, theirs: &EndFacts) -> LadderPlan {
        let mut skipped: Vec<(Rung, String)> = Vec::new();

        for family in Family::BOTH {
            let label = family.label();
            let family_rungs = || {
                Rung::ALL
                    .into_iter()
                    .filter(move |r| r.family() == Some(family))
            };

            // A family one end lacks cannot carry any method at all.
            let missing = if !ours.has(family) {
                Some(format!("this end has no {label} address"))
            } else if !theirs.has(family) {
                Some(format!("the peer has no {label} address"))
            } else {
                None
            };
            if let Some(why) = missing {
                for rung in family_rungs() {
                    skipped.push((rung, why.clone()));
                }
                continue;
            }

            // Rung 2 cannot work behind a symmetric NAT: the external port
            // differs per destination, so the address it learned names a
            // mapping the peer will never reach. Only *our* NAT is consulted:
            // the peer's own plan skips its punch rung for its own symmetry.
            if ours.nat.get(family) == NatType::Symmetric {
                skipped.push((
                    Rung::Punched(family),
                    format!(
                        "the {label} NAT is symmetric, so its external port differs per \
                         destination and ordinary punching cannot reach it"
                    ),
                ));
            }

            // The blast is noisy, so it is planned only when ordinary punching
            // might not do: some end's NAT in this family is symmetric, or
            // unclassified and so possibly symmetric. This reads the unordered
            // pair, so both ends decide alike.
            let might_need_blast = |n: NatType| matches!(n, NatType::Symmetric | NatType::Unknown);
            if !might_need_blast(ours.nat.get(family)) && !might_need_blast(theirs.nat.get(family))
            {
                skipped.push((
                    Rung::Birthday(family),
                    format!(
                        "neither end's {label} NAT is symmetric, so ordinary punching covers it"
                    ),
                ));
            }
        }

        let is_skipped = |r: &Rung| skipped.iter().any(|(s, _)| s == r);

        // `raced` names the candidate classes the one ICE agent should gather
        // and check together, not things to spawn. See the module note. Every
        // method appears once per family, IPv6 first.
        let raced: Vec<Rung> = Rung::ALL
            .into_iter()
            .filter(|r| matches!(r, Rung::Direct(_) | Rung::PortMapped(_) | Rung::Punched(_)))
            .filter(|r| !is_skipped(r))
            .collect();

        // The blast is deliberately noisy, so it never joins the race. It is
        // reached only when the cheap rungs have produced nothing -- or gone to
        // first when punching is already known to be hopeless. The tunnel is
        // always last.
        let mut sequential: Vec<Rung> = Rung::ALL
            .into_iter()
            .filter(|r| matches!(r, Rung::Birthday(_)))
            .filter(|r| !is_skipped(r))
            .collect();
        sequential.push(Rung::SshTunnel);

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

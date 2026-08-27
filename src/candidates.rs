//! The two candidate channels, and why neither of them is optional plumbing.
//!
//! ICE does not stop discovering when the hello has been sent. The agent emits
//! `NewLocalCandidate` as it learns its own peer-reflexive addresses, and the
//! peer does the same and sends them over as `CandidateUpdate`s. Both halves of
//! the connection run these pumps, in opposite roles, for as long as the ladder
//! is racing.
//!
//! **Dropping either direction looks exactly like a working link** until
//! somebody's NAT needs the candidate that was dropped, and then it looks like
//! a network fault. That is why each direction has a test that notices.
//!
//! They are two independent functions rather than one `select!` loop on
//! purpose. A `select!` cancels the arm that did not win, and cancelling a
//! half-finished write leaves a truncated line in front of the next message,
//! while cancelling a half-finished `read_line` silently eats the bytes it had
//! already buffered. Split, each direction can be *finished* instead: the
//! writer ends when the ladder drops its sender, and the reader ends by handing
//! back the message the caller was waiting for.

use oxutrm_host::signalling::write_signal_async;
use oxutrm_proto::{Candidate, Signal};
use tokio::io::AsyncWrite;

/// Send every candidate the ladder discovers to the peer, as a
/// `CandidateUpdate`, for as long as the ladder is still discovering them.
///
/// Ends when `learned` closes, which is what dropping the ladder's sender
/// does — so the writer comes back **unborrowed and mid-line-free**, ready for
/// `Established`. Nothing is cancelled, so no half-written line can precede it.
pub(crate) async fn outbound_candidates<W>(
    writer: &mut W,
    learned: &mut tokio::sync::mpsc::Receiver<Candidate>,
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(candidate) = learned.recv().await {
        write_signal_async(
            writer,
            &Signal::CandidateUpdate {
                candidates: vec![candidate],
            },
        )
        .await?;
    }
    Ok(())
}

/// Feed the ladder every candidate the peer discovers after its hello.
///
/// Returns the first signal that is **not** a `CandidateUpdate`, because that
/// is the one the caller was waiting for and swallowing it would lose the end
/// of the handshake. End of stream is an error: the peer hanging up mid-race
/// is not a nomination.
pub(crate) async fn inbound_candidates<R>(
    reader: &mut R,
    inbound: &tokio::sync::mpsc::Sender<Candidate>,
) -> anyhow::Result<Signal>
where
    R: tokio::io::AsyncBufReadExt + Unpin,
{
    loop {
        match oxutrm_host::signalling::read_signal_async(reader).await? {
            Signal::CandidateUpdate { candidates } => {
                for c in candidates {
                    // A closed receiver means the ladder has already settled,
                    // which is not an error -- there is simply nobody left to
                    // tell. Anything else the peer sends still matters.
                    if inbound.send(c).await.is_err() {
                        break;
                    }
                }
            }
            other => return Ok(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::CandidateKind;

    fn candidate(port: u16) -> Candidate {
        Candidate {
            addr: format!("192.0.2.1:{port}").parse().expect("a test address"),
            kind: CandidateKind::PeerReflexive,
            priority: 1,
        }
    }

    /// The half the handoff warns about: the agent emits `NewLocalCandidate`
    /// whether or not anyone is listening, so a pump that drops them is silent.
    /// Deleting the `write_signal_async` call below makes this test fail and
    /// nothing else in the tree notice.
    #[tokio::test]
    async fn every_learned_candidate_reaches_the_peer_as_a_candidate_update() {
        let (mut ours, theirs) = tokio::io::duplex(64 * 1024);
        let (learned_tx, mut learned_rx) = tokio::sync::mpsc::channel(8);

        learned_tx.send(candidate(4001)).await.expect("send one");
        learned_tx.send(candidate(4002)).await.expect("send two");
        drop(learned_tx);

        outbound_candidates(&mut ours, &mut learned_rx)
            .await
            .expect("the pump must finish cleanly when the ladder is done");
        drop(ours);

        let mut peer = tokio::io::BufReader::new(theirs);
        let mut ports = Vec::new();
        while let Ok(signal) = oxutrm_host::signalling::read_signal_async(&mut peer).await {
            match signal {
                Signal::CandidateUpdate { candidates } => {
                    ports.extend(candidates.iter().map(|c| c.addr.port()));
                }
                other => panic!("the pump wrote something that is not an update: {other:?}"),
            }
        }
        assert_eq!(
            ports,
            vec![4001, 4002],
            "candidates the ladder learned never reached the peer"
        );
    }

    /// A pipe already holding `signals`, ready to be read as a peer would
    /// have written them.
    async fn peer_wrote(signals: &[Signal]) -> tokio::io::BufReader<tokio::io::DuplexStream> {
        let (ours, mut theirs) = tokio::io::duplex(64 * 1024);
        for s in signals {
            oxutrm_host::signalling::write_signal_async(&mut theirs, s)
                .await
                .expect("the peer writes its own signals");
        }
        drop(theirs);
        tokio::io::BufReader::new(ours)
    }

    /// The other half the handoff warns about. A candidate that turns up
    /// *during* the race is the one that rescues a one-sided port mapping, and
    /// a pump that reads it and drops it looks identical to one that works.
    #[tokio::test]
    async fn a_candidate_that_arrives_during_the_race_reaches_the_ladder() {
        let mut reader = peer_wrote(&[
            Signal::CandidateUpdate {
                candidates: vec![candidate(5001)],
            },
            Signal::CandidateUpdate {
                candidates: vec![candidate(5002)],
            },
            Signal::Failed {
                reason: "stop".to_owned(),
            },
        ])
        .await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        inbound_candidates(&mut reader, &tx)
            .await
            .expect("the pump must return the terminating signal");

        let mut ports = Vec::new();
        while let Ok(c) = rx.try_recv() {
            ports.push(c.addr.port());
        }
        assert_eq!(
            ports,
            vec![5001, 5002],
            "candidates that arrived mid-race never reached the ladder"
        );
    }

    /// The terminating signal is what the caller is waiting for. A pump that
    /// consumed it would leave the caller reading a stream that has nothing
    /// left to say.
    #[tokio::test]
    async fn the_signal_that_ends_the_race_is_handed_back_not_swallowed() {
        let mut reader = peer_wrote(&[Signal::Failed {
            reason: "the peer gave up".to_owned(),
        }])
        .await;
        let (tx, _rx) = tokio::sync::mpsc::channel(8);

        let signal = inbound_candidates(&mut reader, &tx)
            .await
            .expect("a terminating signal is not an error");
        match signal {
            Signal::Failed { reason } => assert_eq!(reason, "the peer gave up"),
            other => panic!("the pump handed back the wrong signal: {other:?}"),
        }
    }
}

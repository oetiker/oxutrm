//! The client's half of the session switcher's requests (switcher spec
//! §3.3, §3.4): one control stream per request on the live link, its
//! `Open`, and the one answer -- or, for a switch or a new sibling, the
//! attach exchange that follows.
//!
//! Nothing here changes the session. [`ask`] runs on a task of the loop's
//! and comes back as [`Answered`]; `ClientSession::answered` acts on it, so
//! make-before-break is structural: the old link carries the session until
//! a new one is `Established` and handed over whole.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::Duration;

use oxutrm_host::signalling::{read_answer_async, write_line_async};
use oxutrm_net::NetConfig;
use oxutrm_proto::{
    Answer, Name, Open, Reply, Request, ScreenState, SessionEntry, SessionId, TermSize,
};

use crate::connect::Established;

/// What the selector asks the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ask {
    Sessions,
    Switch { to: SessionId },
    New { name: Option<Name> },
    Kill { id: SessionId },
    Rename { id: SessionId, name: Option<Name> },
}

/// What came of an [`Ask`].
pub(crate) enum Answered {
    /// The host's sessions, oldest first.
    Sessions(Vec<SessionEntry>),
    /// A switch or a new sibling: a link to another session, up and ready
    /// to be swapped in.
    Landed(Box<Established>),
    /// A lobby started its shell and is this session now.
    Started(SessionEntry),
    /// The session is gone: its shell reaped, its entry removed.
    Killed(SessionId),
    Renamed(SessionEntry),
    /// Refused or failed, in words for the user.
    Failed(String),
}

impl std::fmt::Debug for Answered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Answered::Sessions(l) => f.debug_tuple("Sessions").field(l).finish(),
            Answered::Landed(e) => f.debug_tuple("Landed").field(&e.session_id).finish(),
            Answered::Started(e) => f.debug_tuple("Started").field(e).finish(),
            Answered::Killed(id) => f.debug_tuple("Killed").field(id).finish(),
            Answered::Renamed(e) => f.debug_tuple("Renamed").field(e).finish(),
            Answered::Failed(why) => f.debug_tuple("Failed").field(why).finish(),
        }
    }
}

/// How long a request that runs no attach exchange may take to be answered.
/// A kill waits out a shell's grace and its reaping on the host (3 s, then
/// up to 2 s more), a sibling's door adds its own bound: this sits clear of
/// them all. The QUIC idle timeout is off, so without it a request sent
/// into a path that has just gone dark would wait for ever.
pub(crate) const ANSWER_WITHIN: Duration = Duration::from_secs(15);

/// Ask `ask` over a fresh control stream of `conn`. `size` and `cfg` are
/// what an attach exchange runs with, for a switch or a new sibling.
pub(crate) async fn ask(
    conn: quinn::Connection,
    ask: Ask,
    size: TermSize,
    cfg: &NetConfig,
) -> Answered {
    let attaches = matches!(ask, Ask::Switch { .. } | Ask::New { .. });
    let bound = if attaches {
        crate::control::SEARCH_DEADLINE
    } else {
        ANSWER_WITHIN
    };
    match tokio::time::timeout(bound, asked(conn, ask, size, cfg)).await {
        Ok(Ok(answered)) => answered,
        Ok(Err(e)) => Answered::Failed(format!("{e:#}")),
        Err(_) => Answered::Failed("the host did not answer in time".to_string()),
    }
}

async fn asked(
    conn: quinn::Connection,
    ask: Ask,
    size: TermSize,
    cfg: &NetConfig,
) -> anyhow::Result<Answered> {
    use anyhow::Context as _;
    let req = match &ask {
        Ask::Sessions => Request::Sessions,
        Ask::Switch { to } => Request::Switch { to: *to },
        Ask::New { name } => Request::New { name: name.clone() },
        Ask::Kill { id } => Request::Kill { id: *id },
        Ask::Rename { id, name } => Request::Rename {
            id: *id,
            name: name.clone(),
        },
    };
    let (mut send, recv) = conn.open_bi().await.context("opening a control stream")?;
    write_line_async(&mut send, &Open::new(req))
        .await
        .context("asking the host")?;
    let mut recv = tokio::io::BufReader::new(recv);
    let answer = read_answer_async(&mut recv)
        .await
        .context("reading the host's answer")?;
    Ok(match (answer, ask) {
        (Answer::Reply(Reply::Refused(why)), _) => Answered::Failed(why),
        // The attach exchange's own first line: the rest of it follows on
        // this stream, run exactly as a first connect runs it.
        (Answer::Signal(first), Ask::Switch { .. } | Ask::New { .. }) => {
            let e = crate::connect::establish_from(first, recv, send, size, cfg, None).await?;
            Answered::Landed(Box::new(e))
        }
        (Answer::Reply(Reply::Sessions(list)), Ask::Sessions) => Answered::Sessions(list),
        (Answer::Reply(Reply::Entry(e)), Ask::New { .. }) => Answered::Started(e),
        (Answer::Reply(Reply::Done), Ask::Kill { id }) => Answered::Killed(id),
        (Answer::Reply(Reply::Entry(e)), Ask::Rename { .. }) => Answered::Renamed(e),
        (other, ask) => Answered::Failed(format!(
            "the host answered {ask:?} with {}",
            match other {
                Answer::Reply(r) => format!("{r:?}"),
                Answer::Signal(_) => "an attach exchange".to_string(),
            }
        )),
    })
}

/// How long a preview may take to come back (preview spec §1.1): a door's
/// own bound on its loop or its sibling, [`crate::door::SIBLING_TIMEOUT`],
/// and the round trip on top. The QUIC idle timeout is off, so without it
/// a request sent into a path that has just gone dark would wait for ever.
pub(crate) const PREVIEW_WITHIN: Duration = Duration::from_secs(5);

/// Ask for session `id`'s screen over a fresh control stream of `conn`
/// (preview spec §3.4): its window as the host cut it, or why there is
/// none. Unchecked: the caller validates what it keeps.
pub(crate) async fn preview(conn: quinn::Connection, id: SessionId) -> Result<ScreenState, String> {
    let asked = async {
        use anyhow::Context as _;
        let (mut send, recv) = conn.open_bi().await.context("opening a control stream")?;
        write_line_async(&mut send, &Open::new(Request::Preview { id }))
            .await
            .context("asking the host")?;
        read_answer_async(&mut tokio::io::BufReader::new(recv))
            .await
            .context("reading the host's answer")
    };
    match tokio::time::timeout(PREVIEW_WITHIN, asked).await {
        Ok(Ok(Answer::Reply(Reply::Preview(s)))) => Ok(s),
        Ok(Ok(Answer::Reply(Reply::Refused(why)))) => Err(why),
        Ok(Ok(Answer::Reply(r))) => Err(format!("the host answered a preview with {r:?}")),
        Ok(Ok(Answer::Signal(_))) => {
            Err("the host answered a preview with an attach exchange".to_string())
        }
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(_) => Err("the host did not answer in time".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::fixtures::link_pair;
    use oxutrm_host::signalling::read_line_async;
    use std::time::Instant;

    const LOGS: &str = "5d2e8a7c1b9f40e3a6c8d1f2b3e4a5c6";

    /// A host end that reads one request and answers it with `reply`, or
    /// never answers when there is none. Returns what it was asked.
    fn host(conn: quinn::Connection, reply: Option<Reply>) -> tokio::task::JoinHandle<Request> {
        tokio::spawn(async move {
            let (mut send, recv) = conn.accept_bi().await.unwrap();
            let open: Open = read_line_async(&mut tokio::io::BufReader::new(recv))
                .await
                .unwrap();
            match reply {
                Some(r) => write_line_async(&mut send, &r).await.unwrap(),
                None => std::future::pending::<()>().await,
            }
            open.req
        })
    }

    #[tokio::test]
    async fn a_preview_brings_back_the_screen_the_host_sent() {
        let (h, c) = link_pair().await;
        let shot = ScreenState::blank(18, 68).unwrap();
        let asked = host(
            h.sink.connection().clone(),
            Some(Reply::Preview(shot.clone())),
        );
        let id: SessionId = LOGS.parse().unwrap();
        assert_eq!(preview(c.sink.connection().clone(), id).await, Ok(shot));
        assert_eq!(asked.await.unwrap(), Request::Preview { id });
    }

    #[tokio::test]
    async fn a_refused_preview_says_why() {
        let (h, c) = link_pair().await;
        let _asked = host(
            h.sink.connection().clone(),
            Some(Reply::Refused(
                "no session 5d2e8a7c on this host".to_string(),
            )),
        );
        assert_eq!(
            preview(c.sink.connection().clone(), LOGS.parse().unwrap()).await,
            Err("no session 5d2e8a7c on this host".to_string())
        );
    }

    #[tokio::test]
    async fn a_preview_the_host_never_answers_fails_in_time() {
        let (h, c) = link_pair().await;
        let _asked = host(h.sink.connection().clone(), None);
        let begun = Instant::now();
        assert_eq!(
            preview(c.sink.connection().clone(), LOGS.parse().unwrap()).await,
            Err("the host did not answer in time".to_string())
        );
        let took = begun.elapsed();
        assert!(
            took >= PREVIEW_WITHIN && took < PREVIEW_WITHIN + Duration::from_secs(2),
            "{took:?}"
        );
    }
}

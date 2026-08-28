//! `oxutrm host --list` and `oxutrm host --attach <id>`.
//!
//! Reattachment is not a second code path. `--attach` connects to the running
//! session's Unix socket and relays the same `Signal` traffic a first connect
//! would carry, so the session performs a fresh ICE exchange and the client
//! cannot tell the two apart. Anything that only worked on reattach would be
//! untested by every ordinary connect, which is why there is nothing here that
//! only works on reattach.

use std::path::{Path, PathBuf};

use oxutrm_proto::ProtoError;
use tokio::io::{AsyncBufReadExt, AsyncWrite};
use tokio::net::UnixStream;

use crate::registry::{Registry, SessionMeta, check_socket_path_length};
use crate::signalling::{read_signal_async, write_signal_async};

/// Why an attach did not happen.
///
/// Separated the same way the bootstrap errors are, and for the same reason:
/// each one sends the user somewhere different.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    /// No such session. Listing what *does* exist is far more useful than
    /// saying no, because the usual cause is a mistyped or truncated id.
    #[error("no session {id} here. {}", describe_alternatives(.available))]
    UnknownSession { id: String, available: Vec<String> },

    /// The session exists but was never able to detach, so it died with the
    /// ssh connection that created it. Its registry entry may still be here.
    #[error(
        "session {id} is not detachable: it tunnels its data through the ssh \
         connection that created it (rung 4), so it cannot outlive it and \
         cannot be reattached. Start a new session."
    )]
    NotDetachable { id: String },

    /// The entry is there and the socket is not answering. Usually a session
    /// killed without cleanup; `--list` prunes those on its next run.
    #[error("session {id} has a socket at {} that is not answering: {source}. \
             The process may have been killed; `oxutrm host --list` will prune it.",
            path.display())]
    SocketUnreachable {
        id: String,
        path: PathBuf,
        source: std::io::Error,
    },

    /// The registry itself could not be read. `anyhow::Error` deliberately does
    /// not implement `std::error::Error`, so it is flattened here rather than
    /// chained.
    #[error("reading the registry: {0}")]
    Registry(String),

    #[error("{0}")]
    Protocol(#[from] ProtoError),

    #[error("{0}")]
    Io(#[from] std::io::Error),
}

fn describe_alternatives(available: &[String]) -> String {
    if available.is_empty() {
        "There are no live sessions on this host.".to_string()
    } else {
        format!("Live sessions: {}.", available.join(", "))
    }
}

/// Connect to a running session's socket.
///
/// Checks the registry first, so "no such session" and "the session is dead"
/// are told apart before anything touches the filesystem in anger.
pub async fn connect_to_session(root: &Path, id: &str) -> Result<UnixStream, AttachError> {
    let live = Registry::list_in(root).map_err(|e| AttachError::Registry(format!("{e:#}")))?;
    let Some(meta) = live.iter().find(|m| m.session_id == id) else {
        return Err(AttachError::UnknownSession {
            id: id.to_string(),
            available: live.into_iter().map(|m| m.session_id).collect(),
        });
    };

    if !meta.detachable {
        // Recorded from the nominated rung, not from the handshake's intent.
        return Err(AttachError::NotDetachable { id: id.to_string() });
    }

    let path = Registry::socket_path_in(root, id);
    check_socket_path_length(&path).map_err(|e| AttachError::Registry(format!("{e:#}")))?;

    UnixStream::connect(&path)
        .await
        .map_err(|source| AttachError::SocketUnreachable {
            id: id.to_string(),
            path,
            source,
        })
}

/// Pump `Signal` messages from one side to the other until the source ends.
///
/// Returns how many messages were relayed. A clean end of stream is success:
/// the far end finished, which is what happens every time signalling completes.
///
/// Messages are decoded and re-encoded rather than copied as bytes. That costs
/// a little and buys the thing worth having: garbage cannot be relayed into a
/// running session, and a version mismatch is caught at the relay rather than
/// deep inside a session that has already started trusting it.
pub async fn relay_signals<R, W>(from: &mut R, to: &mut W) -> Result<u64, AttachError>
where
    R: AsyncBufReadExt + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut relayed = 0u64;
    loop {
        match read_signal_async(from).await {
            Ok(s) => {
                write_signal_async(to, &s).await?;
                relayed += 1;
            }
            Err(ProtoError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(relayed);
            }
            Err(e) => return Err(AttachError::Protocol(e)),
        }
    }
}

/// One line per session, for `oxutrm host --list`.
///
/// Detachability is shown rather than implied. A session that cannot be
/// reattached looks identical to one that can until you try, and finding out by
/// trying is the worst moment to find out.
#[must_use]
pub fn format_session_list(sessions: &[SessionMeta]) -> String {
    if sessions.is_empty() {
        return "no live oxutrm sessions on this host\n".to_string();
    }
    let mut out = String::new();
    for m in sessions {
        out.push_str(&format!(
            "{}  {:>7}  {:>3}x{:<3}  attach {}  {}  {}\n",
            m.session_id,
            m.pid,
            m.size.cols,
            m.size.rows,
            m.attach_id,
            m.shell,
            if m.detachable {
                "detachable"
            } else {
                "NOT detachable (dies with its ssh)"
            },
        ));
    }
    out
}

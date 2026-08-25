//! `Signal` messages over an asynchronous pipe.
//!
//! The wire format, the strictness rules and the version check all belong to
//! `oxutrm-proto`. This module adds exactly one thing: the ability to read and
//! write them over a `tokio` stream rather than a blocking one, because the
//! signalling channel is a pair of pipes on a child `ssh` and the attach path
//! is a Unix socket.
//!
//! # Why this does not reimplement the parser
//!
//! Deciding what counts as a `Signal` — as opposed to an SSH banner, a motd, or
//! `stty` complaining about a missing tty — is policy, and duplicating policy
//! is how two copies drift apart. So [`read_signal_async`] reads one line
//! asynchronously and hands that single line to
//! [`oxutrm_proto::read_signal`] over a one-line cursor.
//!
//! That gives the skipping behaviour for free, with a pleasant consequence: if
//! the line was preamble, `read_signal` skips it and immediately runs out of
//! cursor, reporting `UnexpectedEof`. So "this line was noise" and "keep
//! reading" become the same branch, and every other error — malformed JSON,
//! version skew — propagates exactly as `oxutrm-proto` decided it should.

use oxutrm_proto::{ProtoError, Signal};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Read one `Signal`, discarding whatever the remote login printed first.
///
/// Real SSH emits a banner before authentication, a motd after it, and
/// `stty: standard input: Inappropriate ioctl for device` when the command runs
/// without a tty. A single un-skipped line of that breaks every connection —
/// and none of it appears on a quiet developer machine, which is why
/// `tests/signalling.rs` supplies it deliberately.
///
/// A line that *looks* like a signal is parsed strictly: malformed JSON and
/// version skew are reported, never skipped. The corollary is that a motd line
/// beginning with `{` breaks the bootstrap, which is the safe direction to fail
/// in — silently discarding a bad `HostHello` would hide the one failure that
/// has to be loudest.
///
/// End of stream is `ProtoError::Io` with `ErrorKind::UnexpectedEof`, so a peer
/// that hung up cleanly can be told from one that sent rubbish.
pub async fn read_signal_async<R>(r: &mut R) -> Result<Signal, ProtoError>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).await? == 0 {
            return Err(ProtoError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "signalling stream closed before a message arrived",
            )));
        }

        // One line, one cursor. `read_signal` applies the same skipping and
        // strictness rules it applies to a blocking stream; running out of
        // cursor means the line was preamble.
        let mut one_line = std::io::Cursor::new(line.as_bytes());
        match oxutrm_proto::read_signal(&mut one_line) {
            Ok(s) => return Ok(s),
            Err(ProtoError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Write one `Signal` and flush it.
///
/// The flush is not optional: the peer is waiting on a pipe, and a `HostHello`
/// sitting in a buffer looks exactly like a host that never answered.
pub async fn write_signal_async<W>(w: &mut W, s: &Signal) -> Result<(), ProtoError>
where
    W: AsyncWrite + Unpin,
{
    // Reuse the blocking encoder so the bytes on the wire are identical
    // whichever side wrote them.
    let mut buf: Vec<u8> = Vec::new();
    oxutrm_proto::write_signal(&mut buf, s)?;
    w.write_all(&buf).await.map_err(ProtoError::Io)?;
    w.flush().await.map_err(ProtoError::Io)?;
    Ok(())
}

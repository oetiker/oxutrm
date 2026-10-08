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

use oxutrm_proto::{Answer, MAX_SIGNAL_LINE, ProtoError, Signal};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
    loop {
        let mut raw = Vec::new();
        // The one rule this module does NOT delegate, because it cannot: a
        // limit checked by `read_signal` is checked on bytes we have already
        // read and are already holding. `take` caps the read itself, so a peer
        // that never sends a newline costs us `MAX_SIGNAL_LINE` bytes and not
        // one more. The *number* still comes from `oxutrm-proto`, so the two
        // readers cannot draw the line in different places.
        let taken = AsyncReadExt::take(&mut *r, MAX_SIGNAL_LINE as u64)
            .read_until(b'\n', &mut raw)
            .await?;
        if taken == 0 {
            return Err(ProtoError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "signalling stream closed before a message arrived",
            )));
        }
        // A spent budget with no terminator means the line needs at least one
        // byte more than the limit allows. A *short* read without one is the
        // ordinary last line of a stream that simply ended, and stays legal.
        if taken == MAX_SIGNAL_LINE && raw.last() != Some(&b'\n') {
            return Err(ProtoError::SignalLineTooLong {
                limit: MAX_SIGNAL_LINE,
            });
        }

        // One line, one cursor. `read_signal` applies the same skipping and
        // strictness rules it applies to a blocking stream - UTF-8 included;
        // running out of cursor means the line was preamble.
        let mut one_line = std::io::Cursor::new(raw.as_slice());
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

/// One line, up to and including its newline, never more than
/// [`MAX_SIGNAL_LINE`] bytes of it. End of stream before any byte is
/// `UnexpectedEof`, as for [`read_signal_async`].
async fn read_bounded_line<R>(r: &mut R) -> Result<Vec<u8>, ProtoError>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut raw = Vec::new();
    let taken = AsyncReadExt::take(&mut *r, MAX_SIGNAL_LINE as u64)
        .read_until(b'\n', &mut raw)
        .await?;
    if taken == 0 {
        return Err(ProtoError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "the stream closed before a line arrived",
        )));
    }
    if taken == MAX_SIGNAL_LINE && raw.last() != Some(&b'\n') {
        return Err(ProtoError::SignalLineTooLong {
            limit: MAX_SIGNAL_LINE,
        });
    }
    Ok(raw)
}

/// Read one door line -- an `Open`, a `Reply` -- strictly: the peer speaks
/// the protocol from its first byte, so nothing is skipped (switcher spec
/// §4.1). Bounded like a signal.
pub async fn read_line_async<R, T>(r: &mut R) -> Result<T, ProtoError>
where
    R: AsyncBufReadExt + Unpin,
    T: DeserializeOwned,
{
    let raw = read_bounded_line(r).await?;
    oxutrm_proto::parse_line(&raw)
}

/// Read what answers an `Open` that may run an attach exchange: its first
/// `Signal`, or a `Reply`.
pub async fn read_answer_async<R>(r: &mut R) -> Result<Answer, ProtoError>
where
    R: AsyncBufReadExt + Unpin,
{
    let raw = read_bounded_line(r).await?;
    oxutrm_proto::parse_answer(&raw)
}

/// Write one door line and flush it.
pub async fn write_line_async<W, T>(w: &mut W, value: &T) -> Result<(), ProtoError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let line = oxutrm_proto::encode_line(value)?;
    w.write_all(&line).await.map_err(ProtoError::Io)?;
    w.flush().await.map_err(ProtoError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::{Open, Reply, Request};

    #[tokio::test]
    async fn a_door_line_round_trips_and_a_banner_is_not_skipped() {
        let mut buf = Vec::new();
        write_line_async(&mut buf, &Open::new(Request::Sessions))
            .await
            .unwrap();
        let mut r = tokio::io::BufReader::new(buf.as_slice());
        let back: Open = read_line_async(&mut r).await.unwrap();
        assert_eq!(back.req, Request::Sessions);

        // A door has no preamble: a banner line is malformed, not noise.
        let mut r = tokio::io::BufReader::new(&b"Welcome to Ubuntu\n"[..]);
        assert!(matches!(
            read_line_async::<_, Open>(&mut r).await,
            Err(ProtoError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn an_answer_is_a_reply_or_a_signal() {
        let mut buf = Vec::new();
        write_line_async(&mut buf, &Reply::Refused("gone".into()))
            .await
            .unwrap();
        let mut r = tokio::io::BufReader::new(buf.as_slice());
        assert!(matches!(
            read_answer_async(&mut r).await.unwrap(),
            Answer::Reply(Reply::Refused(_))
        ));
    }

    #[tokio::test]
    async fn an_endless_door_line_is_refused_at_the_limit() {
        let endless = vec![b'x'; MAX_SIGNAL_LINE + 10];
        let mut r = tokio::io::BufReader::new(endless.as_slice());
        assert!(matches!(
            read_line_async::<_, Open>(&mut r).await,
            Err(ProtoError::SignalLineTooLong { .. })
        ));
    }
}

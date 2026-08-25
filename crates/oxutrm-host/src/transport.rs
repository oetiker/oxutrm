//! The path a session's datagrams actually travel over, and its size limit.
//!
//! A session loop must not know whether it is on a punched UDP socket or on a
//! tunnel inside the ssh connection. It sends a `Frame`, it gets a `Frame`, and
//! it asks the path how large a payload the path will carry. That last part is
//! the whole reason this module exists.
//!
//! # The size limit is a property of the path, never a constant
//!
//! The tempting shape is `conn.max_datagram_size().unwrap_or(1200)`, and it is
//! wrong twice over.
//!
//! It is wrong on the **tunnel**, whose limit is smaller than a datagram's: a
//! 1200-byte fallback silently exceeds what the tunnel accepts, and the failure
//! surfaces far from its cause.
//!
//! It is wrong on **QUIC** too, because `max_datagram_size()` returning `None`
//! does not mean "unknown, guess something safe". It means the peer disabled
//! datagrams, so there is no size that works and substituting one converts a
//! clear error into a mystery.
//!
//! So [`Path`] exposes [`Path::max_payload`] returning a plain `usize`, decided
//! once when the path is built. There is no `Option` at the send site, so there
//! is no `unwrap_or` for anyone to write — the bug is unavailable rather than
//! discouraged.
//!
//! # Why an enum and not a trait
//!
//! There are exactly two kinds of path and the set is closed. An enum makes a
//! third one impossible to add without touching this file, which is precisely
//! where somebody must think about the size limit again. A `dyn` trait would
//! let a new implementation forget.

use std::io;

use oxutrm_proto::Rung;

/// The largest payload the ssh tunnel will carry in one message.
///
/// Deliberately below a normal datagram. The tunnel adds its own 4-byte length
/// prefix and rides inside ssh's own framing, so the usable room is smaller
/// than on the wire it replaces — and a session that assumed otherwise would
/// build frames the tunnel has to reject.
pub const TUNNEL_MAX_PAYLOAD: usize = 1024;

/// The length prefix each tunnelled message carries.
const LENGTH_PREFIX: usize = 4;

/// Why a path could not be built or used.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// `max_datagram_size()` was `None`. Not a missing number to guess at: the
    /// peer turned datagrams off, and every send would fail.
    #[error(
        "this QUIC connection has datagrams disabled, so screen state cannot be \
         sent unreliably over it. The peer did not enable them, or \
         TransportConfig left datagram_receive_buffer_size unset."
    )]
    DatagramsDisabled,

    /// Somebody built a frame larger than the path accepts. A bug in frame
    /// generation rather than a runtime condition, so it is loud.
    #[error(
        "a {len}-byte payload does not fit this {kind} path, which accepts \
         {max}. The sender must ask the path for its limit rather than assuming \
         a datagram-sized one."
    )]
    TooLarge {
        len: usize,
        max: usize,
        kind: &'static str,
    },

    #[error("{0}")]
    Io(#[from] io::Error),
}

/// How a session's datagrams travel.
///
/// The variants exist so a session loop does not have to care which it holds.
/// What it must care about — the size limit — is asked of the path itself.
#[derive(Debug)]
pub enum Path {
    /// A real UDP socket, punched or direct. Rungs 0 to 3.
    Datagram {
        /// Decided when the path was built, from the live connection.
        max_payload: usize,
        rung: Rung,
    },
    /// QUIC inside a stream on the ssh connection. Rung 4.
    ///
    /// Slower, and it dies when the ssh connection does, which is why a session
    /// on it can neither roam nor be reattached.
    Tunnel,
}

impl Path {
    /// A datagram path over a live QUIC connection.
    ///
    /// `max_datagram_size` is the value the connection reports. `None` is an
    /// error rather than a cue to guess: it means datagrams are off.
    pub fn datagram(rung: Rung, max_datagram_size: Option<usize>) -> Result<Path, PathError> {
        let max_payload = max_datagram_size.ok_or(PathError::DatagramsDisabled)?;
        Ok(Path::Datagram { max_payload, rung })
    }

    /// The rung-4 fallback.
    #[must_use]
    pub fn tunnel() -> Path {
        Path::Tunnel
    }

    /// Which rung this path represents.
    #[must_use]
    pub fn rung(&self) -> Rung {
        match self {
            Path::Datagram { rung, .. } => *rung,
            Path::Tunnel => Rung::SshTunnel,
        }
    }

    /// The largest payload this path will carry, in bytes.
    ///
    /// Always a number, never an `Option`, and never a constant chosen by the
    /// caller. Every frame builder asks the path.
    #[must_use]
    pub fn max_payload(&self) -> usize {
        match self {
            Path::Datagram { max_payload, .. } => *max_payload,
            // Reported by the tunnel itself, so a caller cannot substitute a
            // datagram-sized guess.
            Path::Tunnel => TUNNEL_MAX_PAYLOAD,
        }
    }

    /// Can a session on this path outlive the ssh connection that created it?
    ///
    /// The same question [`crate::detachable_for_rung`] answers, asked of a
    /// live path rather than of a rung number.
    #[must_use]
    pub fn is_detachable(&self) -> bool {
        crate::detachable_for_rung(self.rung())
    }

    fn kind(&self) -> &'static str {
        match self {
            Path::Datagram { .. } => "datagram",
            Path::Tunnel => "tunnel",
        }
    }

    /// Check a payload against this path before it is sent.
    ///
    /// Called by whatever builds frames. Returning an error rather than
    /// truncating is deliberate: a truncated frame decodes as garbage on the
    /// far side, which is far harder to diagnose than a refusal here.
    pub fn check_payload(&self, len: usize) -> Result<(), PathError> {
        let max = self.max_payload();
        if len > max {
            return Err(PathError::TooLarge {
                len,
                max,
                kind: self.kind(),
            });
        }
        Ok(())
    }
}

/// Frame one tunnelled message: a 4-byte big-endian length, then the payload.
///
/// The ssh channel is a byte stream with no message boundaries of its own, so
/// the tunnel supplies them. Refuses anything over [`TUNNEL_MAX_PAYLOAD`], so
/// an oversize frame cannot reach the wire and be discovered as a stall.
pub fn frame_tunnel_message(payload: &[u8]) -> Result<Vec<u8>, PathError> {
    Path::tunnel().check_payload(payload.len())?;
    let mut out = Vec::with_capacity(LENGTH_PREFIX + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Read one tunnelled message from a byte stream.
///
/// The length is validated against [`TUNNEL_MAX_PAYLOAD`] **before** any
/// allocation, so a corrupt or hostile prefix cannot ask for four gigabytes.
pub async fn read_tunnel_message<R>(r: &mut R) -> Result<Vec<u8>, PathError>
where
    R: tokio::io::AsyncReadExt + Unpin,
{
    let mut prefix = [0u8; LENGTH_PREFIX];
    r.read_exact(&mut prefix).await?;
    let len = u32::from_be_bytes(prefix) as usize;

    // Checked before allocating. A length prefix arrives from the far side of
    // an ssh connection, so it is input, not a value we chose.
    Path::tunnel().check_payload(len)?;

    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    Ok(payload)
}

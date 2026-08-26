//! Key material, fresh for every attach, never on disk.
//!
//! The trust root is ssh, unchanged. What travels over it is a PSK and the
//! fingerprint of a self-signed certificate, and both are regenerated on every
//! attach — so a key captured from an earlier attach cannot be used to reattach
//! later. That property is worth more than the small cost of making a new one:
//! it means a session's security does not decay the longer it stays alive.
//!
//! Nothing here is ever written to the registry.
//! `tests/no_keys_on_disk.rs` enforces that by reading every byte of every file
//! under it.

use oxutrm_proto::{HostSpki, Psk};
use rand::TryRngCore as _;

use crate::SessionMeta;

/// The length of a PSK, in bytes. 32 bytes from the OS CSPRNG.
///
/// Re-exported from the wire crate rather than restated, because the wire is
/// what decides it: `oxutrm_proto::Psk` cannot hold any other length.
pub const PSK_LEN: usize = oxutrm_proto::WIRE_KEY_LEN;

/// The secrets of one attach.
///
/// The PSK is generated here, because minting it is this crate's job. The
/// certificate fingerprint is **passed in** rather than invented, because the
/// certificate itself is made by the transport layer that will present it —
/// generating a plausible-looking fingerprint here would be a fake that reads
/// like the real thing.
///
/// Both halves are held in the **wire crate's** types, not in bare arrays.
/// That is the point of the change that introduced them: these values are
/// minted here and consumed at the far end, and while the mint side spoke
/// `[u8; 32]` and the wire spoke `String` there was nothing to join the two —
/// so nothing did.
pub struct AttachKeys {
    psk: Psk,
    cert_spki_sha256: HostSpki,
}

impl AttachKeys {
    /// A fresh PSK for this attach, paired with the fingerprint of the
    /// certificate the host will present.
    ///
    /// `OsRng` is the operating system's CSPRNG. It is used directly rather
    /// than through a seeded generator, because a seeded one can be reproduced
    /// and this value must not be.
    pub fn fresh(cert_spki_sha256: HostSpki) -> std::io::Result<AttachKeys> {
        let mut psk = [0u8; PSK_LEN];
        rand::rngs::OsRng
            .try_fill_bytes(&mut psk)
            .map_err(|e| std::io::Error::other(format!("the OS CSPRNG refused: {e}")))?;
        Ok(AttachKeys {
            psk: Psk::new(psk),
            cert_spki_sha256,
        })
    }

    /// The PSK, in the type that goes into `HostHello`.
    ///
    /// There is no `psk_base64()` beside this any more, and that is
    /// deliberate. Encoding now happens in exactly one place — `Psk`'s
    /// `Serialize` — so there is no second encoder to drift out of step with
    /// the decoder. A hand-rolled encode with no matching decode is what made
    /// this seam silently broken in the first place.
    #[must_use]
    pub fn psk(&self) -> &Psk {
        &self.psk
    }

    /// The certificate fingerprint, in the type that goes into `HostHello`.
    #[must_use]
    pub fn cert_spki_sha256(&self) -> HostSpki {
        self.cert_spki_sha256
    }
}

impl std::fmt::Debug for AttachKeys {
    /// Redacted, and hand-written for that reason. A derived `Debug` would put
    /// the PSK into the first log line or error message that formatted a struct
    /// containing one, which is exactly how secrets escape.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachKeys")
            .field("psk", &"<redacted>")
            .field("cert_spki_sha256", &"<redacted>")
            .finish()
    }
}

// There is no `Drop for AttachKeys`. `Psk` has one, so dropping an
// `AttachKeys` zeroes its PSK anyway — through the field, without this crate
// having to remember. The caveat the old implementation had to document is
// also narrower now: it said the value "may already have been copied by a
// base64 encode", and that copy was a `String` nothing could scrub. The encode
// happens on a 44-byte stack buffer inside `Psk::serialize`, which zeroes it
// before returning.

/// One attach generation: a bumped counter and a brand-new set of secrets.
#[derive(Debug)]
pub struct Attach {
    pub attach_id: u64,
    pub keys: AttachKeys,
}

/// Begin a new attach on an existing session.
///
/// Bumps `meta.attach_id` and mints fresh key material. Both halves matter and
/// they belong together:
///
/// * **Fresh keys** mean a PSK captured from a previous attach cannot reattach.
/// * **A bumped `attach_id`** means the two ends agree on which generation they
///   are in. Both `seq` counters reset to 1 at every attach, so without it a
///   host already serving a session could not tell a second `--attach` from the
///   current one, and stale datagrams from the previous generation would look
///   perfectly valid.
///
/// Note what this does **not** touch: `meta.detachable`. That is settled from
/// the nominated rung, long after this runs — see
/// [`SessionMeta::set_detachable`].
pub fn begin_attach(meta: &mut SessionMeta, cert_spki_sha256: HostSpki) -> std::io::Result<Attach> {
    meta.attach_id = meta.attach_id.saturating_add(1);
    Ok(Attach {
        attach_id: meta.attach_id,
        keys: AttachKeys::fresh(cert_spki_sha256)?,
    })
}

/// Proof that the rung has been nominated and this session is allowed to close
/// the descriptors it inherited from ssh.
///
/// It cannot be constructed except by [`settle_detachability`], and both
/// [`crate::sever_from_ssh`] and [`crate::daemonize_session`] demand one. That
/// is the ordering made structural rather than remembered: there is no way to
/// write a call that severs before the rung is known, because there is nothing
/// to pass.
///
/// The failure it prevents is not hypothetical. A rung-4 session carries its
/// QUIC traffic inside the ssh connection, and severing closes every inherited
/// descriptor — so a session that cut the pipes on the handshake's optimistic
/// intent would destroy the link it was about to use, and the symptom would be
/// a session that dies the moment it is left alone.
///
/// # What it gates, exactly
///
/// **Descriptor closure, and only that.** Detaching is two operations, and
/// [`crate::detach_process`] — fork, `setsid`, fork — deliberately needs no
/// permit: forking away from ssh is harmless for every rung, including rung 4,
/// because it touches no descriptor. A rung-4 session forks like any other and
/// then simply never severs, keeping its pipes and its ssh for life.
///
/// That is narrower than what this token gated when detaching was one function,
/// and it is not a weakening: closing the descriptors is the operation the
/// paragraph above describes, and it is now the operation the type is attached
/// to. The other ordering the split introduced — sever only *after* forking —
/// has its own token, [`crate::Detached`], for the same reason.
#[derive(Debug)]
pub struct DetachPermit {
    _private: (),
}

/// Settle detachability from the nominated rung, and say whether this session
/// may sever itself from ssh.
///
/// `Some` for every rung that carries its own UDP socket. `None` for
/// [`Rung::SshTunnel`], whose session must stay attached to the ssh connection
/// for its whole life.
///
/// Writes the outcome into `meta`, so `--list` reports what is true rather than
/// what was hoped for at handshake time.
pub fn settle_detachability(
    meta: &mut SessionMeta,
    rung: oxutrm_proto::Rung,
) -> Option<DetachPermit> {
    if meta.set_detachable(rung) {
        Some(DetachPermit { _private: () })
    } else {
        None
    }
}

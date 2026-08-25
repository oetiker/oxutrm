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

use base64::Engine as _;
use rand::TryRngCore as _;

use crate::SessionMeta;

/// The length of a PSK, in bytes. 32 bytes from the OS CSPRNG.
pub const PSK_LEN: usize = 32;

/// The secrets of one attach.
///
/// The PSK is generated here, because minting it is this crate's job. The
/// certificate fingerprint is **passed in** rather than invented, because the
/// certificate itself is made by the transport layer that will present it —
/// generating a plausible-looking fingerprint here would be a fake that reads
/// like the real thing.
pub struct AttachKeys {
    psk: [u8; PSK_LEN],
    cert_spki_sha256: [u8; 32],
}

impl AttachKeys {
    /// A fresh PSK for this attach, paired with the fingerprint of the
    /// certificate the host will present.
    ///
    /// `OsRng` is the operating system's CSPRNG. It is used directly rather
    /// than through a seeded generator, because a seeded one can be reproduced
    /// and this value must not be.
    pub fn fresh(cert_spki_sha256: [u8; 32]) -> std::io::Result<AttachKeys> {
        let mut psk = [0u8; PSK_LEN];
        rand::rngs::OsRng
            .try_fill_bytes(&mut psk)
            .map_err(|e| std::io::Error::other(format!("the OS CSPRNG refused: {e}")))?;
        Ok(AttachKeys {
            psk,
            cert_spki_sha256,
        })
    }

    #[must_use]
    pub fn psk(&self) -> &[u8; PSK_LEN] {
        &self.psk
    }

    /// The PSK as it travels in `HostHello`.
    #[must_use]
    pub fn psk_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.psk)
    }

    #[must_use]
    pub fn cert_spki_sha256(&self) -> &[u8; 32] {
        &self.cert_spki_sha256
    }

    /// The certificate fingerprint as it travels in `HostHello`.
    #[must_use]
    pub fn cert_spki_sha256_base64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.cert_spki_sha256)
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

impl Drop for AttachKeys {
    /// Overwrite the PSK before the memory is reused.
    ///
    /// A best effort rather than a guarantee, and worth being exact about what
    /// it is not: the value may already have been copied by a `base64` encode,
    /// nothing stops the allocator handing the page to someone else, and
    /// without a volatile write the compiler is entitled to elide a dead store.
    /// The fence makes elision unlikely rather than impossible.
    ///
    /// It is kept anyway because it costs one pass over 32 bytes and removes
    /// the longest-lived copy, which is the one worth removing. Doing it
    /// properly would mean `write_volatile` and a second `unsafe` module in a
    /// crate that currently has exactly one, which is a worse trade for a
    /// guarantee this cannot make either way.
    fn drop(&mut self) {
        self.psk.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

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
pub fn begin_attach(meta: &mut SessionMeta, cert_spki_sha256: [u8; 32]) -> std::io::Result<Attach> {
    meta.attach_id = meta.attach_id.saturating_add(1);
    Ok(Attach {
        attach_id: meta.attach_id,
        keys: AttachKeys::fresh(cert_spki_sha256)?,
    })
}

/// Proof that the rung has been nominated and this session is allowed to
/// detach.
///
/// It cannot be constructed except by [`settle_detachability`], and
/// [`crate::daemonize_session`] demands one. That is the ordering made
/// structural rather than remembered: there is no way to write a call that
/// daemonizes before the rung is known, because there is nothing to pass.
///
/// The failure it prevents is not hypothetical. A rung-4 session carries its
/// QUIC traffic inside the ssh connection, and `daemonize` closes every
/// inherited descriptor — so a session that detached on the handshake's
/// optimistic intent would destroy the link it was about to use, and the
/// symptom would be a session that dies the moment it is left alone.
#[derive(Debug)]
pub struct DetachPermit {
    _private: (),
}

/// Settle detachability from the nominated rung, and say whether this session
/// may daemonize.
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

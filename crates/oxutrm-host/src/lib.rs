// `daemonize` needs raw `fork`, `setsid` and `dup2`, which have no safe
// wrapper — `fork` in particular cannot have one, because what is sound after
// it depends on what the whole process was doing before. Every other module is
// held to the usual rule.
#![deny(unsafe_code)]

//! The remote half of a session: it owns the PTY and the authoritative screen,
//! survives the client going away, and can be found again on reattach.
//!
//! Detached is a normal state, not an error. A session that nobody is watching
//! keeps draining its PTY, transmits nothing, and costs no bandwidth for as
//! long as it is left alone.
//!
//! # The two things here that are easy to get quietly wrong
//!
//! **Where the registry lives.** `$XDG_RUNTIME_DIR` is `/run/user/<uid>`, and
//! systemd tears it down when the user's last login session ends. A session
//! that daemonized into a directory which then vanished is still running and
//! completely unreachable — no socket, no `--list` entry, no way back. So
//! [`registry::choose_registry_root`] uses the runtime directory only where
//! lingering is known to keep it alive, and says so loudly when it falls back.
//!
//! **When it is safe to detach.** [`daemonize`] closes every inherited
//! descriptor, which is exactly right for an ordinary session and fatal for a
//! rung-4 one, whose QUIC traffic runs inside the ssh connection those
//! descriptors belong to. Detachability is therefore settled from the
//! *nominated* rung with [`SessionMeta::set_detachable`], and only then may a
//! session daemonize.

pub mod attach;
pub mod daemon;
pub mod keys;
pub mod ladder;
pub mod registry;
pub mod signalling;
pub mod ssh;
pub mod transport;

pub use daemon::{daemonize, daemonize_session};
pub use keys::{Attach, AttachKeys, DetachPermit, PSK_LEN, begin_attach, settle_detachability};
pub use ladder::{LadderError, LadderPlan, RungResult, RungRunner, nominate, status_line};
pub use registry::{
    META_FILE, PID_REUSE_SLACK_SECS, REGISTRY_SUBDIR, Registry, RegistryGuard, RegistryRoot,
    RegistryRootKind, RootEnv, SOCK_FILE, SessionMeta, check_socket_path_length,
    choose_registry_root, detachable_for_rung, entry_is_stale, linger_enabled, now_unix, pid_alive,
    process_start_unix, read_root_env, resolve_registry_root,
};
pub use transport::{Path, PathError, TUNNEL_MAX_PAYLOAD};

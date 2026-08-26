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
//! **When it is safe to detach.** Detaching is two operations, and they are
//! not safe at the same moment. [`detach_process`] forks away from ssh and is
//! harmless for every rung; [`sever_from_ssh`] closes every inherited
//! descriptor, which is exactly right for an ordinary session and fatal for a
//! rung-4 one, whose QUIC traffic runs inside the ssh connection those
//! descriptors belong to. Detachability is therefore settled from the
//! *nominated* rung with [`SessionMeta::set_detachable`], and only then may a
//! session sever.
//!
//! Splitting them is what lets an async ICE ladder run at all: the fork must
//! happen before any thread exists, the rung cannot be known without a runtime,
//! and the pipes must stay open in between so candidates can still cross them.
//! [`daemonize`] remains the two phases back to back, for callers with nothing
//! left to say over ssh.

pub mod attach;
pub mod daemon;
pub mod keys;
pub mod ladder;
pub mod registry;
pub mod signalling;
pub mod ssh;

pub use daemon::{Detached, daemonize, daemonize_session, detach_process, sever_from_ssh};
pub use keys::{Attach, AttachKeys, DetachPermit, PSK_LEN, begin_attach, settle_detachability};
pub use ladder::LadderPlan;
pub use registry::{
    META_FILE, PID_REUSE_SLACK_SECS, REGISTRY_SUBDIR, Registry, RegistryGuard, RegistryRoot,
    RegistryRootKind, RootEnv, SOCK_FILE, SessionMeta, check_socket_path_length,
    choose_registry_root, detachable_for_rung, entry_is_stale, linger_enabled, now_unix, pid_alive,
    process_start_unix, read_root_env, resolve_registry_root,
};

// There is deliberately no `transport::Path` here any more. It existed to hold
// one rule — that `max_datagram_size()` returning `None` means "the peer turned
// datagrams off", never "guess a size" — and it held it in a type with no
// production caller, while the real send site in `src/link.rs` did the very
// thing it forbade. The rule now lives in `FrameSink::send`, on the path a
// frame actually takes, with a test on that path. Its other half, a tunnel
// framing for rung 4, described a wire nothing speaks: `oxutrm_net::ice` never
// nominates `Rung::SshTunnel`, so it will be written next to the code that
// carries it, and asks its size question again there.

// For the same reason there is no `RungRunner`, `RungResult` or `nominate`
// here any more. `ladder` keeps the policy — [`LadderPlan`], which is pure
// reasoning over `NatType` and `Rung` and needs no socket to be worth testing —
// and nothing else. The mechanism belongs in the binary, next to the socket it
// cannot be separated from, and HAS NOT BEEN WRITTEN YET; the contract records
// what it must do. `ladder`'s module docs record why the separation was not
// merely awkward but unrepresentable. Two facts are worth repeating where a
// reader looking for the missing API will land:
//
//   * `status_line` is `oxutrm_client::status_line` and only that. The copy
//     that lived here spelt every punched rung "IPv4", which is simply wrong
//     for a v6 path; the client's derives the family from `path.remote`. The
//     host never prints a status line, so it has no business owning one.
//   * The trait's only implementor in the whole tree was its own test double.
//     A seam with one mock behind it is not an abstraction, and this one hid
//     the single thing the connectivity code exists to keep hold of: the
//     socket the NAT mapping belongs to.

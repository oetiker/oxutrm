//! The session registry: one directory per session, holding a `sock` and a
//! `meta.json` (spec §9.2).
//!
//! Two things about it are load-bearing rather than incidental.
//!
//! **It never holds key material.** Certificates and the PSK are generated
//! fresh per attach and live only in memory; `meta.json` records what `--list`
//! needs to show and nothing else. `tests/no_keys_on_disk.rs` enforces that by
//! grepping every byte under the registry for the secrets a session held.
//!
//! **A stale entry is more than a dead pid.** Under `$XDG_RUNTIME_DIR` a
//! reboot cleared the registry, so a live pid was proof enough. The `$HOME`
//! fallback this module falls back to is a real filesystem that survives
//! reboots, and pids are recycled — after a reboot some unrelated process is
//! very likely to hold the recorded number. So an entry is stale when the pid
//! is gone **or** when the process now holding it started well after the
//! session recorded its creation time.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use oxutrm_proto::{Rung, TermSize};
use rustix::process::{Pid, test_kill_process};
use serde::{Deserialize, Serialize};

pub const REGISTRY_SUBDIR: &str = "oxutrm";
pub const META_FILE: &str = "meta.json";
pub const SOCK_FILE: &str = "sock";

/// What `--list` shows, and what `--attach` needs to find a session again.
///
/// Everything here is safe to write down. Nothing here is a secret.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    /// The current attach generation, mirrored from `HostHello.attach_id`
    /// (spec §8.5). Rewritten on every attach, so a host already serving a
    /// session can tell a second `--attach` from the current one.
    pub attach_id: u64,
    pub pid: u32,
    pub created_unix: u64,
    pub shell: String,
    pub size: TermSize,
    /// Can this session outlive the ssh connection that created it?
    ///
    /// **Settled by the nominated rung, never at handshake time** — see
    /// [`detachable_for_rung`]. `HostHello.detachable` is the host's *intent*;
    /// this field is the *outcome*. A rung-4 session tunnels QUIC through the
    /// ssh connection for its whole life, so it cannot close those descriptors,
    /// never daemonizes, and dies with its ssh. `--list` shows the difference,
    /// because "reattach later" is a promise oxutrm must not make falsely.
    pub detachable: bool,
}

impl SessionMeta {
    /// Settle detachability from the rung ICE actually nominated, and return
    /// whether this session may now daemonize.
    ///
    /// The ordering is the point, and it is why this is a method rather than a
    /// field the caller sets: a session that daemonized on *intent* and then
    /// landed on rung 4 would have closed the very ssh descriptors its QUIC
    /// traffic runs inside. Settle first, then decide.
    pub fn set_detachable(&mut self, rung: Rung) -> bool {
        self.detachable = detachable_for_rung(rung);
        self.detachable
    }
}

/// A fresh session identifier: 32 lowercase hex characters.
///
/// The length and the alphabet are what `HostHello.session_id` documents, and
/// they are not decoration. The identifier is a **directory name** under the
/// registry root, chosen by this process and joined onto a path, so anything
/// outside `[0-9a-f]` would put path syntax into a filename — and `..` is
/// spelled entirely in characters a laxer alphabet would allow. A fixed-length
/// hex string cannot express one.
///
/// CSPRNG rather than a counter or a pid: two sessions started in the same
/// second must not collide, and a guessable identifier is a guessable socket
/// path in a directory other users can list.
pub fn new_session_id() -> std::io::Result<String> {
    use rand::TryRngCore as _;
    use std::fmt::Write as _;

    let mut raw = [0u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut raw)
        .map_err(|e| std::io::Error::other(format!("reading the system CSPRNG: {e}")))?;

    let mut out = String::with_capacity(32);
    for byte in raw {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

/// `detachable = rung != Rung::SshTunnel`.
///
/// Every other rung carries QUIC over its own UDP socket, which survives the
/// ssh connection closing. Rung 4 does not.
#[must_use]
pub fn detachable_for_rung(rung: Rung) -> bool {
    !matches!(rung, Rung::SshTunnel)
}

/// Seconds since the Unix epoch. Saturates rather than panicking on a clock
/// set before 1970.
#[must_use]
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True when a process with this pid exists and we could signal it.
///
/// `kill(pid, 0)` performs the permission and existence check without
/// delivering anything. `EPERM` means the process exists but belongs to
/// somebody else, which still counts as alive. Pid 0 is excluded because to
/// `kill(2)` it means "every process in our group", which would always
/// succeed.
#[must_use]
pub fn pid_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    // `Pid::from_raw` rejects 0, which is what we want: to `kill(2)` pid 0 means
    // "every process in our group", so it would always report alive.
    let Some(pid) = Pid::from_raw(raw) else {
        return false;
    };
    match test_kill_process(pid) {
        Ok(()) => true,
        Err(e) => e == rustix::io::Errno::PERM,
    }
}

/// Slack between a session recording its creation time and its daemonized
/// process actually starting.
///
/// Generous on purpose: being wrong in this direction only means a stale entry
/// survives one more `--list`, while being wrong the other way deletes a live
/// session's socket.
pub const PID_REUSE_SLACK_SECS: u64 = 5;

/// Seconds since the epoch at which the process holding `pid` started.
///
/// `None` when there is no such process, or when the system will not say.
/// Losing the answer is not fatal in either direction: [`entry_is_stale`]
/// treats `None` as "keep the entry", because listing a dead session is much
/// less bad than deleting a live one's socket.
///
/// There is no portable way to ask this, so there is one implementation per
/// system and they agree only on the return type.
///
/// # Linux
///
/// `/proc/<pid>/stat` field 22 is the start time in clock ticks since boot, and
/// `/proc/stat`'s `btime` turns that into wall-clock time. The command name in
/// field 2 may itself contain spaces and parentheses — `sh -c 'exec -a "a) b"'`
/// is enough to do it — so parsing starts after the **last** `)`.
#[cfg(target_os = "linux")]
#[must_use]
pub fn process_start_unix(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = text.rsplit_once(')')?.1;
    // Fields resume at 3 (state) after the command name, so field 22 is index 19.
    let ticks: u64 = after_comm.split_whitespace().nth(19)?.parse().ok()?;
    let hz = match rustix::param::clock_ticks_per_second() {
        0 => 100,
        n => n,
    };
    let boot = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(boot + ticks / hz)
}

/// macOS has no `/proc`. `proc_pidinfo(PROC_PIDTBSDINFO)` is the supported
/// question, and it answers with a `proc_bsdinfo` whose `pbi_start_tvsec` is
/// already **seconds since the epoch** — so unlike Linux there is no boot time
/// to add and no clock-tick conversion to get wrong.
///
/// `sysctl(KERN_PROC_PID)` would answer the same question and is what the port
/// was planned around, but `libc` does not define `kinfo_proc` for Apple
/// targets, so taking that route would mean declaring the kernel's struct
/// layout here and owning it forever. This one is typed by `libc`.
///
/// A process belonging to another user can refuse to answer, and that is
/// harmless: see the `None` case in [`entry_is_stale`].
///
/// **Compile-verified against `aarch64-apple-darwin`, never run** — nobody on
/// the project has a Mac yet. Its failure mode if it is wrong is the mild one:
/// a `None` costs the pid-reuse guard and nothing else.
#[cfg(target_os = "macos")]
// The rest of this crate is `deny(unsafe_code)` and stays that way. This is
// one FFI call with a scalar return, kept to the smallest scope that can hold
// it rather than a module-wide allowance.
#[allow(unsafe_code)]
#[must_use]
pub fn process_start_unix(pid: u32) -> Option<u64> {
    let pid = i32::try_from(pid).ok()?;
    let want = std::mem::size_of::<libc::proc_bsdinfo>();
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };

    // SAFETY: `info` is an owned local of exactly the type this flavour
    // reports and of exactly the size passed as `buffersize`, and it is not
    // aliased. `proc_pidinfo` writes at most that many bytes and returns how
    // many it wrote.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&raw mut info).cast::<libc::c_void>(),
            i32::try_from(want).ok()?,
        )
    };

    // A pid that is gone, or one this user may not inspect, reports a short
    // write rather than filling the struct. Anything less than the whole
    // thing means the fields below were never written, so it must not be read
    // as "started in 1970" -- that would make every entry look recycled.
    if written != i32::try_from(want).ok()? {
        return None;
    }
    Some(info.pbi_start_tvsec)
}

/// Everywhere else. The pid-reuse guard is lost — [`entry_is_stale`] keeps the
/// entry — and nothing else changes. This arm exists so that a port to another
/// Unix compiles and runs before anyone writes its `sysctl` call, rather than
/// failing at the link stage with no clue why.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[must_use]
pub fn process_start_unix(_pid: u32) -> Option<u64> {
    None
}

/// Is this registry entry dead wood?
///
/// Stale when the pid is gone, or when the pid now belongs to an unrelated
/// process (spec §9.2).
#[must_use]
pub fn entry_is_stale(meta: &SessionMeta) -> bool {
    if !pid_alive(meta.pid) {
        return true;
    }
    match process_start_unix(meta.pid) {
        // Started well after the entry was written: the pid was recycled.
        Some(start) => start > meta.created_unix.saturating_add(PID_REUSE_SLACK_SECS),
        // The pid exists but the system will not say when it started. Keep
        // it: deleting a live session's socket is much worse than listing a
        // dead one.
        None => false,
    }
}

pub struct Registry;

impl Registry {
    /// The registry directory, wherever it has to live to survive logout.
    /// See [`choose_registry_root`].
    pub fn dir() -> anyhow::Result<PathBuf> {
        Ok(Self::dir_at(&resolve_registry_root()?.base))
    }

    #[must_use]
    pub fn dir_at(base: &Path) -> PathBuf {
        base.join(REGISTRY_SUBDIR)
    }

    pub fn socket_path(id: &str) -> anyhow::Result<PathBuf> {
        Ok(Self::socket_path_in(&Self::dir()?, id))
    }

    #[must_use]
    pub fn socket_path_in(dir: &Path, id: &str) -> PathBuf {
        dir.join(id).join(SOCK_FILE)
    }

    /// Every live session, oldest first. Stale entries are removed from disk as
    /// a side effect (spec §9.2).
    pub fn list() -> anyhow::Result<Vec<SessionMeta>> {
        Self::list_in(&Self::dir()?)
    }

    pub fn list_in(dir: &Path) -> anyhow::Result<Vec<SessionMeta>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e).with_context(|| format!("reading registry {}", dir.display()));
            }
        };

        let mut live = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("reading registry {}", dir.display()))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // No meta yet, or unreadable, or not valid JSON: leave it alone. A
            // session that is still registering owns this directory, not us.
            let Ok(text) = std::fs::read_to_string(path.join(META_FILE)) else {
                continue;
            };
            let Ok(meta) = serde_json::from_str::<SessionMeta>(&text) else {
                continue;
            };
            if entry_is_stale(&meta) {
                // Takes the socket with it, which is the point: a stale socket
                // makes `--attach` hang instead of failing.
                let _ = std::fs::remove_dir_all(&path);
            } else {
                live.push(meta);
            }
        }
        live.sort_by(|a, b| {
            a.created_unix
                .cmp(&b.created_unix)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(live)
    }
}

/// Owns one `<registry>/<id>/` directory for as long as the session lives.
/// Dropping it removes the directory, so a session that exits cleanly leaves
/// nothing behind for `--list` to prune.
pub struct RegistryGuard {
    dir: PathBuf,
}

impl RegistryGuard {
    pub fn register(meta: &SessionMeta) -> anyhow::Result<RegistryGuard> {
        Self::register_in(&Registry::dir()?, meta)
    }

    pub fn register_in(root: &Path, meta: &SessionMeta) -> anyhow::Result<RegistryGuard> {
        create_private_dir(root)?;
        let dir = root.join(&meta.session_id);
        // `create_dir` and not `create_dir_all`: an existing directory means
        // another live session already owns this id, and taking it over would
        // delete that session's socket on drop.
        std::fs::create_dir(&dir)
            .with_context(|| format!("creating session directory {}", dir.display()))?;
        set_private_mode(&dir, 0o700)?;
        let guard = RegistryGuard { dir };
        guard.update(meta)?;
        Ok(guard)
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.dir.join(SOCK_FILE)
    }

    #[must_use]
    pub fn meta_path(&self) -> PathBuf {
        self.dir.join(META_FILE)
    }

    /// Rewrite `meta.json`. Called after `daemonize()`, because forking twice
    /// changes the pid that `--list` prunes on, and after every attach, because
    /// `attach_id` moves.
    pub fn update(&self, meta: &SessionMeta) -> anyhow::Result<()> {
        let text = serde_json::to_vec_pretty(meta).context("encoding meta.json")?;
        write_private_file(&self.meta_path(), &text)
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        // Best effort: there is nothing sensible to do on failure at drop time,
        // and `--list` prunes whatever a crash leaves behind.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn set_private_mode(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {mode:o} {}", path.display()))
}

/// Create a directory owned by this user alone, whatever the umask says.
pub(crate) fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    }
    set_private_mode(path, 0o700)
}

/// Write a file readable by this user alone, whatever the umask says.
///
/// The mode is set both at `open` and again afterwards: `mode()` is masked by
/// the umask on creation, and the explicit `chmod` is what makes the bits hold
/// under a loose one.
pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    f.flush()?;
    set_private_mode(path, 0o600)
}

// ---------------------------------------------------------------------------
// Where the registry lives
// ---------------------------------------------------------------------------

/// Where the registry lives, and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegistryRootKind {
    /// `$XDG_RUNTIME_DIR`, which is known to survive logout here.
    RuntimeDir,
    /// `$HOME/.local/state`, chosen because the runtime directory would not.
    StateDir,
}

#[derive(Clone, Debug)]
pub struct RegistryRoot {
    pub base: PathBuf,
    pub kind: RegistryRootKind,
    /// Printed to stderr once, before daemonizing, where it can still be seen.
    /// `None` when all is well.
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct RootEnv {
    pub xdg_runtime_dir: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// `$OXUTRM_STATE_DIR`: an explicit choice, which is never second-guessed.
    pub override_dir: Option<PathBuf>,
    /// `None` means persistence could not be determined.
    pub linger: Option<bool>,
    /// Whether this system has runtime directories at all.
    ///
    /// `false` on macOS, and the difference is not cosmetic. Every fallback
    /// below also explains itself, and on a Mac that explanation would name
    /// `XDG_RUNTIME_DIR` and tell the user to run `loginctl enable-linger` —
    /// a variable that is never set and a program that is not installed — on
    /// every single session. Where the concept does not exist there is nothing
    /// to prefer, nothing to warn about and nothing to advise: the state
    /// directory is simply where sessions live.
    ///
    /// A field rather than a `cfg` in [`choose_registry_root`], so the whole
    /// decision table stays testable from either platform.
    pub runtime_dirs_exist: bool,
}

/// `$HOME/.local/state`, per the XDG base directory specification.
fn state_base(home: &Path) -> PathBuf {
    home.join(".local").join("state")
}

/// Decide where sessions are recorded.
///
/// `$XDG_RUNTIME_DIR` is preferred, but **only when it is known to survive the
/// user logging out**. On a systemd host `/run/user/<uid>` is destroyed with
/// the last login session: the session process keeps running while its registry
/// directory and its `sock` vanish underneath it, so `--list` shows nothing and
/// reattach is impossible. That is exactly the failure oxutrm exists to
/// prevent, arriving through the back door.
///
/// The runtime directory still wins wherever it is safe, because a home
/// directory may be on NFS, where Unix sockets are unreliable.
pub fn choose_registry_root(env: &RootEnv) -> anyhow::Result<RegistryRoot> {
    if let Some(dir) = &env.override_dir {
        return Ok(RegistryRoot {
            base: dir.clone(),
            kind: RegistryRootKind::StateDir,
            warning: None,
        });
    }

    let fallback = |reason: Option<&str>| -> anyhow::Result<RegistryRoot> {
        let home = env.home.as_ref().ok_or_else(|| {
            anyhow!(
                "neither a usable XDG_RUNTIME_DIR nor a HOME, so there is nowhere \
                 to record sessions. Set OXUTRM_STATE_DIR to a directory that \
                 survives logout."
            )
        })?;
        Ok(RegistryRoot {
            base: state_base(home),
            kind: RegistryRootKind::StateDir,
            warning: reason.map(|reason| {
                format!(
                    "oxutrm: {reason}, so sessions are recorded in {} instead of \
                     XDG_RUNTIME_DIR. Sessions will survive, but on a networked \
                     home directory the session socket may be unreliable. To use \
                     the runtime directory instead, run `loginctl enable-linger \
                     $USER` on this host; to choose the location yourself, set \
                     OXUTRM_STATE_DIR.",
                    state_base(home).join(REGISTRY_SUBDIR).display()
                )
            }),
        })
    };

    // Nothing to prefer and nothing to explain. Note that this ignores an
    // `XDG_RUNTIME_DIR` somebody set by hand: on a system with no runtime
    // directories there is no way to ask whether that one outlives the login,
    // and an unverifiable runtime directory is exactly what the table below
    // refuses everywhere else. `OXUTRM_STATE_DIR` remains the way to say where
    // sessions go, and it was already handled above.
    if !env.runtime_dirs_exist {
        return fallback(None);
    }

    match (&env.xdg_runtime_dir, env.linger) {
        (Some(dir), Some(true)) => Ok(RegistryRoot {
            base: dir.clone(),
            kind: RegistryRootKind::RuntimeDir,
            warning: None,
        }),
        (Some(_), Some(false)) => fallback(Some(
            "lingering is off for this user, so XDG_RUNTIME_DIR is destroyed at \
             logout and a detached session would become unreachable",
        )),
        (Some(_), None) => fallback(Some(
            "whether XDG_RUNTIME_DIR survives logout could not be determined",
        )),
        (None, _) => fallback(Some("XDG_RUNTIME_DIR is not set")),
    }
}

/// Ask systemd whether this user's runtime directory outlives their sessions.
///
/// `None` when the question cannot be answered — no `loginctl`, no systemd, or
/// an unexpected answer. The caller treats that as "do not trust it".
#[must_use]
pub fn linger_enabled(uid: u32) -> Option<bool> {
    let out = std::process::Command::new("loginctl")
        .args(["show-user", &uid.to_string(), "--property=Linger"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    match text.trim().strip_prefix("Linger=")?.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

#[must_use]
pub fn read_root_env() -> RootEnv {
    let uid = rustix::process::getuid().as_raw();
    RootEnv {
        // The one place the platform is asked. macOS has neither the variable
        // nor `loginctl`; every other Unix oxutrm runs on is systemd-shaped
        // enough for the table in `choose_registry_root` to apply.
        runtime_dirs_exist: !cfg!(target_os = "macos"),
        xdg_runtime_dir: std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        home: std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        override_dir: std::env::var_os("OXUTRM_STATE_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        linger: linger_enabled(uid),
    }
}

pub fn resolve_registry_root() -> anyhow::Result<RegistryRoot> {
    choose_registry_root(&read_root_env())
}

/// `sockaddr_un::sun_path` holds 108 bytes on Linux and 104 on macOS, in both
/// cases including the terminating NUL, and a long home directory can overflow
/// it. Checked before binding, because the error the kernel gives otherwise
/// says nothing useful. The limit below is the smaller platform's, with room
/// to spare, so one number is right everywhere.
pub fn check_socket_path_length(path: &Path) -> anyhow::Result<()> {
    const SUN_PATH_MAX: usize = 100;
    let len = path.as_os_str().as_encoded_bytes().len();
    if len > SUN_PATH_MAX {
        return Err(anyhow!(
            "the session socket path is {len} bytes, and a Unix socket path cannot \
             exceed {SUN_PATH_MAX}: {}. Set OXUTRM_STATE_DIR to something shorter.",
            path.display()
        ));
    }
    Ok(())
}

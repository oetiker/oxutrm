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
    /// Which boot this session belongs to — see [`boot_token`].
    ///
    /// `Option` for two reasons that both matter: a platform may not be able
    /// to answer, and an entry written before this field existed must keep
    /// parsing. `serde(default)` is what makes the second true; without it an
    /// upgrade would strand every running session behind a parse error.
    #[serde(default)]
    pub boot: Option<String>,
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
/// Run locally on macOS since 2026-09-04 — not just compile-checked. This
/// task's own tests exercise the same file there too. That does not mean
/// every branch is covered; it means the happy path runs on real hardware.
/// Its failure mode if it is wrong is the mild one: a `None` costs the
/// pid-reuse guard and nothing else.
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

/// A token identifying the boot this session belongs to.
///
/// The only contract is that it differs across boots and, where the platform
/// allows, across machines. `None` when the platform cannot answer, which
/// costs the cross-boot staleness check in [`entry_is_stale`] and nothing
/// else.
///
/// There is no portable way to ask this, so there is one implementation per
/// system and they agree only on the return type.
///
/// # Linux
///
/// `/proc/sys/kernel/random/boot_id` is a UUID the kernel regenerates at every
/// boot, and it is random rather than derived, so it also differs between two
/// machines that booted at the same instant. That is what lets a registry on
/// shared storage tell another host's entries from its own.
#[cfg(target_os = "linux")]
#[must_use]
pub fn boot_token() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_owned())
}

/// # macOS
///
/// `kern.boottime` is the wall-clock time the kernel started, to the
/// microsecond, and `libc` types it as a `timeval` — so unlike the
/// `proc_pidinfo` call above there is no kernel struct to declare here.
///
/// **It carries no machine identity.** Two Macs that booted in the same
/// microsecond and shared a registry directory would collide. Accepted
/// deliberately: shared home directories are rare on macOS, and Linux — where
/// they are not — has a random per-boot UUID that is exact. Recorded as a
/// known limit in the design note rather than engineered around.
///
/// Asking for pid 1's start time would have reused the FFI exception above
/// instead of adding one, and does not work: `proc_pidinfo` refuses for
/// root-owned `launchd`, verified by experiment on 2026-09-06.
#[cfg(target_os = "macos")]
// The rest of this crate is `deny(unsafe_code)` and stays that way. This is
// one FFI call with a scalar return, kept to the smallest scope that can hold
// it rather than a module-wide allowance.
#[allow(unsafe_code)]
#[must_use]
pub fn boot_token() -> Option<String> {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let want = std::mem::size_of::<libc::timeval>();
    let mut len = want;

    // SAFETY: `tv` is an owned local of exactly the type this sysctl reports
    // and of exactly the size passed in `len`, and it is not aliased. The
    // name is a NUL-terminated C string literal. Passing a null new-value
    // pointer with a zero length is the documented way to read without
    // writing.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            std::ptr::from_mut(&mut tv).cast::<libc::c_void>(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };

    if rc != 0 || len != want {
        return None;
    }
    // Zero-padded microseconds so the string orders the same way the instant
    // does, and so two tokens never differ only by formatting.
    Some(format!("{}.{:06}", tv.tv_sec, tv.tv_usec))
}

/// Is this registry entry dead wood?
///
/// Stale when the pid is gone, or when the pid now belongs to an unrelated
/// process (spec §9.2).
#[must_use]
pub fn entry_is_stale(meta: &SessionMeta) -> bool {
    // A different boot means a different process table: this pid refers to
    // another boot, or another machine sharing the registry directory. Decide
    // here and do not look at the pid at all -- on a shared registry it names
    // an unrelated live process on this host, which would read as "alive".
    //
    // Only when BOTH sides have a token: a `None` on either side means "cannot
    // tell", and the rules below are the answer for that case.
    if let (Some(entry), Some(current)) = (meta.boot.as_deref(), boot_token().as_deref())
        && entry != current
    {
        return true;
    }

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
// Proving a candidate registry root is safe to use
// ---------------------------------------------------------------------------

/// What an existing path at a candidate registry root turns out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum DirVerdict {
    /// Nothing there. Create it.
    Absent,
    /// A directory, ours, mode `0700`. Reuse it.
    Ours,
    /// A directory, ours, wrong mode. `chmod` and reuse: we own it, so this is
    /// recovery from a crash or an older version.
    OursWrongMode,
    /// Anything else. Never used, never repaired. The string says why, for a
    /// message a user has to act on.
    NotOurs(String),
}

/// Judge a path from metadata obtained **without following symlinks**.
///
/// Pure, and takes `our_uid` rather than calling `getuid` itself, so the
/// foreign-owner case is testable without privileges — a directory owned by
/// another user cannot be created by a test.
#[must_use]
pub fn dir_verdict(md: &std::fs::Metadata, our_uid: u32) -> DirVerdict {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    if md.file_type().is_symlink() {
        return DirVerdict::NotOurs("it is a symbolic link".to_owned());
    }
    if !md.is_dir() {
        return DirVerdict::NotOurs("it is not a directory".to_owned());
    }
    if md.uid() != our_uid {
        return DirVerdict::NotOurs(format!("it is owned by uid {}, not {our_uid}", md.uid()));
    }
    // Exactly 0700, not merely "no group or other bits". A directory at 0600
    // has no bits we object to and still cannot be traversed, so testing only
    // `& 0o077` would accept a root nothing could be created inside.
    if md.permissions().mode() & 0o777 != 0o700 {
        return DirVerdict::OursWrongMode;
    }
    DirVerdict::Ours
}

/// Why a candidate registry root could not be used.
#[derive(Debug)]
pub enum PrepareError {
    /// This location does not exist on this system, or is not a safe place to
    /// put anything. Nothing of ours is there. **Try the next candidate.**
    Unavailable(String),
    /// The path exists and is not safely ours. **Do not try the next
    /// candidate** — falling through is a downgrade a squatter can force.
    Occupied(String),
    /// An ordinary I/O failure: a full or read-only filesystem. Try the next.
    Io(std::io::Error),
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why) | Self::Occupied(why) => f.write_str(why),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Create a registry root, or prove an existing one is safe to reuse.
///
/// The parent of a candidate is often group- or world-writable (`/dev/shm`
/// and `/var/tmp` are both `1777`), so this function is the whole of the
/// protection. Three rules carry it:
///
/// **The parent's sticky bit is load-bearing.** `1777` lets only an entry's
/// owner delete or rename it, which is what stops another user replacing our
/// directory once it exists. A group- or world-writable parent *without*
/// `S_ISVTX` offers no such protection — a fellow group member (0770) or any
/// user (0777) could rename our directory away and put something else in its
/// place — so that location is refused outright.
///
/// **The parent must be owned by us or by root.** A parent owned by anyone
/// else was created by someone we have no reason to trust, sticky bit or not
/// — the sticky bit only says *other* users cannot rearrange its entries, it
/// says nothing about the owner, who always can.
///
/// **An occupied path is terminal, not a reason to look elsewhere.** See
/// `resolve_registry_root`. This includes losing a race to create the
/// directory: a squatter who wins `mkdir` under a sticky, world-writable
/// parent can create and remove their own entry for free, so this must never
/// silently fall through to the next candidate — [`create_root`] re-judges
/// whatever is there the moment `mkdir` reports it already exists.
pub fn prepare_root(base: &Path) -> Result<(), PrepareError> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let parent = base
        .parent()
        .ok_or_else(|| PrepareError::Unavailable(format!("{} has no parent", base.display())))?;

    let parent_md = std::fs::metadata(parent)
        .map_err(|e| PrepareError::Unavailable(format!("{}: {e}", parent.display())))?;
    if !parent_md.is_dir() {
        return Err(PrepareError::Unavailable(format!(
            "{} is not a directory",
            parent.display()
        )));
    }
    let parent_mode = parent_md.permissions().mode();
    // Group- or world-writable without the sticky bit: another member of the
    // owning group, or (world-writable) any user, could replace our
    // directory between sessions, so nothing we put here could be trusted
    // later.
    if parent_mode & 0o022 != 0 && parent_mode & 0o1000 == 0 {
        return Err(PrepareError::Unavailable(format!(
            "{} is group- or world-writable without the sticky bit, so a directory \
             created there could be replaced by another user",
            parent.display()
        )));
    }
    let our_uid = rustix::process::getuid().as_raw();
    // The sticky bit only constrains who can rearrange the parent's other
    // entries -- its own owner is exempt from it. A parent owned by neither
    // us nor root was created by whoever that uid is, and trusting it would
    // mean trusting them.
    if parent_md.uid() != our_uid && parent_md.uid() != 0 {
        return Err(PrepareError::Unavailable(format!(
            "{} is owned by uid {}, which is neither us nor root, so it is not a \
             safe place to create a registry root",
            parent.display(),
            parent_md.uid()
        )));
    }

    match std::fs::symlink_metadata(base) {
        Ok(md) => apply_verdict(base, dir_verdict(&md, our_uid), our_uid),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_root(base, our_uid),
        Err(e) => Err(PrepareError::Io(e)),
    }
}

/// Act on a verdict already reached for an existing (or freshly re-checked)
/// path. Split out of [`prepare_root`] so [`create_root`] can re-enter it
/// after losing the `mkdir` race, without re-running the parent checks.
fn apply_verdict(base: &Path, verdict: DirVerdict, our_uid: u32) -> Result<(), PrepareError> {
    match verdict {
        DirVerdict::Absent => create_root(base, our_uid),
        DirVerdict::Ours => Ok(()),
        DirVerdict::OursWrongMode => set_private_mode(base, 0o700)
            .map_err(|e| PrepareError::Io(std::io::Error::other(e.to_string()))),
        DirVerdict::NotOurs(why) => Err(PrepareError::Occupied(format!(
            "{} cannot be used because {why}. Remove or rename it, or set \
             OXUTRM_STATE_DIR to a directory you control.",
            base.display()
        ))),
    }
}

/// Create `base` fresh, private from the instant `mkdir` returns.
///
/// The mode is passed to `mkdir` itself via `DirBuilder`, not applied
/// afterwards with a separate `chmod`: plain `std::fs::create_dir` creates at
/// `0777 & !umask`, and under a loose umask (`002`, say) that leaves the
/// directory group- or world-writable for the whole window between creation
/// and a follow-up `chmod` — long enough for another process to plant an
/// entry inside that survives the tightening. `set_private_mode` afterwards
/// remains, as a belt for whatever mode the platform's `mkdir` actually
/// honours under its own umask handling.
fn create_root(base: &Path, our_uid: u32) -> Result<(), PrepareError> {
    use std::os::unix::fs::DirBuilderExt;

    match std::fs::DirBuilder::new().mode(0o700).create(base) {
        Ok(()) => set_private_mode(base, 0o700)
            .map_err(|e| PrepareError::Io(std::io::Error::other(e.to_string()))),
        // Something now occupies the path that `symlink_metadata` reported
        // absent a moment ago. A squatter racing us under a sticky,
        // world-writable parent can always win this: creating and removing
        // their own entry costs them nothing, and they can retry forever.
        // Falling through to `Io` here would let that race force the exact
        // downgrade this module exists to prevent, so whatever is there now
        // is judged on its own terms rather than treated as a transient
        // failure.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::symlink_metadata(base) {
                Ok(md) => apply_verdict(base, dir_verdict(&md, our_uid), our_uid),
                // Disappeared again between the failed `mkdir` and this
                // re-check: something is actively creating and removing
                // entries at this path. That is not a location to trust with
                // a retry loop of our own -- stop here, the same as any other
                // occupied path.
                Err(_) => Err(PrepareError::Occupied(format!(
                    "{} could not be created because something else is creating \
                     and removing entries there at the same time",
                    base.display()
                ))),
            }
        }
        // Anything else -- ENOSPC, EROFS -- says nothing about who owns the
        // path, so it is an ordinary I/O failure rather than a trust decision.
        Err(e) => Err(PrepareError::Io(e)),
    }
}

// ---------------------------------------------------------------------------
// Where the registry lives
// ---------------------------------------------------------------------------

/// Where the registry lives, and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegistryRootKind {
    /// `$OXUTRM_STATE_DIR`: the user said so. Never second-guessed.
    Explicit,
    /// `$XDG_RUNTIME_DIR`, which is known to survive logout here.
    RuntimeDir,
    /// `/dev/shm/oxutrm-<uid>`: tmpfs, cleared at boot, carries no ageing
    /// rule. The normal case on Linux without lingering.
    SharedMemory,
    /// `/var/tmp/oxutrm-<uid>`: local and logout-surviving everywhere, but it
    /// outlives a reboot, so staleness detection is load-bearing here.
    VarTmp,
    /// `$HOME/.local/state`: last resort. May be a network filesystem, where
    /// a Unix socket is unreliable, so it warns.
    HomeState,
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
    /// Whether this system has `/dev/shm` at all.
    ///
    /// `false` on macOS. A field rather than a `cfg` in
    /// [`registry_root_candidates`], so the whole decision table stays
    /// testable from either platform — the same reasoning as
    /// `runtime_dirs_exist` above.
    pub shm_exists: bool,
}

/// `$HOME/.local/state`, per the XDG base directory specification.
fn state_base(home: &Path) -> PathBuf {
    home.join(".local").join("state")
}

/// The ordered places this environment could keep its registry, best first.
///
/// Pure: it decides *preference*, not *safety*. Whether a candidate is
/// actually usable is a filesystem question and belongs to [`prepare_root`].
/// Keeping them apart is what lets this table be exhaustively tested from
/// either platform.
#[must_use]
pub fn registry_root_candidates(env: &RootEnv) -> Vec<RegistryRoot> {
    // An explicit choice is the whole list: falling through would silently
    // put sessions somewhere the user did not ask for.
    if let Some(dir) = &env.override_dir {
        return vec![RegistryRoot {
            base: dir.clone(),
            kind: RegistryRootKind::Explicit,
            warning: None,
        }];
    }

    let uid = rustix::process::getuid().as_raw();
    let mut out = Vec::new();

    // Ahead of /dev/shm on purpose: it is tmpfs and boot-cleared like shm, and
    // additionally 0700 by construction rather than 1777, so there is no
    // squatting race to lose in the first place.
    if env.runtime_dirs_exist
        && let (Some(dir), Some(true)) = (&env.xdg_runtime_dir, env.linger)
    {
        out.push(RegistryRoot {
            base: dir.clone(),
            kind: RegistryRootKind::RuntimeDir,
            warning: None,
        });
    }

    if env.shm_exists {
        out.push(RegistryRoot {
            base: PathBuf::from(format!("/dev/shm/oxutrm-{uid}")),
            kind: RegistryRootKind::SharedMemory,
            warning: None,
        });
    }

    out.push(RegistryRoot {
        base: PathBuf::from(format!("/var/tmp/oxutrm-{uid}")),
        kind: RegistryRootKind::VarTmp,
        warning: None,
    });

    if let Some(home) = &env.home {
        out.push(RegistryRoot {
            base: state_base(home),
            kind: RegistryRootKind::HomeState,
            warning: Some(format!(
                "oxutrm: no local directory was usable, so sessions are recorded in {} \
                 instead. Sessions will survive, but on a networked home directory the \
                 session socket may be unreliable. Set OXUTRM_STATE_DIR to choose the \
                 location yourself.",
                state_base(home).join(REGISTRY_SUBDIR).display()
            )),
        });
    }

    out
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
            kind: RegistryRootKind::HomeState,
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
            kind: RegistryRootKind::HomeState,
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
        shm_exists: cfg!(target_os = "linux") && Path::new("/dev/shm").is_dir(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boot_token_is_available_on_this_platform() {
        // Both supported platforms can answer. A `None` here means the reader
        // below is broken, not that the platform is exotic -- oxutrm-host is
        // cfg'd for linux and macos only.
        let token = boot_token().expect("this platform must supply a boot token");
        assert!(
            !token.is_empty(),
            "an empty token would compare equal to itself forever"
        );
        assert!(
            !token.contains(char::is_whitespace),
            "token must be trimmed: {token:?}"
        );
    }

    #[test]
    fn the_boot_token_is_stable_within_one_boot() {
        // The whole design rests on this: two reads during one boot must agree,
        // or every entry would look foreign to the next `--list`.
        assert_eq!(boot_token(), boot_token());
    }

    fn meta_for_staleness(pid: u32, boot: Option<&str>) -> SessionMeta {
        SessionMeta {
            session_id: "0123456789abcdef0123456789abcdef".to_owned(),
            attach_id: 1,
            pid,
            created_unix: now_unix(),
            shell: "/bin/sh".to_owned(),
            size: TermSize { cols: 80, rows: 24 },
            detachable: true,
            boot: boot.map(str::to_owned),
        }
    }

    #[test]
    fn an_entry_from_another_boot_is_stale_even_though_its_pid_is_alive() {
        // Our own pid, so every pre-existing liveness check says "alive". Only
        // the boot comparison can make this stale -- delete that check and this
        // test fails rather than passing for the wrong reason.
        let meta = meta_for_staleness(std::process::id(), Some("not-this-boot"));
        assert!(entry_is_stale(&meta));
    }

    #[test]
    fn an_entry_from_this_boot_still_follows_the_pid_rules() {
        let live = meta_for_staleness(std::process::id(), boot_token().as_deref());
        assert!(
            !entry_is_stale(&live),
            "our own live pid, this boot: not stale"
        );
    }

    #[test]
    fn an_entry_with_no_boot_token_follows_the_old_rules_unchanged() {
        // Written by a version before this field existed. It must not become
        // stale merely for lacking a token, or upgrading would strand sessions.
        let live = meta_for_staleness(std::process::id(), None);
        assert!(!entry_is_stale(&live));
    }

    #[test]
    fn meta_json_written_before_this_field_existed_still_parses() {
        let old = r#"{"session_id":"0123456789abcdef0123456789abcdef","attach_id":1,
            "pid":1,"created_unix":1,"shell":"/bin/sh",
            "size":{"cols":80,"rows":24},"detachable":true}"#;
        let meta: SessionMeta = serde_json::from_str(old).expect("old meta.json must still parse");
        assert_eq!(meta.boot, None);
    }

    #[test]
    fn a_directory_we_own_at_0700_is_ours() {
        let dir = tempfile::Builder::new()
            .prefix("oxu-")
            .tempdir_in("/tmp")
            .unwrap();
        set_private_mode(dir.path(), 0o700).unwrap();
        let md = std::fs::symlink_metadata(dir.path()).unwrap();
        let uid = rustix::process::getuid().as_raw();
        assert!(matches!(dir_verdict(&md, uid), DirVerdict::Ours));
    }

    #[test]
    fn a_directory_we_own_with_extra_bits_is_recoverable_not_refused() {
        // Ours, so a wrong mode is a crash or an older version, not a trust
        // decision about somebody else's directory. Recovering beats refusing.
        let dir = tempfile::Builder::new()
            .prefix("oxu-")
            .tempdir_in("/tmp")
            .unwrap();
        set_private_mode(dir.path(), 0o755).unwrap();
        let md = std::fs::symlink_metadata(dir.path()).unwrap();
        let uid = rustix::process::getuid().as_raw();
        assert!(matches!(dir_verdict(&md, uid), DirVerdict::OursWrongMode));
    }

    #[test]
    fn a_directory_owned_by_someone_else_is_not_ours() {
        let dir = tempfile::Builder::new()
            .prefix("oxu-")
            .tempdir_in("/tmp")
            .unwrap();
        set_private_mode(dir.path(), 0o700).unwrap();
        let md = std::fs::symlink_metadata(dir.path()).unwrap();
        let not_us = rustix::process::getuid().as_raw().wrapping_add(1);
        assert!(matches!(dir_verdict(&md, not_us), DirVerdict::NotOurs(_)));
    }

    #[test]
    fn a_plain_file_is_not_ours_however_it_is_owned() {
        let dir = tempfile::Builder::new()
            .prefix("oxu-")
            .tempdir_in("/tmp")
            .unwrap();
        let file = dir.path().join("squat");
        std::fs::write(&file, b"").unwrap();
        let md = std::fs::symlink_metadata(&file).unwrap();
        let uid = rustix::process::getuid().as_raw();
        assert!(matches!(dir_verdict(&md, uid), DirVerdict::NotOurs(_)));
    }

    #[test]
    fn a_symlink_is_not_ours_even_when_it_points_at_a_directory_we_own() {
        // The case that catches an lstat-versus-stat mistake. Following the link
        // would report a directory we own and pass -- while handing control of the
        // path to whoever can rewrite the link.
        let dir = tempfile::Builder::new()
            .prefix("oxu-")
            .tempdir_in("/tmp")
            .unwrap();
        let target = dir.path().join("real");
        std::fs::create_dir(&target).unwrap();
        set_private_mode(&target, 0o700).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let md = std::fs::symlink_metadata(&link).unwrap();
        let uid = rustix::process::getuid().as_raw();
        assert!(matches!(dir_verdict(&md, uid), DirVerdict::NotOurs(_)));
    }

    fn env_linux() -> RootEnv {
        RootEnv {
            xdg_runtime_dir: Some(PathBuf::from("/run/user/1000")),
            home: Some(PathBuf::from("/home/u")),
            override_dir: None,
            linger: Some(false),
            runtime_dirs_exist: true,
            shm_exists: true,
        }
    }

    fn kinds(env: &RootEnv) -> Vec<RegistryRootKind> {
        registry_root_candidates(env)
            .iter()
            .map(|r| r.kind)
            .collect()
    }

    #[test]
    fn an_explicit_state_dir_is_the_only_candidate() {
        // Never second-guessed, and nothing to fall through to: if the user's own
        // choice is unusable they must be told, not quietly overridden.
        let mut env = env_linux();
        env.override_dir = Some(PathBuf::from("/somewhere/chosen"));
        let got = registry_root_candidates(&env);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, RegistryRootKind::Explicit);
        assert_eq!(got[0].base, PathBuf::from("/somewhere/chosen"));
    }

    #[test]
    fn without_lingering_shared_memory_comes_first() {
        // The whole point of the change: no `loginctl enable-linger` required.
        assert_eq!(
            kinds(&env_linux()),
            vec![
                RegistryRootKind::SharedMemory,
                RegistryRootKind::VarTmp,
                RegistryRootKind::HomeState
            ]
        );
    }

    #[test]
    fn with_lingering_the_runtime_dir_still_wins() {
        // It is tmpfs, boot-cleared AND 0700 by construction rather than 1777.
        let mut env = env_linux();
        env.linger = Some(true);
        assert_eq!(kinds(&env)[0], RegistryRootKind::RuntimeDir);
    }

    #[test]
    fn an_undeterminable_linger_does_not_win_the_runtime_dir() {
        // `None` means "could not tell", and an unverifiable runtime directory is
        // exactly what the old code refused everywhere else.
        let mut env = env_linux();
        env.linger = None;
        assert_eq!(kinds(&env)[0], RegistryRootKind::SharedMemory);
    }

    #[test]
    fn without_shared_memory_the_portable_fallback_leads() {
        // The macOS shape: no runtime directories, no /dev/shm.
        let mut env = env_linux();
        env.shm_exists = false;
        env.runtime_dirs_exist = false;
        env.xdg_runtime_dir = None;
        env.linger = None;
        assert_eq!(
            kinds(&env),
            vec![RegistryRootKind::VarTmp, RegistryRootKind::HomeState]
        );
    }

    #[test]
    fn home_state_is_last_and_carries_the_warning_no_other_candidate_does() {
        let got = registry_root_candidates(&env_linux());
        let home = got.last().unwrap();
        assert_eq!(home.kind, RegistryRootKind::HomeState);
        let warning = home
            .warning
            .as_deref()
            .expect("the last resort must explain itself");
        assert!(
            warning.contains("networked"),
            "it must name the actual risk: {warning}"
        );
        assert!(
            got[..got.len() - 1].iter().all(|r| r.warning.is_none()),
            "a local candidate has nothing to warn about"
        );
    }

    #[test]
    fn with_no_home_the_list_simply_ends_early() {
        let mut env = env_linux();
        env.home = None;
        assert_eq!(
            kinds(&env),
            vec![RegistryRootKind::SharedMemory, RegistryRootKind::VarTmp]
        );
    }

    #[test]
    fn the_per_uid_directories_are_named_for_this_user() {
        let got = registry_root_candidates(&env_linux());
        let uid = rustix::process::getuid().as_raw();
        assert_eq!(got[0].base, PathBuf::from(format!("/dev/shm/oxutrm-{uid}")));
        assert_eq!(got[1].base, PathBuf::from(format!("/var/tmp/oxutrm-{uid}")));
    }

    #[test]
    fn create_root_that_loses_the_mkdir_race_is_occupied_not_io() {
        // `create_root` only ever sees `AlreadyExists` when something is
        // already at `base` the instant `mkdir` runs -- which is exactly what
        // a squatter racing a real `prepare_root` call produces. Planting the
        // squat ourselves first reproduces that outcome deterministically,
        // without needing an actual concurrent process.
        let dir = tempfile::Builder::new()
            .prefix("oxu-")
            .tempdir_in("/tmp")
            .unwrap();
        let base = dir.path().join("oxutrm-1234");
        std::fs::write(&base, b"squat").unwrap();
        let uid = rustix::process::getuid().as_raw();

        match create_root(&base, uid) {
            Err(PrepareError::Occupied(_)) => {}
            other => panic!(
                "losing the mkdir race to a squatter must be Occupied, not {other:?} -- \
                 Io would let the caller fall through to the next candidate"
            ),
        }
    }
}

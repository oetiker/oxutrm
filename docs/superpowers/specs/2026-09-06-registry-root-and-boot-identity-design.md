# Registry root and boot identity — design

**Status:** design, approved in conversation 2026-09-06. Not yet planned or
implemented.

## The problem

`oxutrm host --attach` needs a Unix socket. Today that socket lives wherever
the registry lives, and `choose_registry_root` picks between exactly two
places:

1. `$XDG_RUNTIME_DIR`, but **only** when `loginctl` says lingering is on, and
2. `$HOME/.local/state/oxutrm` otherwise.

On a normal systemd host without lingering, that means the socket lands in the
home directory. Observed on `thinlinc` on 2026-09-04, where `$HOME` is
**CephFS**:

> oxutrm: lingering is off for this user … sessions are recorded in
> `/home/oetiker/.local/state/oxutrm` … **on a networked home directory the
> session socket may be unreliable.**

Two distinct defects hide behind that sentence.

**A Unix socket on a network filesystem.** It worked across three attaches in
the Tier B1 hand test, but AF_UNIX on a network filesystem is unsupported
territory that happens to work; "worked once on one machine" is not a
guarantee we should be shipping.

**A registry shared between machines.** `$HOME` is shared across the thinlinc
nodes, and `SessionMeta` carries **no machine or boot identity** — only a bare
`pid`. So `--list` on one node can display sessions belonging to another, and
the liveness check (`pid_alive`, plus `process_start_unix` for pid reuse) is
evaluating a pid number against the *wrong machine's* process table. This is
latent today and independent of Tier B1.

The obvious workaround — `loginctl enable-linger $USER` — is not acceptable:
it requires per-user admin action on every host, and **reattach should work
without lingering**.

`/run/user/<uid>` cannot be made to work without lingering. logind removes it
at last logout unconditionally; that is what lingering *is*. So the socket has
to move somewhere else.

## What the registry actually needs

Stating the requirements plainly removes most of the candidates:

| Requirement | Why |
|---|---|
| **Local filesystem** | AF_UNIX must be reliable |
| **Survives logout** | `KillUserProcesses=no` is the common default, so a detached host outlives the login; its socket must too |
| **Per host** | a `pid` means nothing on another machine |
| **Not age-swept** | a cleaner removing a socket under a live shell is the exact failure this code exists to prevent |
| **No admin action** | must work out of the box |

Durability across a **reboot is not required**. Every registry entry describes
a running process, and no session survives a reboot. Storage that outlives a
reboot is not a feature here; it is a source of stale entries.

## Decision: `/dev/shm` on Linux, `/var/tmp` elsewhere

Measured on `thinlinc` (2026-09-06):

| Candidate | Age-swept | Cleared at boot | Local | Survives logout |
|---|---|---|---|---|
| `/tmp` | **yes — `D /tmp 1777 root root 30d`, cleaner timer active** | yes | yes | yes |
| `/var/tmp` | no | **no** | yes | yes |
| `/dev/shm` | no | yes | yes | yes |

`/dev/shm` is `tmpfs rw,nosuid,nodev`, 74 GB, mode `1777`. Binding and
connecting an AF_UNIX socket there was verified by experiment, not assumed
(`sun_path` length 31, far inside the 108-byte limit). In the host's entire
`tmpfiles.d` ruleset the only rule touching these directories is the 30-day
sweep of `/tmp`.

`/tmp` is **disqualified**, not merely ranked lower. A socket's `mtime` is set
at bind and does not advance when clients connect, so a session idle past 30
days would have its socket swept out from under a live shell.

Being cleared at boot is the property that matters most: it restores the strong
staleness position this module's own documentation describes for
`$XDG_RUNTIME_DIR` — *"a reboot cleared the registry, so a live pid was proof
enough"* — instead of leaning on the `/proc` start-time heuristic as the
load-bearing check.

`/dev/shm` is Linux-only, so non-Linux hosts fall back to `/var/tmp`, where
reboot survival makes the existing staleness machinery load-bearing again.

### The new resolution order

1. `$OXUTRM_STATE_DIR` — an explicit choice, never second-guessed. Unchanged.
2. `$XDG_RUNTIME_DIR` when lingering is on. Still ahead of `/dev/shm`: it is
   tmpfs, boot-cleared, **and** `0700` by construction rather than `1777`.
3. `/dev/shm/oxutrm-<uid>` — Linux, when `/dev/shm` exists and is usable.
4. `/var/tmp/oxutrm-<uid>` — portable fallback.
5. `$HOME/.local/state/oxutrm` — **last resort only**, when nothing above is
   usable, carrying today's warning. Kept so the function never has to fail
   with "nowhere to record sessions".

`RegistryRootKind` gains variants to match, and `StateDir` is renamed: it now
describes a genuine last resort rather than the normal case.

```rust
pub enum RegistryRootKind {
    Explicit,      // $OXUTRM_STATE_DIR
    RuntimeDir,    // $XDG_RUNTIME_DIR, lingering on
    SharedMemory,  // /dev/shm/oxutrm-<uid>
    VarTmp,        // /var/tmp/oxutrm-<uid>
    HomeState,     // $HOME/.local/state — last resort, warns
}
```

`RootEnv` gains one field, following the existing `runtime_dirs_exist`
precedent so the decision table stays testable from either platform:

```rust
/// Whether this system has `/dev/shm` at all. `false` on macOS.
/// A field rather than a `cfg`, so the table is testable from either platform.
pub shm_exists: bool,
```

### Policy and verification are separated

Choosing where to *prefer* is policy and belongs in a pure function. Deciding
whether a directory is *safe to use* is a filesystem question and cannot be. An
earlier draft of this spec conflated them, and then claimed a test row —
"present but refused by the ownership check" — that a pure decision table
cannot express. The split:

```rust
/// Pure. The ordered candidates for this environment, best first.
pub fn registry_root_candidates(env: &RootEnv) -> Vec<RegistryRoot>;

/// Impure. Create the directory, or verify an existing one is safe to reuse.
pub fn prepare_root(root: &RegistryRoot) -> Result<(), PrepareError>;

pub enum PrepareError {
    /// This location does not exist on this system at all — no `/dev/shm`,
    /// no `$XDG_RUNTIME_DIR`. Try the next candidate.
    Unavailable(String),
    /// The path exists and is NOT safely ours. Do not try the next candidate;
    /// report this and stop.
    Occupied(String),
    /// Ordinary I/O failure creating it (a full or read-only filesystem).
    /// Try the next candidate.
    Io(std::io::Error),
}
```

`resolve_registry_root` walks the candidate list, taking the first that
succeeds. `Unavailable` and `Io` move to the next candidate. **`Occupied` does
not** — see below.

An explicit `$OXUTRM_STATE_DIR` never falls through on any error: it is never
second-guessed, so if `prepare_root` refuses it the walk fails loudly rather
than silently choosing somewhere the user did not ask for.

### A squatted directory must fail, not fall through

This is the one place where the obvious behaviour is the wrong one.

`/dev/shm` and `/var/tmp` are both `1777`. Any local user can create
`/dev/shm/oxutrm-1003` before we do — and so can the owning user, by accident,
with a stray file or a leftover from a crash. If "it exists and is not ours"
merely moved to the next candidate, an attacker who squats **both** local
candidates forces resolution down to `$HOME/.local/state`: back onto the
networked home directory this design exists to get off, silently, with the
socket-reliability warning being the only hint. That is a downgrade attack that
costs the attacker two `mkdir`s, and the same mechanism degrades a session by
accident when the squatter is the user's own stray file.

So `Occupied` is terminal. oxutrm reports the offending path, says what is
wrong with it (not a directory / owned by uid N / has group or other bits), and
tells the user to inspect and remove it, or to set `$OXUTRM_STATE_DIR`. It does
not chmod it, does not remove it, and does not quietly use somewhere else.

The precedent is tmux, which refuses to start when `/tmp/tmux-<uid>` is not
owned by the user with the expected mode rather than falling back. The deeper
precedent is `/run/user/<uid>` itself, whose whole point is a per-user
directory in a parent users cannot write to.

**The sticky bit bounds the exposure.** `1777` means only an entry's owner may
delete or rename it, so once we have created `oxutrm-<uid>` nobody else can
replace it. The race exists only at first creation after each boot (for
`/dev/shm`) — and losing it is now loud rather than silent.

**The parent's sticky bit is therefore load-bearing and must be checked.** On a
`/dev/shm` or `/var/tmp` mounted `0777` *without* `S_ISVTX`, another user could
delete our directory and substitute their own between sessions. If the parent
is world-writable and not sticky, the candidate is `Unavailable` — that
location is not safe to use at all, and moving on is correct because nothing
about it is ours yet.

### The directory must be proven ours

`prepare_root` verifies, without following symlinks (`O_NOFOLLOW`, or `lstat`
before use), that an existing path is:

- a real directory, not a file, not a symlink → otherwise `Occupied`
- owned by our uid → otherwise `Occupied`
- mode `0700` exactly. If it is ours but the mode is wrong, `chmod` it back to
  `0700` and proceed: we own it, so this is recovery from a crash or an older
  version, not a trust decision about somebody else's directory.

`set_private_mode(…, 0o700)` already exists and is the right tool for creation.
The type-and-ownership check on a **pre-existing** directory is the new logic,
and it is the only part of this design where a mistake is a security bug rather
than a wrong path.

## Boot identity

Moving to boot-cleared storage removes the cross-boot problem in the normal
case, but not on the `/var/tmp` fallback and not for anyone pointing
`$OXUTRM_STATE_DIR` at shared storage. One field settles both the reboot case
and the wrong-machine case:

```rust
/// Identifies the boot this session belongs to.
///
/// An opaque token whose only contract is that it differs across boots and,
/// where the platform allows, across machines. `None` on a platform that
/// cannot supply one, and on entries written before this field existed.
pub boot: Option<String>,
```

| Platform | Token | Differs per boot | Differs per machine |
|---|---|---|---|
| Linux | `/proc/sys/kernel/random/boot_id` | yes | yes — a random UUID per boot |
| macOS | `sysctl kern.boottime`, as `sec.usec` | yes | only if the boots differ |

macOS gets no machine identifier. `IOPlatformUUID` would make it exact, but it
is a stable machine identifier and the registry's standing rule is that
`meta.json` "records what `--list` needs to show and nothing else". A collision
requires two Macs booted in the same microsecond **and** a shared registry
directory; shared home directories are rare on macOS, which is the platform
where `boot_id` is already exact. Accepted deliberately, recorded here as a
known limit.

### How staleness uses it

Boot identity **does not replace** the existing checks. It settles staleness
*across* boots; pids are still recycled *within* a boot, which is what
`process_start_unix` and `PID_REUSE_SLACK_SECS` catch. The rules compose:

1. `boot` is `Some` and differs from this boot's token → **stale, definitively.**
   No pid inspection; the pid refers to another boot or another machine.
2. Otherwise, today's logic unchanged: stale when the pid is gone, or when the
   process holding it started more than `PID_REUSE_SLACK_SECS` after
   `created_unix`.
3. `boot` is `None` (an old entry, or a platform that cannot answer) → today's
   logic, unchanged. `serde` defaults the field, so existing `meta.json` files
   keep parsing.

The asymmetry in rule 2's slack is deliberate and stays: being wrong towards
"keep" costs one extra line in `--list`; being wrong towards "delete" destroys
a live session's socket.

## What this does not do

- **No socket/registry split.** An earlier draft put `meta.json` on durable
  storage and the socket on local disk, recording the socket path in the
  metadata. Once the registry no longer needs `$HOME`, the split buys nothing
  and costs a schema field plus two location policies.
- **No hostname field.** `boot` covers the wrong-machine case on Linux, and a
  hostname is weaker: it collides, it changes, and it says nothing about
  reboots.
- **No abstract-namespace sockets.** Attractive on Linux — no filesystem, exact
  lifetime, nothing to clean up — but they have no filesystem permissions, so
  `SO_PEERCRED` would become load-bearing security, and being Linux-only they
  need this design as a fallback anyway. Additive, not alternative; revisit if
  `/dev/shm` disappoints.
- **No migration.** Entries under `$HOME/.local/state/oxutrm` are not read or
  moved. They describe processes that are dying anyway, and the directory
  remains a last-resort target, so nothing is orphaned that was not already
  ephemeral.

## Testing

**The ordering** (`registry_root_candidates`) stays a pure function of
`RootEnv`, tested as the existing decision table is. New rows: shm present;
shm absent (the macOS shape, where `runtime_dirs_exist` is also `false`);
lingering on versus off ahead of shm; an explicit `$OXUTRM_STATE_DIR`
short-circuiting everything.

**The verification** (`prepare_root`) is about the filesystem and needs real
directories. Accepted: absent (created `0700`); a directory owned by us at
`0700`; a directory owned by us at the wrong mode (chmod'd back, then used).
`Occupied`: a plain file; a symlink, including one pointing at a directory we
*do* own, which is what catches a `lstat`-versus-`stat` mistake; a directory
owned by another uid. `Unavailable`: a parent that is world-writable without
the sticky bit. This is the security-relevant part and gets the injection
treatment the project requires — break each check, watch its test fail, restore
it.

**The walk** (`resolve_registry_root`) is where the two failure kinds are
distinguished, and getting them backwards is the downgrade attack:

- `Unavailable` on the first candidate → the second is chosen.
- **`Occupied` on the first candidate → the walk FAILS.** It must not reach the
  second. The test that matters most is the attack itself: squat every local
  candidate and assert oxutrm reports the squat rather than quietly landing on
  `HomeState`.
- A refused **explicit** `$OXUTRM_STATE_DIR` fails on any error kind, never
  falling through.

Boot identity needs a round-trip test that an entry written under a different
token is reported stale without consulting the pid at all, and that a `None`
entry still follows the old path.

## A correction to make while in this file

`process_start_unix`'s macOS arm carries:

> **Compile-verified against `aarch64-apple-darwin`, never run — nobody on the
> project has a Mac yet.**

Both halves are now false: the suite was run on a Mac on 2026-09-04. The
comment should say what is actually true about that function's evidence. A
comment that outlives its truth is this project's named signature defect, and
this one sits on the pid-reuse guard that boot identity is being layered onto.

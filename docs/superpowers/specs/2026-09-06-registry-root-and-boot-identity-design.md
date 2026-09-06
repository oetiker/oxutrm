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
/// `Err` means "refused" — the caller moves to the next candidate.
pub fn prepare_root(root: &RegistryRoot) -> anyhow::Result<()>;
```

`resolve_registry_root` walks the candidate list, calling `prepare_root` on
each and taking the first that succeeds. Refusal falling through to the next
candidate is then a property of the walk, testable against a real filesystem,
while the ordering stays a pure table testable from either platform.

An explicit `$OXUTRM_STATE_DIR` is the one candidate that must **not** fall
through: it is never second-guessed, so if `prepare_root` refuses it the walk
fails loudly rather than silently choosing somewhere the user did not ask for.

### The directory must be proven ours

`/dev/shm` and `/var/tmp` are both `1777` sticky world-writable. Any user can
create `/dev/shm/oxutrm-1003` first. `prepare_root` must therefore verify,
without following symlinks, that an existing path is a real directory owned by
our uid with no group or other permission bits. A path that exists and fails
that test is refused — never "fixed" by `chmod`, and never used.

`set_private_mode(…, 0o700)` already exists and is the right tool for creation.
The ownership-and-type check on a **pre-existing** directory is the new logic,
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
directories: one owned by us at `0700` (accepted), a symlink where the
directory should be (refused), a directory with group or other bits set
(refused), a plain file (refused). This is the security-relevant part and gets
the injection treatment the project requires — break the check, watch the test
fail, restore it.

**The walk** (`resolve_registry_root`) is where refusal-falls-through is
proved: point the first candidate at something `prepare_root` refuses and
assert the second is chosen; then assert that a refused **explicit**
`$OXUTRM_STATE_DIR` fails instead of falling through.

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

# Tier B1 hand test — method, not yet run

**Status: NOT RUN.** Every table below is empty on purpose. This file is the
recipe a person runs against the real host (`thinlinc`) over ssh, with the two
ends timestamped and liveness read from the process or the exit file — never
from a tmux pane, which keeps showing the last painted screen after the
process behind it has exited. Nothing here may be read as a finding until the
corresponding blank has been filled in from an actual run; a prediction in a
handoff is a hypothesis, not a finding, and this project was burned once
already by a confident report from a frozen pane.

This note follows the precedent of
`docs/superpowers/notes/2026-08-30-tier-a-hand-test.md`: same tone, same
structure, same rule that an unfilled slot must look unfilled rather than
plausible.

## Why this could not be run in this session

Two independent reasons, both real, neither worked around:

1. **The brief's Step 2 recipe does not describe a runnable test as written.**
   It says to run `oxutrm host --attach <id>` from a third terminal and asks
   whether the screen arrives complete. But `run_host_attach` (`src/main.rs`)
   is a *signalling* relay between this terminal's stdin/stdout and the
   session's Unix socket — its peer is meant to be a client process speaking
   the attach protocol, not a human typing at a prompt. Run by hand in a
   terminal, it emits the host's hello over the socket and then waits for a
   client hello that a human terminal will never send. No screen can arrive
   that way.

   The client side cannot be pointed at the socket either, to make this a
   real two-process test: `SshChannel::open`
   (`crates/oxutrm-host/src/ssh.rs:231`) appends the hardcoded
   `REMOTE_SERVE = ["oxutrm", "host", "--serve"]`, and `src/connect.rs:58`
   calls it via `SshLauncher::ssh()` with no CLI override to ask for
   `--attach` instead — a standing project constraint. This is exactly the B2
   gap the plan itself names — B1 ships the host-side half of reattach but no
   client-side attach path — and the plan's own hand-test recipe reaches into
   that gap without noticing it.

2. `ssh` is unavailable to this session (project policy requires explicit
   user confirmation before any network-reaching command, which was not
   sought for this task), and the run needs a real remote host.

Given both, the honest deliverable for this step is the method below, with
every observation slot left visibly empty, rather than an invented or
predicted set of numbers.

## The scaffold this method needs, and why it is acceptable here

Because no `--attach`-requesting client exists yet (that is B2), the only way
to observe the host's second-attach behaviour with the *real* client and
*real* ssh is to make the far end run `oxutrm host --attach <id>` in place of
`oxutrm host --serve`, without changing anything under test. The way to do
that:

**Shadow `oxutrm` on the target with a one-line shim earlier in the remote
`PATH`**, so the command ssh actually executes is the shim, not the real
binary — and the shim's only job is to exec the real binary with
`host --attach <id>` appended, discarding whatever argv ssh tried to pass
(`host --serve`).

This is a scaffold standing in for the B2 gap, not a workaround of it. It
changes only the argv the far end receives; it does not touch
`run_host_attach`, `SshChannel::open`, `REMOTE_SERVE`, or anything else under
test. **A B2 client with a real `--attach` path over ssh would not need this
shim at all** — it would ask for `--attach` directly, and this whole section
would be deletable.

**This is acceptable on `thinlinc` and would not be acceptable on a machine
with real users**, because the user has confirmed there is no productive
`oxutrm` on `thinlinc` — shadowing the binary in its `PATH` for the duration
of one hand test disturbs nothing else running there. On a shared host where
other people rely on `oxutrm` actually meaning `oxutrm host --serve`, this
exact shim would silently redirect their sessions into `--attach` against
someone else's id, which would be a real incident, not a test artifact.

### Concrete steps for the shim

**Verified on `thinlinc`: `~/.local/bin` is on the non-interactive ssh
`PATH`** — `ssh thinlinc 'echo $PATH'` shows it, so `ssh host command`
resolves `oxutrm` to `~/.local/bin/oxutrm` without needing `.bashrc`,
`.bash_profile`, `.profile`, or `~/.ssh/environment` to run at all. (A reader
on a different machine should not assume this — `ssh host command` runs a
*non-interactive, non-login* shell on most configurations, which typically
does **not** source the interactive-only parts of shell startup files, so
re-check with the same `ssh <host> 'echo $PATH'` probe before trusting it
there.) Given that fact, the shim does not need a separate earlier-in-`PATH`
directory at all: it can replace `~/.local/bin/oxutrm` directly.

1. Pick the session id to attach to. It comes from
   `ssh thinlinc '/scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm host --list'`
   at recipe step 2, once step 1 has a session running — **by absolute path,
   and before the shim exists**, for the reason spelled out at step 3.
2. `~/.local/bin/oxutrm` is currently a **symlink** to
   `/scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm` (the rebuilt
   binary — see "Rebuild the host binary" below). Replace the symlink with a
   shim script at the same path:

   ```bash
   mv ~/.local/bin/oxutrm ~/.local/bin/oxutrm.real-symlink   # save the symlink itself, not its target
   cat > ~/.local/bin/oxutrm <<'EOF'
   #!/bin/sh
   exec /scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm host --attach <id>
   EOF
   chmod +x ~/.local/bin/oxutrm
   ```

   The shim execs the real binary by its **absolute path**, so there is no
   recursion through the now-shadowed `~/.local/bin/oxutrm`.
3. **Verify before using it for the real test — by LOOKING at it, never by
   running it.**

   ```bash
   ssh thinlinc 'which oxutrm; ls -la ~/.local/bin/oxutrm; cat ~/.local/bin/oxutrm'
   ```

   `which` should name `~/.local/bin/oxutrm`, `ls -la` should show a regular
   executable file (no `->` arrow) and `cat` should show the two shim lines
   with the intended id in them.

   **Do not verify with `oxutrm --version`.** The shim is
   `exec <abs-path> host --attach <id>` with no `"$@"`: it *discards* argv, so
   `ssh thinlinc 'oxutrm --version'` does not print a version — it performs a
   real attach against `<id>`, burning a generation before the test has
   started and displacing whoever is attached. That is also why every
   remaining step below that wants the real binary on the target spells out
   its absolute path.
4. **Remove the shim afterwards. This step must not be skipped.** A shim left
   in place turns every future `oxutrm <target>` on this machine into an
   attach against a specific old session id instead of a normal connection,
   silently, for whoever runs it next. Put the original symlink back — the one
   step 2 stashed, so that nothing else is left behind either:

   ```bash
   mv ~/.local/bin/oxutrm.real-symlink ~/.local/bin/oxutrm
   ```

   Only if that stash is somehow gone, recreate the symlink instead:

   ```bash
   rm ~/.local/bin/oxutrm
   ln -sf /scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm ~/.local/bin/oxutrm
   ```

   Confirm with
   `ssh thinlinc 'ls -la ~/.local/bin/oxutrm ~/.local/bin/oxutrm.real-symlink'`:
   the first should show a symlink arrow (`->`) again, not a regular
   executable file, and the second should be **gone** ("No such file or
   directory"). A leftover `oxutrm.real-symlink` is the tell that the `rm` +
   `ln -sf` branch was used and the stash was never cleaned up.

## Before any of this: rebuild the host binary

Carried over from Tier A's note, because it is still true and still
load-bearing: **the host binary on `thinlinc` must be rebuilt from this
branch (`feat/session-recovery-tier-b1`) before running this test.** A binary
that predates this branch cannot exercise a socket this branch is what binds
— `run_host_attach`, `HostSession::adopt`, and `run_with_attaches` are all new
here. Tier A's equivalence note ("host-equivalent to trunk") does not carry
forward across a phase that changes host code, and this one does. If the
socket isn't bound because the binary predates the branch, the hand test
would fail for the wrong reason — a missing feature masquerading as a
protocol or timing bug.

**`/scratch/oetiker/oxutrm-handtest` already exists** as a detached worktree
left behind on purpose by the Tier A run (see that note's cleanup section).
Reusing it is the expected case, not the exception — `git worktree add` to an
already-registered path errors out, so do not copy a bare `add` command
blindly. Check which state applies and use the matching branch:

```bash
# Expected case: the worktree from Tier A (or a previous B1 run) is still there.
git -C /scratch/oetiker/oxutrm-handtest fetch origin --prune
git -C /scratch/oetiker/oxutrm-handtest checkout --detach <this branch's HEAD sha>
cd /scratch/oetiker/oxutrm-handtest
CARGO_TARGET_DIR=/scratch/oetiker/cargo-target-oxutrm-main \
  cargo build --release -j4

# Only if the worktree is genuinely gone (e.g. someone ran `worktree remove`):
git -C checkouts/oxutrm worktree add --detach /scratch/oetiker/oxutrm-handtest <sha>
cd /scratch/oetiker/oxutrm-handtest
CARGO_TARGET_DIR=/scratch/oetiker/cargo-target-oxutrm-main cargo build --release -j4
```

The explicit `CARGO_TARGET_DIR` is what keeps
`~/.local/bin/oxutrm` (a symlink into that same target dir) pointing at the
new binary with no repointing needed — Tier A's note found that leaving this
implicit builds into `.bashrc`'s different default target dir and leaves the
old binary in place, silently.

**Already confirmed working on `thinlinc` at `origin/main`** (not yet this
branch's HEAD, but the same build path this section describes): the reuse
path above, release build in 1m21s, producing an 8.9 MB `x86_64` binary,
`--version` printing `oxutrm 0.2.0`. `cargo` there is **1.96.0**, which is
exactly this project's MSRV floor — a toolchain downgrade on `thinlinc` would
break this build outright, so if the build fails with a version-gated
feature error, check `cargo --version` before anything else. The vendored
`quinn-proto` `[patch.crates-io]` (see `docs/quinn-pto-backoff.md`) applied
cleanly there too — it had previously only been exercised on macOS, so
Linux/x86_64 was an open question this build closes.

**A version string alone does not prove the binary is from this branch.**
Both a pre-B1 binary and a post-B1 binary print `oxutrm --version` as
`0.2.0` — the crate version was not bumped for this branch, so `--version`
cannot discriminate them. Trusting it here would repeat exactly the mistake
this project was burned by once already (a confident report from stale
state). Verify instead with something that actually distinguishes the two
builds:
- the target binary's own timestamp, immediately after the build
  (`ls -la /scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm`) should
  be from the run just performed, not from an earlier rebuild; or
- the sha actually checked out in the worktree
  (`git -C /scratch/oetiker/oxutrm-handtest rev-parse HEAD`) should match
  this branch's HEAD, not `origin/main` or an older commit.

Both of those are file and repository inspections, which is the second reason
to prefer them: this whole section runs **before** the shim is installed, and
once it is installed nothing on `thinlinc` may be verified by running
`oxutrm` at all — see recipe step 3.

Do not let the rebuild silently repoint the `~/.local/bin/oxutrm` symlink at
a different tree; if it does, put it back
(`ln -sf /scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm
~/.local/bin/oxutrm`) before running anything below.

**Leave `~/checkouts/oxutrm` on `thinlinc` alone.** Tier A's note found it
stale at `39b2379` and dirty with 5 modified files, and that is still true.
None of the commands above touch it — they operate on the separate detached
worktree at `/scratch/oetiker/oxutrm-handtest` specifically so this dirty
checkout does not need to be disturbed.

## Rules carried over from Tier A, still load-bearing

- **Check liveness from the process or the exit file, never from a tmux
  pane.** A pane keeps showing the last painted screen after the process
  behind it has exited — that produced a wrong "still alive" report in an
  earlier phase. `kill -0 <pid>` or an `EXITED rc=...` line in the client's
  redirected stderr are the only trustworthy liveness signals here.
- **Timestamp both ends with `date +%T` before calling any number a defect.**
  Run it on the Mac and on `thinlinc` immediately before and after each
  numbered step and record all values. A previous phase's "6 s counter
  overstatement" turned out to be 14 s actual against 15 s displayed, once
  both ends were actually timestamped.
- **`kill -CONT` before `kill -TERM` in cleanup.** A `SIGSTOP`ped session
  survives a plain `kill`; one was observed still in stopped state roughly
  ten hours after a Tier A hand test. Whether `SIGTERM` alone would suffice
  is untested and stays untested here too — sending `CONT` first removes the
  question rather than depending on it.
- **Every hand test leaves a session behind that cannot be reattached** once
  this test's own attach has taken it, so cleanup (`pgrep -a -f oxutrm`, kill
  what is left) is budgeted into the test's time, not bolted on after.
- **Something seen once and not reproduced is not a known defect, and its
  non-reproduction is not a fix.** Record anomalies as anomalies; do not
  smooth them into either a confirmed bug or a confirmed non-issue on a
  single observation.

## The recipe

**Every `oxutrm` command run ON `thinlinc` below uses the absolute path**, not
the bare name. From the moment the shim is installed at step 3, `oxutrm` on
that machine means "attach to `<id>`, discarding argv" — so a bare
`oxutrm host --list` would not list anything, it would perform an attach and
burn a generation. Only the Mac-side `oxutrm thinlinc` is deliberately bare:
resolving through the shim on the far end is the whole point of the scaffold.
For brevity below:

```bash
OX=/scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm
```

### Part 1 — the LIVE takeover (a second attach while a client is still there)

This is the case B4 builds on, and it is the only one that produces
`TAKEN_OVER` and a displaced-client experience. It has to come first, because
it needs the first client still attached.

1. **Terminal 1 (Mac):** `oxutrm thinlinc`. Run something that leaves a
   recognisable screen (e.g. `date` a few times, or a short-lived counter).
   **Leave it attached** — do not detach. Note this terminal's size
   (`stty size` inside the session, or the window's own dimensions).
2. **Terminal 2 (Mac):** `ssh thinlinc "$OX host --list"` — note the session
   id and the `attach` number in the listing (this is the `attach_id`
   generation counter mirrored from `HostHello.attach_id`; a second attach is
   expected to bump it).
3. Set up the shim per "The scaffold this method needs" above, pointed at the
   id from step 2, and verify it **by looking at it** (`ls -la` + `cat`) —
   never by running it.
4. **Terminal 3 (Mac), deliberately a DIFFERENT SIZE from terminal 1:** run
   the real client through real ssh (`oxutrm thinlinc`, unmodified) — ssh now
   lands on the shim, which execs `oxutrm host --attach <id>` instead of
   `--serve`. Record:
   - Does the screen arrive complete, and how long did it take?
   - **Is the shell at THIS terminal's geometry?** Run `stty size` in the
     reattached session and compare with terminal 3's own size. A review
     found `adopt` recording the newcomer's size without resizing the pty or
     the emulator, which left the shell at terminal 1's geometry until the
     window was resized by hand; this is the field check for that fix.
   - **What did terminal 1 — the client that was still attached — show at the
     moment it was displaced?** Specifically: did it say it had been taken
     over (`TAKEN_OVER` is the close reason the host sends), or did it just go
     silent / report the host as unreachable? Copy the exact wording.
5. **Terminal 2:** `ssh thinlinc "$OX host --list"` again — did `attach`
   increment in the registry?

### Part 2 — the reattach AFTER a detach

6. Detach terminal 3's client normally (not `Ctrl-\ q` — see below) and
   observe: **does `oxutrm host --attach` (via the shim) return promptly when
   the far end hangs up, and is any output lost at that moment?** This is not
   in the brief's original question list — it is here because of a review
   finding. `run_host_attach` calls `runtime.shutdown_background()` on exit
   rather than letting the runtime drop naturally, because a blocking stdin
   read cannot be cancelled, only abandoned, and a normally-dropped runtime
   would wait for it forever. That half now has an automated guard
   (`tests/host_attach.rs`). What still does not: `tokio`'s `io-std` feature
   backs `stdout()` with the blocking pool too, so if the stdin→socket branch
   of the `tokio::select!` in `run_host_attach` wins the race, the
   socket→stdout branch can be cancelled mid-write, and `shutdown_background`
   does not wait for that write to finish either. What could be lost is the
   tail of a screen update at the exact moment of detach. It is self-healing
   — the host's first datagram of any fresh attach is a full state (design
   spec §8.5, `docs/superpowers/specs/2026-08-29-session-recovery-design.md`)
   — and the window is narrow, but **nothing automated tests that, and this
   hand test is the only place it is exercised.** Watch for it and record
   whatever is observed, including "nothing observed" if that is what
   happens.
7. With **no client attached** (step 6 left the session detached), run
   `oxutrm thinlinc` again — the shim is still installed and still points at
   the same id, so this is a reattach-after-detach rather than a takeover.
   Record:
   - Does the screen arrive complete, and does it still show what the shell
     did while nobody was attached?
   - Does the generation (`attach` in
     `ssh thinlinc "$OX host --list"`) keep moving?
   - Nothing should be displaced this time, because there was no live client
     to displace. Note it if anything is.
8. **Remove the shim.** Do not skip this — see above, including the check that
   `~/.local/bin/oxutrm.real-symlink` is gone.
9. **Cleanup**: `pgrep -a -f oxutrm` on both ends, `kill -CONT` then
   `kill -TERM` whatever is left, confirm `ssh thinlinc "$OX host --list"`
   reports no live sessions.

## Observations

*(All slots below are intentionally empty. Fill in only from an actual run;
do not estimate or infer a plausible-looking number.)*

| Step | Observation |
|---|---|
| Mac `date +%T` at test start | — not run — |
| `thinlinc` `date +%T` at test start | — not run — |
| Session id used | — not run — |
| Terminal 1 size (`stty size` inside the session) | — not run — |
| Terminal 3 size (deliberately different) | — not run — |
| `attach` number before this test's first attach | — not run — |
| Shim verified by `ls -la ~/.local/bin/oxutrm` (regular file, no `->`) | — not run — |
| Shim verified by `cat ~/.local/bin/oxutrm` (the two lines, right id) | — not run — |
| **Part 1, live takeover:** screen arrived complete? | — not run — |
| Part 1: time to arrive | — not run — |
| Part 1: `stty size` in the reattached session vs terminal 3's own size | — not run — |
| Part 1: what terminal 1 (still attached) showed at the moment it was displaced, verbatim | — not run — |
| Part 1: did the displaced client name the takeover, or just go silent? | — not run — |
| Part 1: `attach` number after the takeover, per `--list` | — not run — |
| **Part 2, reattach after detach:** `run_host_attach` return time after far end hangs up | — not run — |
| Part 2: any output lost at that moment (tail of a screen update)? | — not run — |
| Part 2: screen arrived complete, including what ran while detached? | — not run — |
| Part 2: `attach` number after it — does the generation keep moving? | — not run — |
| Part 2: anything displaced, though there was nothing to displace? | — not run — |
| Shim removed and symlink restored (`ls -la ~/.local/bin/oxutrm` shows `->` again)? | — not run — |
| `~/.local/bin/oxutrm.real-symlink` gone after restore? | — not run — |
| `pgrep -a -f oxutrm` after cleanup | — not run — |

## Anomalies

*(None recorded — the test has not been run. Anomalies seen during a real run
belong here, each described in full, with the standing rule applied: seen
once and not reproduced is not a known defect, and non-reproduction is not a
fix.)*

## The honest limit

**No automated test on this branch drives a second attach all the way to a
newcomer painting a screen.** The composed and unit tests added across Tasks
1-4 cover `HostSession::adopt`, `run_with_attaches`, the socket binding, and
`run_host_attach`'s own wiring in isolation, but none of them puts a real
client process on the far end of a real attach and watches a screen arrive —
that needs the client-side attach path, which is B2's job to extract. **B1's
evidence for its headline behaviour — that `oxutrm host --attach <id>` moves
a live shell between terminals — is this one hand test, run once, on one
machine, once it is actually run.** That is the same standing of evidence the
PTO backoff fix had, and it is worth the same scepticism: a single run on a
single machine confirms the mechanism fires and looks right, not that it is
free of the kind of anomaly Tier A's note found (and left unexplained) in its
first afternoon of real use.

**Something seen once and not reproduced is not a known defect, and its
non-reproduction is not a fix.** This applies to whatever this run finds, the
same as it applied to Tier A's.

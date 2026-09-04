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

1. Pick the session id to attach to (from `oxutrm host --list` on `thinlinc`,
   after Test 1 below has left a detached session behind — see the recipe).
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
3. **Verify before using it for the real test**: a throwaway
   `ssh thinlinc 'which oxutrm; oxutrm --version'` should still print
   `oxutrm 0.2.0` (the shim's `exec` makes it behave like the real binary for
   `--version` too — see the discrimination note below for why a version
   string alone is not enough to trust the *build*, though it is enough here
   to confirm the shim itself is reachable and executable).
4. **Remove the shim afterwards. This step must not be skipped.** A shim left
   in place turns every future `oxutrm <target>` on this machine into an
   attach against a specific old session id instead of a normal connection,
   silently, for whoever runs it next. Restore the symlink exactly:

   ```bash
   rm ~/.local/bin/oxutrm
   ln -sf /scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm ~/.local/bin/oxutrm
   ```

   (equivalent to `mv ~/.local/bin/oxutrm.real-symlink ~/.local/bin/oxutrm`
   if that file from step 2 is still around). Confirm removal with
   `ssh thinlinc 'ls -la ~/.local/bin/oxutrm'` — it should show a symlink
   arrow (`->`) again, not a regular executable file.

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

1. `oxutrm thinlinc`, run something that leaves a recognisable screen (e.g.
   `date` a few times, or a short-lived counter), detach cleanly.
2. `oxutrm host --list` on `thinlinc`, in a second terminal — note the
   session id and the `attach` number shown in the listing (this is the
   `attach_id` generation counter mirrored from `HostHello.attach_id`; a
   second attach is expected to bump it).
3. Set up the shim per "The scaffold this method needs" above, pointed at
   the id from step 2. Verify it resolves before using it for the real test.
4. From a **third** terminal, run the real client through real ssh
   (`oxutrm thinlinc`, unmodified) — ssh now lands on the shim, which execs
   `oxutrm host --attach <id>` instead of `--serve`. Record:
   - Does the screen arrive complete, and how long did it take?
   - What did the **second** terminal (the one that had the session before
     this attach) show at the moment it was displaced?
5. `oxutrm host --list` again: did `attach` increment in the registry?
6. Detach the client normally (not `Ctrl-\ q` — see below) and observe:
   **does `oxutrm host --attach` (via the shim) return promptly when the far
   end hangs up, and is any output lost at that moment?** This is not in the
   brief's original question list — it is here because of a review finding.
   `run_host_attach` calls `runtime.shutdown_background()` on exit rather
   than letting the runtime drop naturally, because a blocking stdin read
   cannot be cancelled, only abandoned, and a normally-dropped runtime would
   wait for it forever. The reason is real. But `tokio`'s `io-std` feature
   backs `stdout()` with the blocking pool too, so if the stdin→socket branch
   of the `tokio::select!` in `run_host_attach` wins the race, the
   socket→stdout branch can be cancelled mid-write, and `shutdown_background`
   does not wait for that write to finish either. What could be lost is the
   tail of a screen update at the exact moment of detach. It is self-healing
   — the host's first datagram of any fresh attach is a full state (design
   spec §8.5, `docs/superpowers/specs/2026-08-29-session-recovery-design.md`)
   — and the window is narrow, but **nothing automated tests this path, and
   this hand test is the only place it is exercised.** Watch for it and
   record whatever is observed, including "nothing observed" if that is what
   happens.
7. Repeat the shim setup for a fourth attach (a new shim pointed at the same
   id, or the same shim re-run — either is fine since the id does not
   change). Does the generation (`attach` in `--list`) keep moving?
8. **Remove the shim.** Do not skip this — see above.
9. **Cleanup**: `pgrep -a -f oxutrm` on both ends, `kill -CONT` then
   `kill -TERM` whatever is left, confirm `oxutrm host --list` reports no
   live sessions.

## Observations

*(All slots below are intentionally empty. Fill in only from an actual run;
do not estimate or infer a plausible-looking number.)*

| Step | Observation |
|---|---|
| Mac `date +%T` at test start | — not run — |
| `thinlinc` `date +%T` at test start | — not run — |
| Session id used | — not run — |
| `attach` number before this test's first attach | — not run — |
| Shim verified reachable before real test (`ssh thinlinc 'which oxutrm; oxutrm --version'` output) | — not run — |
| Screen arrived complete? | — not run — |
| Time to arrive | — not run — |
| Displaced terminal's screen at the moment of takeover | — not run — |
| `attach` number after the attach, per `--list` | — not run — |
| `run_host_attach` return time after far end hangs up | — not run — |
| Any output lost at that moment (tail of a screen update)? | — not run — |
| `attach` number after a fourth attach — does the generation keep moving? | — not run — |
| Shim removed and symlink restored (`ls -la ~/.local/bin/oxutrm` shows `->` again)? | — not run — |
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

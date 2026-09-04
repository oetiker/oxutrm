# Tier B1 hand test — method, and the run

**Status: RUN on 2026-09-04, 16:01–16:05 CEST, against `thinlinc` from macOS,
with the binary built from `68d12d5` (trunk).** Both parts are TAKEN and every
observation slot below is filled from that run.

**Headline: B1's claim holds. A live shell moved between three terminals of
three different sizes, keeping its scrollback, and the displaced client said
it had been taken over rather than going silent.** The generation counter
advanced 1 → 2 → 3 in the registry and the shell's pid never changed. The
`adopt`-resize bug that the whole-branch review found by reading is confirmed
fixed in the field: each newcomer got its own geometry, not its predecessor's.

**The one thing this run could NOT exercise is the question the note added
for it** — `run_host_attach`'s behaviour when the far end hangs up. The relay
had already exited on its own before any hangup, which turns out to be by
design and to mean the risk was described in the wrong place. See
"Anomalies", item 2: it is a correction to a code comment, not a defect.

The original text of the method follows, unchanged except for the filled-in
tables. This file is the recipe a person runs against the real host
(`thinlinc`) over ssh, with the two ends timestamped and liveness read from
the process or the exit file — never from a tmux pane, which keeps showing the
last painted screen after the process behind it has exited (**that trap fired
again in this run — see Anomalies item 1**). Nothing here may be read as a
finding unless the corresponding slot was filled in from the actual run; a
prediction in a handoff is a hypothesis, not a finding, and this project was
burned once already by a confident report from a frozen pane.

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

*(Every slot is filled from the run of 2026-09-04. Nothing here is estimated
or inferred; where something could not be observed, that is what it says.)*

| Step | Observation |
|---|---|
| Mac `date +%T` at test start | `16:01:42` |
| `thinlinc` `date +%T` at test start | `16:01:42` — the two ends agreed to the second, so no number below needs a clock correction |
| Session id used | `65d0e54d9f125b657df50b4572c1695b`, host pid `3535400` |
| Terminal 1 size (`stty size` inside the session) | `30 100` (tmux `-x 100 -y 30`) |
| Terminal 3 size (deliberately different) | `40 120` (tmux `-x 120 -y 40`) |
| `attach` number before this test's first attach | `attach 1` |
| Shim verified by `ls -la ~/.local/bin/oxutrm` (regular file, no `->`) | Yes — `-rwxr-xr-x 1 oetiker oep 119 Sep 4 16:02`, no arrow |
| Shim verified by `cat ~/.local/bin/oxutrm` (the two lines, right id) | Yes — `#!/bin/sh` + `exec /scratch/…/oxutrm host --attach 65d0e54d…`, id matched |
| **Part 1, live takeover:** screen arrived complete? | **Yes.** Terminal 3 showed the running shell's screen including `TERMINAL-ONE-MARKER 16:02:08`, the marker terminal 1 had printed — i.e. the shell itself moved, scrollback intact |
| Part 1: time to arrive | Client launched `16:02:43`, screen observed complete at `16:02:48` — **≤5 s**. Not measured more finely; the observation was a poll, not a timer, so this is an upper bound |
| Part 1: `stty size` in the reattached session vs terminal 3's own size | **`40 120` — matches terminal 3, not terminal 1's `30 100`.** This is the field confirmation of the `adopt` resize fix |
| Part 1: what terminal 1 (still attached) showed at the moment it was displaced, verbatim | On its stderr: `Error: the host closed the session without the shell exiting: taken over by a newer attach`, then `EXITED rc=1`. Its tmux **pane** still showed the last painted screen — see Anomalies item 1 |
| Part 1: did the displaced client name the takeover, or just go silent? | **Named it explicitly** ("taken over by a newer attach"). It did not report silence or an unreachable host — which is the whole point of `TAKEN_OVER` |
| Part 1: `attach` number after the takeover, per `--list` | `attach 2`, and the registry's recorded size changed to `120x40`. Host pid unchanged at `3535400` |
| **Part 2, reattach after detach:** `run_host_attach` return time after far end hangs up | **Could not be exercised as framed.** No `host --attach` process was alive even *before* the hangup — the relay exits as soon as the signalling exchange completes. So it returns promptly, but not for the reason the question assumed. See Anomalies item 2 |
| Part 2: any output lost at that moment (tail of a screen update)? | **None observed** — and per Anomalies item 2, no screen update ever flows through that relay, so this is not the place such a loss could occur |
| Part 2: screen arrived complete, including what ran while detached? | Screen arrived complete at the third size — full history, both `TERMINAL-ONE-MARKER 16:02:08` and `TERMINAL-THREE 16:03:00`. **"What ran while detached" was NOT exercised**: nothing was started before the detach, so there was no detached-period output to carry. A gap in this run, not a finding |
| Part 2: `attach` number after it — does the generation keep moving? | **Yes — `attach 3`**, registry size `90x25`, host pid still `3535400`. Geometry followed a third time (`stty size` → `25 90`) |
| Part 2: anything displaced, though there was nothing to displace? | Nothing. `err4` stayed empty and no client reported a takeover — correct, since no client was attached |
| Shim removed and symlink restored (`ls -la ~/.local/bin/oxutrm` shows `->` again)? | Yes — `lrwxrwxrwx … -> /scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm`. `oxutrm --version` printing `0.2.0` normally is itself the proof the shim is gone: under the shim it would have attached instead |
| `~/.local/bin/oxutrm.real-symlink` gone after restore? | Yes — `No such file or directory`. The `mv` branch was used, so no stash was left behind |
| `pgrep -a -f oxutrm` after cleanup | `thinlinc`: no live sessions, no processes. Mac: no clients, tmux server killed |

## Anomalies

Three, none of them a failure of B1's claim. The standing rule applies to all
of them: **something seen once and not reproduced is not a known defect, and
its non-reproduction is not a fix.**

### 1. The tmux-pane trap fired again, exactly as this note warns

After terminal 1 was displaced, its tmux pane still showed the full, healthy,
last-painted screen — prompt and all. Nothing about the pane suggested the
process behind it had died. The only signals that told the truth were the
redirected stderr (`taken over by a newer attach`, `EXITED rc=1`) and the
process table.

Not a defect: it is how tmux works, and this note and Tier A's both warn about
it. Recorded because it fired **in the one run that was watching for it**,
which is the strongest possible argument for keeping the rule. A reader who
had judged the takeover from the pane would have concluded the displaced
client was fine.

### 2. The relay's lifetime is the exchange, not the session — so the `shutdown_background()` risk is described in the wrong place

The observation slot for "does `run_host_attach` return promptly when the far
end hangs up" could not be filled as intended, and finding out why is the most
useful thing this run produced besides the headline.

**No `oxutrm host --attach` process was alive on `thinlinc` at all**, at any
point after the takeover completed — checked before the hangup, while the
session was working normally. `run_host_attach` relays *signalling*: once the
attach exchange finishes and the QUIC path is up, terminal traffic flows
directly between the client and the host session, the relay's socket side
completes, and the process exits on its own. It never sees a user detach,
because it is already gone by then.

That means the risk `run_host_attach`'s comment describes is real but
mislocated. The comment (and this note's own step 6, and the review finding
behind both) frames the cancelled-write window as losing **"the tail of a
screen update at the exact moment of detach."** No screen update ever traverses
that relay. What could be lost is the tail of a **signalling message**, at the
moment the exchange completes — a bounded, single-message loss, not a stream,
and at a different point in the session's life than anyone had assumed.

The `shutdown_background()` call itself is not implicated: the process was
observed exiting promptly and unaided every time, which is the property it
exists to protect. **What needs fixing is the comment at `src/main.rs`, and
step 6 of this note.** Filed rather than fixed here, so that the correction is
made with a reviewer rather than in the same breath as the observation.

### 3. `thinlinc` warns on every invocation that the session socket may be unreliable

Not new, and not caused by B1, but B1 is the first thing that makes it matter.
Every `host --list` and `host --serve` prints:

> lingering is off for this user, so `XDG_RUNTIME_DIR` is destroyed at logout
> and a detached session would become unreachable, so sessions are recorded in
> `/home/oetiker/.local/state/oxutrm` instead … **on a networked home
> directory the session socket may be unreliable.**

B1's whole feature is a Unix socket bound in that directory. On this run it
worked perfectly across three attaches — but a Unix socket on a networked
home directory is exactly the kind of thing that works until it does not, and
"it worked once on one machine" is the standing of evidence here. Worth a
deliberate look before anyone concludes reattach is reliable in this
deployment: either `loginctl enable-linger $USER`, or `OXUTRM_STATE_DIR`
pointed at local disk.

## The honest limit

**No automated test on this branch drives a second attach all the way to a
newcomer painting a screen.** The composed and unit tests added across Tasks
1-4 cover `HostSession::adopt`, `run_with_attaches`, the socket binding, and
`run_host_attach`'s own wiring in isolation, but none of them puts a real
client process on the far end of a real attach and watches a screen arrive —
that needs the client-side attach path, which is B2's job to extract. **B1's
evidence for its headline behaviour — that `oxutrm host --attach <id>` moves
a live shell between terminals — is this one hand test, run once, on one
machine, on 2026-09-04.** That is the same standing of evidence the PTO
backoff fix had, and it is worth the same scepticism: a single run on a single
machine confirms the mechanism fires and looks right, not that it is free of
the kind of anomaly Tier A's note found (and left unexplained) in its first
afternoon of real use.

Two specific gaps in *this* run, stated so nobody reads more into it than it
carries:

- **The detached-period case was not exercised.** Nothing was left running
  before the detach, so "does the screen show what the shell did while nobody
  was attached" is still unanswered.
- **The hangup case was not exercised either**, and could not be — Anomalies
  item 2 explains why the question was aimed at a process that had already
  exited.

Everything else in the recipe was run, and passed.

**Something seen once and not reproduced is not a known defect, and its
non-reproduction is not a fix.** This applies to whatever this run finds, the
same as it applied to Tier A's.

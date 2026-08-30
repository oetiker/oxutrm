# Tier A hand test — method, not yet run

**Status: NOT YET TAKEN.** Every number below is a blank waiting for a real
observation. This file is the method a person runs against the real host
(`thinlinc`) over ssh, with the two ends timestamped and liveness read from
the process or the exit file — never from a tmux pane, which keeps showing
the last painted screen after the process behind it has exited. Nothing here
may be read as a finding until the corresponding blank has been filled in
from an actual run; a prediction in a handoff is a hypothesis, not a finding,
and this project was burned once already by a confident report from a frozen
pane.

Task 6's own scope stopped short of running this: it is a side effect on a
shared machine (a rebuild, and `SIGSTOP` against live processes on
`thinlinc`), which is the user's call and not something to do
unsupervised. What follows is the recipe, ready to run.

## Before any of this: rebuild the host binary

**The host binary on `thinlinc` must be rebuilt before this hand test, full
stop.** A previous handoff recorded it as "host-equivalent to `main`" only
because phase 1 changed no host code. This phase is different: Task 3
(`fix(host): detach from a vanished client on our own clock`) added
`HostSession`'s own detach clock (`DETACH_AFTER`, `last_heard`), which is
host-side code. That equivalence is dead — the binary on `thinlinc` predates
the very mechanism Test 2 below is trying to observe.

Rebuild for `thinlinc`'s architecture, and **keep the symlink**:

```bash
~/.local/bin/oxutrm -> /scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm
```

Do not let the rebuild silently repoint the symlink at a different tree; if
it does, put it back before running anything below.

## The recipe (verbatim from the phase plan)

On the Mac:

```bash
# The trailing sleep is ESSENTIAL -- without it the tmux server dies with the
# client and the exit message is lost.
tmux new-session -d -s ox -x 100 -y 30 \
  'target/release/oxutrm thinlinc 2>/tmp/err.txt; echo "EXITED rc=$?" >> /tmp/err.txt; sleep 900'
```

- Induce silence by sending **`SIGSTOP` to the host-side session process**.
  Deterministic, instant, reversible with `SIGCONT`, no firewall rules
  needed. Get the PID from `oxutrm host --list` on `thinlinc` — **not** from
  `pgrep host --serve`, which detaches and re-execs, so the PID `pgrep` finds
  is transient and already gone by the time you send a signal to it.
- Observe with `tmux capture-pane -p`.
- **Check liveness from the process or `/tmp/err.txt`, never from pane
  content.** A pane keeps showing the last painted screen after the process
  behind it has exited; that produced a wrong "still alive at 40 s" report
  last phase. `kill -0 <pid>` or the presence/content of `EXITED rc=...` in
  `/tmp/err.txt` are the only trustworthy liveness signals.
- **Timestamp both ends with `date +%T` before calling any number a
  defect.** A claimed 6 s counter overstatement was 14 s actual against 15 s
  displayed, once both ends were actually timestamped. Run `date +%T` on the
  Mac and on `thinlinc` immediately before and after each step below and
  record all four.

### A `SIGSTOP`ped session ignores `SIGTERM` until continued

Not in the original recipe, and worth stating plainly because it was
observed for real on 2026-08-30: a session `SIGSTOP`ped during the previous
phase's hand test was still in process state `T` ten hours later and did
**not** die to a plain `SIGTERM`. A stopped process does not run its signal
handlers — `SIGTERM`'s handler (if any) cannot run until the process is
scheduled again, and `SIGSTOP`/`SIGCONT` bypass ordinary signal delivery
entirely. **Cleanup must `kill -CONT <pid>` before `kill -TERM <pid>`,** or
the stopped session survives the test and sits there consuming a PTY and a
registration entry indefinitely. Check `oxutrm host --list` after cleanup to
confirm nothing was left in that state.

## What to establish, in order

### Test 1 — `SIGSTOP` for 60 s

**Claim to verify:** the box appears at ~2 s, the counter climbs past 33 s
(the old fatal threshold), and the client is still alive at 60 s. This single
observation is the phase's whole point: before phase 2 the client died at
~33 s under exactly this condition.

Procedure:
1. `date +%T` on both ends.
2. Start the session (recipe above), confirm it is up.
3. `oxutrm host --list` on `thinlinc`; note the session's PID.
4. `kill -STOP <pid>`.
5. `tmux capture-pane -p` at roughly 2 s, 33 s, 45 s, 60 s after the stop.
   Record the pane content and a `date +%T` alongside each capture.
6. At 60 s: confirm the client process is still running (`ps`/`kill -0` from
   the Mac side, or check `/tmp/err.txt` is still empty of an `EXITED`
   line) — not from the pane.
7. `kill -CONT <pid>` (host side).
8. Confirm the notice box clears and the shell is usable again; timestamp.

| Measurement | Value |
|---|---|
| Mac `date +%T` at test start | NOT YET TAKEN |
| `thinlinc` `date +%T` at test start | NOT YET TAKEN |
| PID stopped | NOT YET TAKEN |
| Pane content at ~2 s (box appeared?) | NOT YET TAKEN |
| Pane content at ~33 s (counter value) | NOT YET TAKEN |
| Client alive at 60 s? (process/exit-file check, not pane) | NOT YET TAKEN |
| Time box cleared after `SIGCONT` | NOT YET TAKEN |
| Shell usable after clear? | NOT YET TAKEN |

### Test 2 — the host detaches and comes back

**Claim to verify:** with the *client* `SIGSTOP`ped for 40 s, the host
process is not burning a core while nobody is attached (confirmed from `top`
or `ps` **`TIME` deltas, not `%CPU`**, which is a lifetime average and reads
as near-zero on a long-lived process regardless of what it is doing right
now), and the screen is correct once the client resumes.

**Measure the case the complaint was about.** Run something that writes
steadily before stopping the client — `while true; do date; sleep 0.2; done`
— or the measurement is of an idle session and proves nothing about the
17-20%-of-a-core case Task 3 exists for.

Procedure:
1. Start the session, start the steady writer inside it.
2. `date +%T` on both ends.
3. Find the **client** process (on the Mac) and `SIGSTOP` it — this is the
   client this time, not the host, since the point is to make the *host*
   decide nobody is listening.
4. Record `ps -o pid,time,pcpu -p <host-pid>` on `thinlinc` at the moment of
   stop, then again ~20 s later and ~40 s later. The `TIME` column delta
   between samples is the actual CPU consumed in that interval — that is
   the number that matters, not `%CPU`.
5. At 40 s, `SIGCONT` the client.
6. Confirm the screen catches up correctly (the steady writer's most recent
   lines are visible, not a frozen mid-outage screen).

| Measurement | Value |
|---|---|
| Mac `date +%T` at test start | NOT YET TAKEN |
| `thinlinc` `date +%T` at test start | NOT YET TAKEN |
| Host `ps TIME` at stop | NOT YET TAKEN |
| Host `ps TIME` at +20 s | NOT YET TAKEN |
| Host `ps TIME` at +40 s | NOT YET TAKEN |
| TIME delta over the 40 s (the real cost) | NOT YET TAKEN |
| Screen correct on resume? | NOT YET TAKEN |

### Test 3 — a real route change

**This is the one that is genuinely unproven.** The handoff flags *"the
route probe is unproven under a VPN"*, and the measured source addresses on
this machine differ per peer (VPN `10.46.18.101` for the internet,
`192.168.17.5` for the LAN host), so a VPN toggle should visibly move the
route.

**Run both orderings — this is not optional, and it is not in the original
recipe.** A review of this phase found that a route which moves *before*
silence is detected cannot be seen by the probe **unless** the baseline was
seeded at connection time. It now is: `ClientSession::new` reads the source
address the connection is using the moment it comes up, specifically so that
"walk out of Wi-Fi range" — where the route has already moved by the time
`SILENT_AFTER` (2 s) notices anything — still has something to compare
against. A test that only moves the route *during* an already-established
`Silent` phase would never exercise that seeded-baseline path at all, and
would leave the common real-world case — the move happens first, the silence
is noticed after — entirely unverified.

**Ordering A — route moves first, then silence is noticed** (the common
"walked out of Wi-Fi range" case; exercises the baseline seeded in
`ClientSession::new`):
1. Start the session over the current network.
2. Toggle the network (wifi off/on onto a different network, or VPN
   connect/disconnect) — do this *before* any outage is otherwise visible.
3. Observe: does the notice appear at all (a route move alone is not
   silence — the box logic is unrelated to the route), does the probe run
   once `Silent` is entered, does a rebind fire, does the session recover?

**Ordering B — silence begins first, then the route moves mid-outage**
(the case the plan's Step 3 originally described):
1. Start the session.
2. `SIGSTOP` the host (or otherwise induce silence) so `Silent` is entered.
3. While `Silent`, toggle the network.
4. Observe the same four questions as Ordering A.

For both orderings, also be explicit about **whether the rebind is what
actually recovered the session, or whether the OS did it for free.** A
wildcard-bound socket may follow a route change on its own, because the
kernel picks the source address per outgoing packet regardless of what
`oxutrm` does — if the session survives even when `follow_route`/the rebind
logic is not what moved it, say so plainly, because that changes what phase
3 needs to build (a probe confirming an already-correct socket is a very
different job from a probe driving a rebind that is actually load-bearing).

| Measurement | Ordering A (move first) | Ordering B (move during outage) |
|---|---|---|
| Mac `date +%T` at test start | NOT YET TAKEN | NOT YET TAKEN |
| `thinlinc` `date +%T` at test start | NOT YET TAKEN | NOT YET TAKEN |
| Network change performed | NOT YET TAKEN | NOT YET TAKEN |
| Notice behaviour observed | NOT YET TAKEN | NOT YET TAKEN |
| Did the probe run? | NOT YET TAKEN | NOT YET TAKEN |
| Did a rebind fire? | NOT YET TAKEN | NOT YET TAKEN |
| Did the session recover? | NOT YET TAKEN | NOT YET TAKEN |
| Rebind load-bearing, or would a wildcard socket have followed anyway? | NOT YET TAKEN | NOT YET TAKEN |

## Cleanup checklist

- [ ] Every stopped process `CONT`ed before it is `TERM`ed (see above —
  `SIGTERM` alone does nothing to a stopped process).
- [ ] `oxutrm host --list` on `thinlinc` shows nothing left over from this
  test run.
- [ ] `~/.local/bin/oxutrm` symlink still points at
  `/scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm`.
- [ ] tmux session `ox` killed.

## What this note feeds

Three open questions this phase inherits are waiting on exactly this note:
`SILENT_AFTER` = 2 s, `REBUILD_AFTER` = 20 s, and whether the route probe
works under a VPN. None of them can be answered until the table cells above
are filled in from a real run.

## Related: the composed automated test

The automated composed test for this phase
(`session::tests::a_session_outlives_a_silence_that_used_to_kill_it` in
`src/session.rs`) covers the *session-layer* version of Test 1's claim (the
notice raises under silence and clears on its own once the host answers) as
a repeatable, 35-second, real-elapsed-time test. It is `#[ignore]`d because
of that 35 s cost; run it with:

```bash
cargo test -j4 --bin oxutrm outlives_a_silence -- --nocapture --ignored --test-threads=1
```

It does **not** substitute for Test 1 above. Verified directly (restoring
`max_idle_timeout(Some(30s))` in `crates/oxutrm-net/src/quic.rs` and
rerunning it): the composed test's connection-survival assertion does not
fail even against that reverted config, because in an automated in-process
test both peers' quinn transports stay live and keep ACKing/keep-aliving
each other regardless of what the session-layer code does — the failure
mode phase 2 fixes is specifically a **suspended host process**, which
cannot be reproduced by two live, unsuspended tasks in the same test binary.
Only `SIGSTOP` against a real process — Test 1 above — exercises that path.

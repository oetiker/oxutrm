# Tier A hand test — method, not yet run

**Status: RUN on 2026-08-30, 19:42-22:00 CEST. Tests 1, 2 and 3 (Ordering A)
are TAKEN; Test 3 Ordering B is not, deliberately and for a stated reason.**
The tables below now carry real observations, and the run notes say which
session each number came from.

**Headline: Tier A's claim holds, and it has a ceiling nobody had measured.**
The client survived a 60 s host freeze (old fatal threshold: ~33 s) and every
freeze up to 100 s recovered on its own, the notice clearing in under a
second. A detached host costs **exactly zero** measurable CPU. The route probe
and rebind were observed working on a real network under a real VPN, including
the seeded-baseline case the review added them for. **But a path outage of
~7 minutes never recovers at all** — both ends alive, nothing to reattach
them — because `REBUILD_AFTER` is spec-only and no rebuild loop exists yet.
That is the single most useful thing this run found. Two anomalies were seen once each in the first two sessions
of the afternoon and did not reproduce in five later attempts; both are
recorded in full under "Anomalies" so a later session can hunt them.

The original text of the method follows, unchanged except for the filled-in
tables. Every number below is a real observation. This file is the method a person runs against the real host
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

### Done, 2026-08-30 19:42 CEST

Rebuilt at trunk `fd9f001` (v0.2.0). The checkout at `~/checkouts/oxutrm` on
`thinlinc` was left **untouched** — it is still dirty and behind at `39b2379`
with the stale duplicate the handoff describes, and nothing here needed to
disturb it. The build ran instead from a fresh detached worktree:

```bash
git -C checkouts/oxutrm worktree add --detach /scratch/oetiker/oxutrm-handtest fd9f001
cd /scratch/oetiker/oxutrm-handtest
CARGO_TARGET_DIR=/scratch/oetiker/cargo-target-oxutrm-main cargo build --release -j4
```

The explicit `CARGO_TARGET_DIR` is what keeps the symlink honest: it is the
path the symlink already points into, so the existing
`~/.local/bin/oxutrm` picks up the new binary with no repointing. `.bashrc`
sets a *different* default (`$HOME/scratch/cargo-target`), so leaving it
implicit would have built into the wrong tree and left the stale Aug 29
binary in place, silently. Verified after: `~/.local/bin/oxutrm --version`
prints `oxutrm 0.2.0`, and the file at the symlink's target is dated
`Aug 30 19:42` (it was `Aug 29 09:23`).

**The worktree is still there** at `/scratch/oetiker/oxutrm-handtest`, so a
rerun needs no rebuild. Remove it with `git -C checkouts/oxutrm worktree
remove /scratch/oetiker/oxutrm-handtest` when it stops being useful.

### The path these measurements were taken over

`route -n get 192.168.0.11` on the Mac resolves through **`utun65`, a VPN
tunnel, source `192.168.17.5`** — so every number in Tests 1 and 2 was taken
with the session running *over a VPN*, not over the plain LAN. Worth knowing
before comparing against any earlier or later run, and it means "does this
work over a VPN at all" is now answered yes, even though Test 3's question
about a VPN *toggle* is still open.

The client's stderr went to a scratchpad file rather than `/tmp/err.txt`;
nothing else about the recipe was changed.

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

### `kill -CONT` before `kill -TERM`, as a precaution — not a proven necessity

**Correction to an earlier draft of this note**, caught in review: a prior
version of this section stated as an *observation* that a `SIGSTOP`ped
session "ignores `SIGTERM` until continued." That overstated what was
actually seen, and this document's whole rule is that nothing in it may be
read as a finding until it was actually measured — so here is exactly what
was, and was not, observed.

**What was observed:** on 2026-08-30, `ps` showed PID `2685979` still in
process state `Tl` (stopped) roughly ten hours after the previous phase's
hand test had `SIGSTOP`ped it. **What was not observed:** whether a plain
`SIGTERM` alone would have failed to kill it. Cleanup at the time sent
`kill -CONT` and `kill -TERM` together, so the counterfactual — `SIGTERM`
with no preceding `SIGCONT` — was never actually run.

The mechanism as originally stated was also over-general: on Linux, a
process with SIGTERM at its **default disposition** (not caught, not
ignored) is killed by `SIGTERM` even while stopped — the kernel delivers
default-action termination signals to a stopped process without requiring
it to be scheduled first. The claim "a stopped process cannot see `SIGTERM`
until resumed" is only true if the process has installed a *handler* for
it, and whether `oxutrm host --serve` installs one for `SIGTERM` is
**not verified** by anything in this note or its predecessor.

**What to actually do:** send `kill -CONT <pid>` before `kill -TERM <pid>`
during cleanup regardless. It is cheap, it is not wrong, and it removes the
question entirely rather than depending on `oxutrm host --serve`'s signal
disposition — which is untested here. But do not repeat the earlier
overstatement: whether `SIGTERM` alone would have sufficed is **untested**,
not disproven. If it matters later (e.g. some other tool sends `SIGTERM`
without `SIGCONT`), that is a real open question, not a closed one. Check
`oxutrm host --list` after cleanup either way, to confirm nothing was left
registered in a stopped state.

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

**Run 1a — the 60 s claim** (session `8a6e7424`, host pid 2659410, client pid
15312). Stopped 97 s in the end, well past the 60 s the test asks for.

| Measurement | Value |
|---|---|
| Mac `date +%T` at test start | 19:43:37 |
| `thinlinc` `date +%T` at test start | 19:43:38 — the two clocks agree to within the ssh round trip |
| PID stopped | host session 2659410 (client 15312, confirmed by `ps` before signalling) |
| Pane content at ~2 s (box appeared?) | **No box.** Plain shell prompt, last output still the pre-test echo |
| Pane content at ~33 s (counter value) | Box up: `no reply from host` / **`silent for 36s - sent 375 - lost 0`** — past the old fatal ~33 s and still going |
| Client alive at 60 s? (process/exit-file check, not pane) | **YES.** `kill -0 15312` succeeded and the stderr file held no `EXITED` line. Pane read `silent for 63s - sent 377 - lost 0` |
| Time box cleared after `SIGCONT` | **Did not clear in this run** — see Anomaly A. Measured properly in run 1c below: **under 1 s** |
| Shell usable after clear? | YES (run 1c) |

**The counter runs ~3 s ahead of elapsed-since-`SIGSTOP`**, consistently at
all four samples (13 s at +10 s, 36 s at +33 s, 48 s at +45 s, 63 s at +60 s).
This is correct, not a defect, and both ends were timestamped before saying
so: the displayed clock is `last_heard`, so it counts from the host's **last
answer**, which is up to one `HEARTBEAT_IDLE` (5 s) older than the moment the
host actually stopped. The box's *raise* is driven by `owed_since` and duly
appeared between +3 s and +10 s, not at +2 s, for the same reason.

**Held input, observed in passing:** typing `echo TYPED-DURING-OUTAGE`
(24 characters) into the stranded client produced
`24 bytes typed since - kept, not sent` — the exact byte count, and the
client sent nothing.

**Run 1c — recovery, with the freeze kept under `DETACH_AFTER`** (session
`bd7cb477`, host pid 2674519, client pid 19106). Stopped 20 s.

| Measurement | Value |
|---|---|
| Mac / `thinlinc` `date +%T` at start | 19:47:05 / 19:47:06 |
| Box at +3 s | Not yet up |
| Box at +10 s / +19 s | `silent for 10s` / `silent for 19s` — counter tracks elapsed exactly here, the last exchange having been recent |
| `SIGCONT` at | 19:47:26, after 20 s stopped |
| Time box cleared after `SIGCONT` | **Under 1 s** — gone at the first sample (CONT+1 s) and at every sample after |
| Shell usable after clear? | **YES** — `echo RECOVERED-$(date +%T)` round-tripped and printed `RECOVERED-19:47:49` |

That clearing is the ack-only fix (`d40264e`) working against a real host: an
idle host answers a heartbeat with an ack-only frame and nothing else, and
that frame alone now takes the box down.

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

**Measured with `/proc/<pid>/stat` fields 14+15 (utime+stime, 100 ticks/s),
not `ps TIME`.** `ps TIME` has one-second resolution, and the whole quantity
being measured here is under a second — it would have reported `00:00:00`
throughout and proved nothing. The `TIME`-delta-not-`%CPU` rule the note
insists on is honoured; this is the same rule at finer resolution.

Session `40b6721c`, host pid 2693054, client pid 23146. Steady writer
`while true; do date; sleep 0.2; done` (5 lines/s) running throughout, so
this is not an idle session. Client stopped for 60 s, which crosses
`DETACH_AFTER` (30 s) and so measures the detached steady state as well.

| Measurement | Value |
|---|---|
| Mac `date +%T` at test start | 19:57:37 |
| `thinlinc` `date +%T` at test start | 19:57:37 |
| Host cost while ATTACHED, writer running (20 s) | 17 ticks = **170 ms = 0.85% of a core** |
| Host cost, client stopped, window 0-20 s (still attached, nothing acked) | 65 ticks = **650 ms = 3.25% of a core** |
| Host cost, window 20-40 s (`DETACH_AFTER` fires at 30 s) | 34 ticks = **340 ms = 1.7% of a core** |
| Host cost, window 40-60 s (**fully detached**) | **0 ticks = 0 ms = 0.0% of a core** |
| TIME delta over the whole 60 s stopped | 99 ticks = 990 ms |
| Screen correct on resume? | **YES, exactly.** 5 s after `SIGCONT` the pane's last line read `19:59:15` against a real clock of `19:59:14` |

**What this establishes.** The detached state costs *nothing measurable* —
zero ticks across a full 20 s window while the child was still writing five
lines a second. That is `DETACH_AFTER` doing exactly what it was added for,
and it confirms end to end the claim in its doc comment that "detaching
closes nothing": the host kept draining the pty, and on the client's return
`screen_stale` forced a snapshot that was correct to the second.

**The 17-20%-of-a-core figure is NOT reproduced at this load.** The attached
cost here is 0.85% of a core and the pre-detach unacked cost peaks at 3.25%.
Note what the numbers say about direction: a stopped client costs the host
*more* than an attached one (nothing is being acked, so diffs are recomputed
against an ageing base and retransmitted) right up until the detach, at which
point it costs nothing at all. The inherited 17-20% figure was flagged in the
handoff as "one afternoon on two machines, re-measure before building on it";
it has now been re-measured under a 5 lines/s writer and is much smaller.
Whether some heavier load reaches 17-20% is untested — this measures the load
the note specified, and nothing else.

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

**Ordering A was run three times on 2026-08-30 (21:30, 21:51, 21:58 CEST),
the user toggling the VPN by hand. Ordering B was NOT run** — see below for
why, and for why run 3 covers most of what it was for.

**How a rebind was observed.** `follow_route` binds a *fresh* socket
(`bind_socket` -> `adopt` -> `Link::rebind`), so a rebind is visible from
outside as a new UDP socket on the client pid. **Sample every socket, not the
first one**: the original `*:443` socket lingers on its own fd for the life of
the session, so a watcher reading only the first `lsof` row sees `*:443`
throughout and concludes — wrongly — that nothing ever rebound. That mistake
was made and caught during this run.

The probe itself was cross-checked with a stand-in doing exactly what
`roam::route_source` does (`socket(AF_INET, SOCK_DGRAM)`, `connect` to the
peer, `getsockname`). It agreed with the client's behaviour at every sample.

| Measurement | Ordering A (move first) | Ordering B (move during outage) |
|---|---|---|
| Mac `date +%T` at test start | 21:58:05 (run 3, the instrumented one) | NOT TAKEN |
| `thinlinc` `date +%T` at test start | clocks agreed to within the ssh round trip all afternoon | NOT TAKEN |
| Network change performed | VPN `utun65` down 21:58:10, back 21:58:27 (~17 s). Route fell to `en0`; probe reading moved `192.168.17.5` -> **`10.46.18.101`** and back | NOT TAKEN |
| Notice behaviour observed | Raised 21:58:14 (`silent for 8s`), cleared 21:58:33 — i.e. **~6 s after the path returned** | NOT TAKEN |
| Did the probe run? | **YES.** The move was visible to exactly the syscall pair the probe uses, and the client acted on it within one `ROUTE_PROBE_EVERY` | NOT TAKEN |
| Did a rebind fire? | **YES, twice — once per route move.** New socket `*:53884` at 21:58:14 (~4 s after the route moved), new socket `*:54968` at 21:58:27 (~2 s after it moved back) | NOT TAKEN |
| Did the session recover? | **YES.** Notice gone by 21:58:33, shell usable (`FINAL-21:59:00` round-tripped), 295 frames lost across the blip and the screen still correct | NOT TAKEN |
| Rebind load-bearing, or would a wildcard socket have followed anyway? | **Still not established — and this matters.** See below | NOT TAKEN |

### The seeded baseline is confirmed working

This is the review finding the seeding exists for, and it is now observed on a
real network. The route moved at 21:58:10; silence was not noticed until
21:58:14. So the **first probe of the outage already read the moved address**
(`10.46.18.101`) and had to compare it against a baseline taken while the link
was healthy. It did, it detected the move, and it rebound. Had the baseline
been taken on the first probe of the outage — the design the review
rejected — that probe would have adopted `10.46.18.101` as "normal" and no
rebind could ever have fired. **The "walked out of Wi-Fi range" case works.**

### What is still NOT established: whether the rebind was necessary

Every socket involved is **wildcard-bound** (`*:443`, `*:53884`, `*:54968`),
so the kernel re-sources a wildcard socket per outgoing packet regardless of
what oxutrm does. The session might well have recovered with no rebind at all.
What this run proves is that the mechanism **fires, promptly, correctly, and
does no harm** — not that it is load-bearing. The handoff's open question
("does a wildcard-bound socket even need the rebind?") is therefore **still
open**, and answering it needs a run with the rebind disabled, which is a
one-line experiment nobody has done.

There is a second reason this VPN is a weak instrument for the question: **it
restores the same tunnel address every time** (`192.168.17.5` before and
after, all three runs). So "the route moved" here means "it fell to `en0` and
came back", not "the peer is now reached from a new stable address". A real
address change — a different Wi-Fi network, a tether — remains untested.

### The long-outage run: recovery has a ceiling, and it is not in Tier A

Run 1 dropped the VPN for **seven minutes** (21:30:00 -> 21:37:09) and the
session **never came back**. At 21:39:21, more than two minutes after the path
was fully restored to its original address, the client still showed
`no reply from host` / `silent for 565s - sent 458 - lost 0`, and its send
rate had collapsed to roughly **one frame every two minutes**. The host was
alive the whole time (`ps` elapsed 10:00), still registered in
`oxutrm host --list`, simply detached.

**This is a gap, not a defect.** `REBUILD_AFTER` appears **nowhere in the
source** — `grep -rn REBUILD_AFTER --include=*.rs` returns nothing. It exists
only in the design spec, which says so itself: *"Until phase 3 builds
`REBUILD_AFTER` and §6's re-punch..."*. With no rebuild loop, a QUIC
connection that has backed off after a long outage has nothing to bring it
back, and the route probe cannot help because by then the route has returned
to where it started and there is nothing to detect.

So the practical envelope of Tier A, as shipped, measured:

| Outage | Result |
|---|---|
| Host frozen 20 s / 40 s / 100 s (`SIGSTOP`) | Recovers, notice clears within ~2 s |
| Client frozen 40 s / 60 s / 75 s (`SIGSTOP`) | Recovers, screen correct to the second |
| Path gone ~17-19 s (VPN off/on) | Recovers ~6-13 s after the path returns; rebind fires |
| Path gone ~7 min (VPN off/on) | **Never recovers.** Both ends alive, nothing reattaches |

### Why Ordering B was not run

Ordering B asks for the route to move *while already `Silent`*. Run 3 covers
the mechanism it was aimed at: `follow_route` only probes while `Silent`, so
**both** rebinds observed above necessarily happened inside a `Silent` phase —
the second one (21:58:27) is precisely "the route moved while the session was
already silent", which is Ordering B's definition. What Ordering B would add
beyond that is a different *starting* condition, not a different code path,
and it costs another hand toggle of the user's VPN. It is left NOT TAKEN
deliberately rather than silently.

### An observation to explain or dismiss

The original `*:443` socket (`fd 9`) is **never released**. It is still open
at the end of the session, after two rebinds, while the intermediate rebound
sockets (`*:51710`, `*:53884`) were properly closed as each was superseded.
Followed up in the code, and it is **not** a per-rebind leak. `Link::rebind`
does `self.socket = socket`, dropping the old `Arc`, and the trace bears that
out: each superseded socket went from two fds to one to none as quinn let go
of its in-flight state (`*:51710` and `*:53884` both drained away completely).
`ladder::adopt` does not dup either — it moves the std socket into tokio.

What is left is **one descriptor, from the connection-setup path, retained for
the life of the session**: the original socket started with two fds (9 and
16), rebind replaced the one quinn held, and fd 9 stayed open to the end. The
rebound sockets never acquire that second holder because they skip the ladder,
so the likeliest owner is something the ladder or the STUN demux keeps — note
`serve.rs` binds its equivalent as `_stun_rx`. Bounded and constant, not
growing. Worth a look when someone is next in `ladder.rs`, not worth a bug.

## Anomalies — seen once each, neither reproduced

Both of these happened in the first two sessions of the afternoon. Every
session after ~19:46 behaved perfectly across five deliberate repetitions.
They are recorded at full detail because a fault that appears twice in an
hour and then hides is exactly the kind this project has been burned by, and
because **neither is explained**. Do not read either as a known defect, and
do not read the non-reproduction as a fix.

### Anomaly A — a session that never came back after a 97 s host freeze

Run 1a stopped the host session for 97 s (19:43:38 -> 19:45:15). On
`SIGCONT` the notice did **not** clear. It was still up 8 s later
(`silent for 107s`) and still up at 19:46:03 (`silent for 148s - sent 379 -
lost 0`). Throughout, the host process was alive and running (`ps` state
`Sl`), the session was still listed by `oxutrm host --list`, and the client
was still heartbeating — `sent` crept 377 -> 379, so the client had not
given up. The client had to be closed with `Ctrl-\ q` (which worked
cleanly, `rc=0`).

**The obvious explanation is wrong.** "97 s exceeds `DETACH_AFTER` (30 s), so
the host detached and never took the client back" does not survive the
control runs: the same freeze was repeated at **20 s** (run 1c), **40 s**
(run C) and **100 s** (run D) on a later session, and all three recovered —
the box cleared within 2 s and the shell was usable immediately
(`POST-RUNC-20:01:47`, `POST-RUND-20:04:35` both round-tripped). Run D is
the same duration as the failure, on the same code, and it recovered.

Note also that reattach-after-detach demonstrably works in the *other*
direction: Test 2 froze the client for 60 s, the host's CPU going to exactly
zero proves it really did detach, and the screen was correct to the second on
resume.

### Anomaly B — the host process exited, silently

After Test 2's first run (session `bd7cb477`, host pid 2674519), the host
**process was gone**: `oxutrm host --list` reported `no live oxutrm sessions
on this host` and `ps -p 2674519` returned nothing. The client was still
alive and showed `no reply from host` / `silent for 28s - sent 1389 - lost
70` — **not** a session-ended message, so the host died without sending a
final frame. The screen had frozen at `19:49:11`, which is 19:48:42 + 29 s:
one second short of `DETACH_AFTER` from the client's stop.

`HostSession::run` returns only on the child exiting or on an error
propagating out of `turn()`; the child was an infinite
`while true; do date; sleep 0.2; done` loop, so an error path is the
likelier of the two. **Nothing was recoverable about the cause**, and that
is a finding in itself: `sever_from_ssh` points 0, 1 and 2 at `/dev/null`,
so a severed host that dies of an error writes its message nowhere. If this
is chased, the first move is a way to see a severed host's stderr.

**Not reproduced.** The identical sequence — client `SIGSTOP` for 40 s then
resume — was run twice more on a healthy session (runs A and B, the second
after first freezing the host for 20 s to match the failing run's history)
and once at 75 s, and the host survived all three with the screen correct
after each.

## Cleanup checklist

Done at 22:00 CEST on 2026-08-30:

- [x] Every stopped process `CONT`ed before it is `TERM`ed. (The
  counterfactual stays untested, as this note requires: `CONT` was always
  sent first, so whether `TERM` alone would have sufficed is still unknown.)
- [x] `oxutrm host --list` on `thinlinc` reports `no live oxutrm sessions on
  this host`, and `pgrep -a -f oxutrm` finds nothing at all.
- [x] `~/.local/bin/oxutrm` symlink unchanged, still
  `-> /scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm`, now
  resolving to a v0.2.0 binary.
- [x] tmux session `ox` killed; no tmux server left running.
- [x] `thinlinc`'s dirty checkout at `~/checkouts/oxutrm` left **exactly as
  found** — still at `39b2379` with its five modified and two untracked
  files. Nothing in this run touched it.

**Left behind on purpose:** the build worktree at
`/scratch/oetiker/oxutrm-handtest` (detached at `fd9f001`), so a rerun needs
no rebuild. `git -C checkouts/oxutrm worktree remove
/scratch/oetiker/oxutrm-handtest` when it is no longer wanted.

**Five sessions were created and all five were cleaned up.** The note's older
warning still holds and was confirmed again: closing the client with
`Ctrl-\ q` leaves the host session registered and `detachable`, and since
reattach is Tier B, nothing can ever pick it up — it must be killed by PID.
Budget that into every hand test.

## What this note feeds

Three open questions this phase inherits were waiting on exactly this note.
Where they stand after the run of 2026-08-30:

- **`SILENT_AFTER` = 2 s — good, keep it.** The notice consistently appeared
  a few seconds into a real outage and never once appeared spuriously across
  an afternoon of sessions, including over a VPN. Note the raise is not at
  2 s wall-clock: silence is owed from the first *unanswered* send, so with
  `HEARTBEAT_IDLE` at 5 s an idle session's box shows up ~5-8 s after the
  path dies. That felt right rather than slow.
- **`REBUILD_AFTER` = 20 s — untestable as written, because it does not
  exist.** It is in the spec and in no `.rs` file. The 7-minute outage above
  is what its absence costs; that measurement is the argument for building
  it, and Tier B is where.
- **Does the route probe work under a VPN? YES, observed directly** — the
  probe read `192.168.17.5` -> `10.46.18.101` -> `192.168.17.5` across a real
  VPN toggle and rebound on each move. With the caveat recorded above: this
  VPN restores the *same* tunnel address, so a genuine new-address case is
  still untested.

And one question this note did not set out to answer but now can:

- **Does a wildcard socket need the rebind at all? Still open**, and now with
  a cheap way to settle it: disable the rebind and rerun the VPN blip. Every
  socket involved is wildcard-bound, so the kernel may have been re-sourcing
  them for free the whole time.

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

A second, **short and non-`#[ignore]`d** sibling,
`session::tests::a_short_silence_raises_and_clears_the_notice_on_a_real_clock`,
covers the same notice-raise/notice-clear round trip in ~9s of real time on
every default `cargo test` run, so CI is not entirely without real-clock
coverage of this behaviour between hand tests. It starts from a genuinely
synced session and lets `HEARTBEAT_IDLE` (5s) and `SILENT_AFTER` (2s) fire
for real, back to back, rather than shortening either threshold. Same
caveat as the 35s test: it is a session-layer guard, not a transport-level
one. It does not touch what Test 1 above or the config-wiring unit test
(`the_server_config_actually_wires_no_idle_timeout` in
`crates/oxutrm-net/src/quic.rs`, added in review to catch a dropped
`cfg.transport_config(...)` call) cover.

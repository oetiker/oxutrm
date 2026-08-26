<!--
RESCUED FROM A SESSION SCRATCHPAD, 2026-08-26. It was written on scratch, which
is garbage-collected, and it was nearly lost. The body below is verbatim as
written on 2026-08-25 against `main` at fc305c9 — do not edit it to reflect
later decisions; read the correction here instead, and read the contract, which
is normative where this document and it disagree.

**§5 STEP 1 IS INVALIDATED. DO NOT BUILD IT.**

Step 1 says "A real `RungRunner`". That trait has since been DELETED
(`733288b`), and it must not come back in that shape. It could not have carried
a real rung, for three reasons now recorded normatively in the contract beside
the rung-4 framing rules:

  1. The race belongs to the candidate PAIRS, not to the rungs. Rungs 0-2 are
     three candidate classes on ONE socket, and `IceAgent` already races every
     pair on it and reports which rung won. One future per rung means concurrent
     receive loops on one socket stealing each other's datagrams — the failure
     `StunDemuxSocket` exists to prevent. NAT mappings are per-socket.
  2. A nomination must hand back the SOCKET, not just an address.
     `birthday_blast` returns `BirthdayResult { socket, .. }` precisely because
     the mapping belongs to that socket and QUIC must adopt that exact one.
  3. The MTU is not knowable at nomination — it is a `quinn` property discovered
     after the handshake, which is strictly later.

And the crate-graph question this document leaves open is now DECIDED:
**`oxutrm-host` MUST NOT depend on `oxutrm-net`.** The ladder driver goes in the
root binary, which already depends on both. `ladder` keeps the policy
(`LadderPlan`) and nothing else.

Everything else in §5's build order is sound. Step 0 (split `daemonize`) is
DONE. Steps 2 and 3 stand as written.
-->

# Wiring `oxutrm <ssh-target>` — a design

Read-only investigation, 2026-08-25, against `main` at `fc305c9`. No file in the
repository was modified.

---

## 1. Does `keys.rs` already resolve the fork/runtime conflict?

**No. It enforces the half of the conflict that makes the problem hard.**

`crates/oxutrm-host/src/keys.rs:149-172` introduces `DetachPermit`, a token with
a private field that cannot be constructed anywhere except `settle_detachability`:

```rust
pub struct DetachPermit {
    _private: (),
}

pub fn settle_detachability(
    meta: &mut SessionMeta,
    rung: oxutrm_proto::Rung,
) -> Option<DetachPermit> {
    if meta.set_detachable(rung) {
        Some(DetachPermit { _private: () })
    } else {
        None
    }
}
```

and `crates/oxutrm-host/src/daemon.rs:157` demands one:

```rust
pub fn daemonize_session(_permit: crate::DetachPermit) -> anyhow::Result<()> {
    daemonize()
}
```

Its own doc comment states the intent plainly (`keys.rs:141-143`):

> That is the ordering made structural rather than remembered: there is no way to
> write a call that daemonizes before the rung is known, because there is nothing
> to pass.

So `keys.rs` guarantees **rung-before-daemonize** at compile time. It says nothing
whatsoever about **threads-before-fork**, which is rule 2 in `daemon.rs:27-30` and
survives only as a comment:

> 2. **Call it before any thread exists**, a tokio runtime included. `fork` copies
>    only the calling thread, so a runtime built beforehand wakes up in the child
>    with its worker threads gone and deadlocks.

The two rules are jointly unsatisfiable in the shape the M3 plan assumed, and
`keys.rs` is what makes one of them un-negotiable. It does, however, contain the
*hint* to the resolution: the permit gates `daemonize_session`, not `daemonize`,
and the reason given for the permit is specifically that a rung-4 session must not
**close the ssh descriptors**. That is a statement about descriptor closure, not
about forking. Once you notice that those are two different operations welded into
one function, the resolution follows.

Recording this because the brief asked: the previous reviewer's suspicion was
reasonable but wrong in direction. `keys.rs` tightens the conflict rather than
relieving it.

---

## 2. The resolution: fork first, sever later

### The conflict, stated exactly

Three facts collide.

1. `HostHello` carries `candidates`, `nat_type` and `bound_port`
   (`crates/oxutrm-proto/src/signal.rs:17-44`). Candidates come from
   `stun_discover`, which is `async` and takes a `&tokio::net::UdpSocket`
   (`crates/oxutrm-net/src/discover.rs:132`). **So async work is required before
   the first byte of signalling, not merely before nomination.**
2. Nomination requires `IceAgent::run(&mut self, socket: Arc<tokio::net::UdpSocket>)`
   (`crates/oxutrm-net/src/ice.rs:160`), and QUIC cannot start before it
   (`quic.rs:8-11`, `ice.rs:10-15`).
3. `daemonize()` must fork before any thread exists (`daemon.rs:27-30`), and must
   not run until the rung is known (`daemon.rs:31-35`, enforced by `DetachPermit`).

### The resolution

**Split `daemonize()` into its two independent operations and put the async phase
between them.**

- **Phase 1 — `detach_process()`**: `fork`, `setsid`, `fork`, `umask`. Runs as the
  *first statement* of `oxutrm host --serve`, before a socket is bound, before a
  runtime is built, before anything has had the chance to create a thread.
  Descriptors 0/1/2 — the ssh pipes — cross the fork untouched, which is exactly
  what signalling needs. Requires no permit: forking is harmless for every rung.
- **Phase 2 — `sever_from_ssh(permit: DetachPermit)`**: `chdir("/")`,
  `close_inherited_descriptors()`, `reopen_standard_descriptors()`. Runs after
  `Established` is flushed and `settle_detachability` has returned a permit. A
  rung-4 session never obtains a permit and therefore never severs — it keeps the
  ssh pipes for its whole life, which is precisely what rung 4 requires.

`daemonize()` stays as the composition of both, so `tests/daemonize.rs` and
`src/bin/oxutrm-daemon-probe.rs` keep working unchanged. `daemonize_session` keeps
its signature and becomes `sever_from_ssh`'s alias or is retired in favour of it.

The `DetachPermit` guarantee is not weakened — it is **sharpened**. Today the
permit gates "fork and close descriptors"; afterwards it gates only "close
descriptors", which is the operation its own documentation says it exists to
prevent for rung 4 (`keys.rs:144-148`).

### Why this is right, mechanism by mechanism

| Rule (`daemon.rs`) | Under the split |
|---|---|
| 1. intermediates `_exit`, never unwind | Unchanged. Better, in fact: the `RegistryGuard` is now created *after* the fork, so the intermediates have nothing to destroy. |
| 2. fork before any thread exists | **Structurally unbreakable.** The fork is the first thing the process does. There is no code before it that could create a thread. |
| 3. fork after `HostHello` is flushed | **Dissolved.** The rule existed only because forking and closing pipes were the same call. Phase 2 still runs after `Established`. |
| 4. only when detachability is settled | Unchanged, and still enforced by `DetachPermit`, now attached to the operation it describes. |

Two properties fall out for free, and both are improvements over the current
design:

- **Errors after the fork are still reportable.** In the M3 shape, anything that
  failed after `daemonize()` had nowhere to go — `daemon.rs:121-122` and the M3
  plan both say so. Under the split, a ladder failure, a registry-root warning, or
  a `Signal::Failed { reason }` all still reach the user over the live ssh pipes.
  The `RegistryRoot::warning` that `registry.rs:345-346` says must be "printed to
  stderr once, before daemonizing, where it can still be seen" now genuinely can be.
- **`refresh_pid` is trivially correct.** The registry entry is written by the
  grandchild, so the pid `--list` prunes on is right by construction rather than by
  a rewrite after the fact.

### The mechanism the split relies on, named so nobody "fixes" it

The grandchild holds the ssh pipes open. `sshd` sends `exit-status` when the
process it spawned exits — which the fork parent does immediately — but does not
close the channel until stdout and stderr reach EOF. Because the grandchild still
holds them, the local `ssh` stays alive for the whole handshake and ladder, which
is what bidirectional `CandidateUpdate` exchange requires. When the grandchild
severs, EOF arrives and `ssh` exits.

This is the same phenomenon as the classic "my daemon made ssh hang" bug, used
deliberately. It must be written down at the call site or someone will one day
"fix" it by closing stdout early, and rungs 1–3 will stop being able to exchange
candidates.

One consequence on the local side: `SshChannel::recv` treats EOF as a failure and
calls `diagnose()` (`ssh.rs:246-254`), which would report `NoSignal`. **The client
must stop reading the ssh channel once it has seen `Established`** and let
`kill_on_drop` reap it. That is a change to the caller, not to `ssh.rs`.

### The rejected alternatives, with their concrete failure modes

**(a) Carry the socket across the fork as a raw fd, build a fresh runtime in the
grandchild.**

Mechanically it can be made to work — `bind_socket` already returns a plain
`std::net::UdpSocket` (`socketfam.rs:35`), and `dup(2)` on the fd before dropping
the runtime gives a fd with no epoll registration of its own. Two things kill it:

- *The proof obligation is unauditable.* Rule 2 does not say "no tokio worker
  threads"; it says **no thread**. `Runtime::drop` joins the worker threads but
  does not join detached blocking-pool threads, and this process will have run
  `stun_discover` (via `stunclient`), `PortMapping::acquire` (via `crab_nat` and
  `igd-next`), and possibly `loginctl`. To fork soundly you would have to prove
  that no dependency, at any version, left a thread behind. The failure mode is a
  grandchild that deadlocks holding a lock a vanished thread was in the middle of
  — intermittent, unreproducible, and it surfaces only in a detached session on
  someone else's machine.
- *It forces a keep-list into the highest-risk function.*
  `close_inherited_descriptors` (`daemon.rs:92-116`) closes every fd above 2 by
  enumeration, and that indiscriminacy is the entire value of
  `tests/daemonize.rs`. Adding "except these" is precisely the seam through which
  a descriptor survives. Commit `6152a29` records that skipping this function was
  one of the three injected faults the test caught; weakening it to solve an
  ordering problem is a bad trade.

**(b) Run the ladder in a separate process.**

The socket would come back over `SCM_RIGHTS`, so the NAT mapping does survive
(same open file description). But: a second binary role, an IPC protocol, the PSK
crossing a socketpair, and a new failure mode — the helper dying mid-ladder,
leaving a session with a punched socket and nobody to nominate — all to solve an
ordering problem that the split solves with no new process, no new protocol and
no new failure mode. Reject.

**(c) is what fits the code that already exists.** It is also the only one of the
three that makes rule 2 impossible to break rather than merely documented.

### Rung 3 changes the socket — state it now

`birthday_blast` returns `BirthdayResult { socket: std::net::UdpSocket, .. }`
(`birthday.rs:36-41`), with the comment "QUIC takes this one over, because the
mapping belongs to this socket and no other". So the crate's "one socket from the
first probe to the last datagram" invariant (`net/src/lib.rs:11-16`) has an
explicit exception at rung 3, and the wiring must treat "the socket QUIC gets" as
an output of the ladder, not as a variable captured before it ran.

---

## 3. End to end: `oxutrm <ssh-target>`

Two processes, two runtimes, one fork. `fn main()` stays **synchronous** — each
subcommand builds its own runtime, because `host --serve` must fork before one
exists.

### Local: `oxutrm <target>` (`run_connect`)

| # | Step | Notes |
|---|---|---|
| L1 | Parse args. Terminal **not** in raw mode. | ssh may need to prompt for a passphrase; raw mode would corrupt it. |
| L2 | Build the tokio runtime. | The local side never forks. |
| L3 | `SshChannel::open(&SshLauncher::ssh(), target)` | `ssh.rs:182`. Spawns `ssh <target> oxutrm host --serve`, drains stderr continuously. |
| L4 | `bind_socket(&cfg)` → `tokio::net::UdpSocket::from_std` → `Arc` | One socket for STUN, ICE and QUIC. |
| L5 | `ch.recv()` → `HostHello { psk, cert_spki_sha256, candidates, nat_type, bound_port, attach_id, .. }` | Skips banner/motd; version skew fails loudly. |
| L6 | `local_candidates(&sock)` + `stun_discover(&sock, &cfg)` | Our candidates and our NAT type. |
| L7 | `ch.send(ClientHello { caps: detect_caps(), size: terminal_size()?, .. })` | |
| L8 | Ladder, `IceRole::Controlling`: `LadderPlan::for_nat(nat)` → `nominate(&plan, nat, runner)` | New `CandidateUpdate`s from either side are fed into the agent as they arrive. |
| L9 | On `IceEvent::Nominated`: `quic_client(&sock, remote, cert_spki_sha256)` | Migration is local-address only; the remote address is fixed here for the whole attach. |
| L10 | `ch.recv()` → `Established`. **Stop reading the ssh channel.** | The host is about to sever; further EOF is expected, not an error. |
| L11 | `RawGuard::enter()` | Late, deliberately: after every prompt ssh could have shown. |
| L12 | `client.announce(&path, &mut stdout)` | One line, then silence (`session.rs:290`). |
| L13 | `ClientSession::run(&mut stdout).await` | §4. |
| L14 | Shell exits or the link closes → drop `RawGuard` → print the exit line → `exit(code)` | |

### Remote: `oxutrm host --serve` (`run_host_serve`)

| # | Step | Process | Notes |
|---|---|---|---|
| R1 | **`detach_process()`** — fork, setsid, fork, umask | ssh's child → grandchild | **Nothing has run before this.** No socket, no runtime, no thread. The parent `_exit(0)`s, so ssh reports the command finished; the channel stays open because the grandchild holds 0/1/2. |
| R2 | `resolve_registry_root()`; print `root.warning` to stderr | grandchild | Reaches the user, because stderr is still the ssh pipe. |
| R3 | Build the tokio runtime | grandchild | Everything from here is async. Threads are now allowed: there will be no further fork. |
| R4 | `generate_cert()` → `(cert, key, spki_sha256)`; `begin_attach(&mut meta, spki_sha256)` | grandchild | `keys.rs:128`. Fresh PSK and fresh certificate per attach; `attach_id` bumped. |
| R5 | `bind_socket` → tokio → `Arc`; `local_candidates` + `stun_discover` | grandchild | |
| R6 | Write `HostHello` to stdout and **flush** | grandchild | `detachable: true` here is intent, never outcome (`signal.rs:34-43`). |
| R7 | Read `ClientHello` from stdin | grandchild | `size` and `caps` land here; `caps` is recorded and **never** used to pick `TERM`. |
| R8 | Ladder, `IceRole::Controlled` | grandchild | The controlling side nominates; this side is told (`ice.rs:287-295`). |
| R9 | `quic_server(&sock, cert, key)` → endpoint; `endpoint.accept()` | grandchild | Endpoint up *before* `Established`, so the client's Initial is never sent into a void. |
| R10 | Write `Established { path }` and flush | grandchild | The last signalling message. |
| R11 | `settle_detachability(&mut meta, path.rung)` → `Option<DetachPermit>` | grandchild | `None` for rung 4. |
| R12 | `Some(permit)` → **`sever_from_ssh(permit)`**: chdir, close every fd > 2, reopen 0/1/2 on `/dev/null` | grandchild | ssh sees EOF and exits; the user's prompt returns. `None` → skip; a rung-4 session keeps its pipes and its ssh for life. |
| R13 | `RegistryGuard::register_in(root, &meta)`; bind the session Unix socket | grandchild | **After** R12, or the socket is closed a moment later (`daemon.rs:37-38`). |
| R14 | `HostSession::spawn(shell, size, scrollback, Link::new(conn, endpoint, sock))` | grandchild | `negotiate_term()` takes no arguments: the child's `TERM` comes from the emulator, never from the client. |
| R15 | `HostSession::run().await`, concurrently with an accept loop on the Unix socket for `--attach` | grandchild | |
| R16 | Shell exits → drop `RegistryGuard` → the directory goes with it | grandchild | |

### Answers to the specific questions

- **Where the fork happens**: R1, the first statement of `host --serve`, before a
  socket, a runtime or a thread exists.
- **What crosses it**: nothing but the inherited descriptors 0/1/2 (ssh's pipes),
  the environment, and the argv. No socket, no key, no runtime, no fd juggling.
- **When the runtime is created**: R3, after the fork, in the grandchild. It is
  never destroyed — the grandchild runs it for the session's life. On the local
  side, once at L2.
- **When ICE nominates**: R8/L8, before QUIC, after both hellos.
- **When QUIC starts**: R9/L9, immediately after nomination and before
  `Established`.
- **When raw mode is entered**: L11, on the local side only, after ssh has finished
  with the terminal and after nomination succeeded — so a failed connect leaves a
  cooked terminal and a readable error.

---

## 4. `ClientSession::run()`, and the 250 Hz spin

### What `run()` should be

`ClientSession::turn(&[u8], &mut W)` (`session.rs:320`) is correct and well
tested; it is the *waiting* that is missing. `run()` should be an event loop that
wakes on one of five things and calls the existing `turn`:

```rust
pub async fn run<W: Write>(&mut self, out: &mut W) -> Result<i32> {
    let stdin = AsyncFd::new(dup_nonblocking(rustix::stdio::stdin())?)?;
    let mut winch = tokio::signal::unix::signal(SignalKind::window_change())?;
    let mut buf = [0u8; 8192];
    loop {
        tokio::select! {
            g = stdin.readable() => { /* read into buf; turn(&buf[..n], out)? */ }
            Some(f) = self.link.source.recv() => { self.link.source.push_back(f);
                                                   self.turn(&[], out)?; }
            _ = winch.recv() => self.resize(terminal_size()?),
            () = tokio::time::sleep_until(self.next_due()) => { self.turn(&[], out)?; }
            () = self.link.sink.connection().closed() => return Ok(...),
        }
    }
}
```

Three notes on the mechanics:

- `FrameSource` needs one line of new state: a single-slot `push_back` that
  `try_recv` checks first (`link.rs:257`). That is what lets the select arm take a
  frame off the channel to *learn readiness* and still let `turn`'s existing
  `while let Some(frame) = try_recv()` drain loop consume it. `FrameSource::recv`
  (`link.rs:252`), currently callerless, becomes the readiness source.
- `AsyncFd` over stdin rather than `tokio::io::stdin()`: a tty is pollable, and
  `tokio::io::stdin()` parks a blocking-pool thread in an uncancellable `read`.
  `main.rs:150` already sets stdin non-blocking, which `AsyncFd` requires.
- SIGWINCH replaces the poll in `main.rs:158-160`. `tokio::signal` needs no
  `unsafe`, which the current comment gives as the reason for polling.

The **pacing deadline arm is load-bearing, not an optimisation**. A frame is the
only thing that carries an `ack`, so a client that is only watching output must
still send. Commit `bbec42b` and the test
`a_client_that_stops_typing_keeps_receiving_output` (`session.rs:509`) exist
because that was got wrong once. `next_due()` must be derived from `last_send` and
`link.sink.pacing_interval()`, i.e. the same predicate as `due()`
(`session.rs:375`).

### Event-driven now, or later?

**Now — and do the host at the same time.**

*Cost of doing it now*: roughly 150 lines on the client, ~15 on the host, one new
`push_back` slot, and one new method on `HostTerm` (`impl AsFd`, so
`HostSession::run` can `AsyncFd` the PTY controller). The PTY is already
non-blocking (`pty.rs:93`), and the M4 plan already lists `AsFd for HostTerm` as a
planned modification. Nothing existing has to change: `turn()` keeps its signature,
so every test in `session.rs` stays valid.

*Cost of doing it later*: `HostSession::run` (`session.rs:230-238`) sleeps 4 ms in
a loop, forever. A **detached** session — the state this whole project exists to
make cheap — would wake 250 times a second on a shared server, computing a diff
against a screen nobody is watching. `lib.rs:11-13` promises the opposite:

> A session that nobody is watching keeps draining its PTY, transmits nothing, and
> costs no bandwidth for as long as it is left alone.

That claim is true about bandwidth and false about CPU, and a poll loop is what
makes it false. Retrofitting is also strictly harder once `run()` has callers and
tests written against poll semantics.

The one thing that must *stay* polled: nothing. The PTY, the socket, the keyboard,
the window size and the pacing timer are all pollable or timer-driven.

---

## 5. Build order

Each step leaves `cargo clippy --all-targets -- -D warnings` and
`cargo test --jobs 4 -- --test-threads 4` green.

### Step 0 — Split `daemonize` (small, and it de-risks everything after it)

Add `detach_process()` and `sever_from_ssh(DetachPermit)`; keep `daemonize()` as
their composition.

Extend `src/bin/oxutrm-daemon-probe.rs` with a second mode: hold the leaked
descriptors, `detach_process()`, **write a marker line on an inherited descriptor
and to the parent's stdout**, then `sever_from_ssh`, then write the report.

*What the test proves*: that the inherited descriptors genuinely survive phase 1
(so signalling across the fork is possible at all) and genuinely die at phase 2
(so the detach is still complete). Both halves, observed from outside, in one
process. This is the resolution in §2 turned into an assertion before a line of
CLI wiring depends on it.

### Step 1 — A real `RungRunner`

**There is no real one today.** `grep -rn 'impl RungRunner'` finds exactly one hit:
`Scripted` in `crates/oxutrm-host/tests/ladder.rs:47`. `ladder.rs` explicitly does
not implement any rung. So "the connectivity ladder exists and is tested" is true
of the *decision logic* and false of the *mechanism that feeds it*. This is the
single largest piece of missing code under the CLI, and it must come first.

There is an architectural mismatch to resolve here, and it is not cosmetic:
`nominate()` races `plan.raced` as three independent `attempt` calls
(`ladder.rs:185-204`), but rungs 0, 1 and 2 all drive **one** `IceAgent` on **one**
socket. Three concurrent `IceAgent::run` calls on one socket would steal each
other's datagrams — the same class of bug `StunDemuxSocket` exists to prevent.

Two ways out; I recommend the second:

- *Reshape `ladder.rs`* so the raced group is one attempt taking a set of rungs.
  Cleaner, but it changes a tested API and its tests.
- *Keep `ladder.rs` untouched.* `LadderRunner` holds the shared agent behind a
  `OnceCell<IceOutcome>`. Each raced `attempt(rung)` contributes its candidate
  class, then awaits the one shared run, then returns `Nominated` only if
  `IceAgent`'s own rung classification (`ice.rs:340-353`) matches its own rung, and
  `Failed`/`Skipped` otherwise. `nominate`'s `abort_all()` on the winner then does
  the right thing. More moving parts inside the runner, zero API churn outside it.

*What the test proves*: a loopback test that two real `LadderRunner`s nominate each
other, plus a netns case that the real runner (not `Scripted`) traverses the
existing cone-NAT topology. That is the first time the ladder's policy and the
ladder's mechanism are proven connected.

### Step 2 — `host --serve`, without the fork

Wire R2–R16 with R1 and R12 omitted. Equivalent to the M3 plan's `--no-detach`.

*What the test proves*: `tests/cli.rs` spawns `host --serve` with piped stdio,
plays the client half of the handshake, and asserts the session appears in
`--list` and its screen advances. The whole signalling → ladder → QUIC → session
chain, in one process, with no fork anywhere near it. If this is red, the fork is
not the reason.

### Step 3 — Add the fork

Insert R1 and R12. Nothing else changes.

*What the test proves*: the same cli test, extended — the `--serve` process exits
0 while the session keeps running; the Unix socket appears; `--list` shows it; and
`/proc/<session-pid>/fd` contains only 0, 1, 2, all pointing at `/dev/null`, plus
the socket and the PTY. This is `tests/daemonize.rs`'s claim re-proved on the real
binary rather than on a probe.

### Step 4 — `oxutrm <target>`

Wire L1–L14, including `ClientSession::run` and the host's event loop (§4).

*What the test proves*: an end-to-end test that runs `oxutrm <target>` under a real
PTY with `SshLauncher::command(oxutrm-fake-ssh)`, types into it, and asserts the
*rendered bytes* contain the shell's answer. Not "the two `ScreenState`s match" —
the sync tests already prove that — but what a person sees.

### Step 5 — `host --attach <id>`

`connect_to_session` and `relay_signals` already exist (`attach.rs:75`, `:110`).
What is missing is the session-side accept loop that treats an inbound relay as a
new attach: `begin_attach` (fresh keys, bumped `attach_id`), reset both `Sender`
and `Receiver`, re-run ICE, new QUIC connection.

*What the test proves*: attach, detach, reattach; `attach_id` incremented on disk;
the second PSK differs from the first; the screen comes back intact. And that
`--attach` refuses a session recorded `detachable: false` with the message in
`AttachError::NotDetachable`.

### Step 6 — Rung 4

`Path::tunnel()`, `frame_tunnel_message` and `read_tunnel_message` exist
(`transport.rs`); the relay that carries QUIC inside the ssh channel does not
(`crates/oxutrm-net/src/tunnel.rs` is planned in M4 and absent).

*What the test proves*: with UDP blocked in a netns, the ladder falls to rung 4,
`settle_detachability` returns `None`, the session **does not** sever, `--list`
shows `NOT detachable`, `--attach` refuses it, and killing the ssh kills the
session. That last assertion is the one that catches a session that severed on
intent.

---

## 6. What can and cannot be tested without two real machines

### Can, and mostly already is

- The whole CLI chain, using `ssh localhost` or `oxutrm-fake-ssh`. `ssh localhost`
  is a genuine proof of detachability: the session outliving that connection is the
  real property, not a simulation of it.
- Descriptor closure, via `/proc/<pid>/fd`, from outside the process.
- NAT traversal against cone, symmetric and nested double NAT, with real
  `nftables`, via `crates/oxutrm-net/tests/netns.rs`.
- Local-address roaming (127.0.0.1 → 127.0.0.2), already asserted in
  `the_session_survives_the_client_changing_its_own_local_address`.
- Frame convergence, out-of-order streams, rejection-is-not-disconnection,
  input-exactly-once.

### Cannot

- **Rung 1 has, as far as I can tell, never successfully executed anywhere.**
  `PortMapping::acquire` needs a router speaking NAT-PMP, PCP or UPnP-IGD; there is
  none in the netns topologies. It is the rung most likely to be silently broken.
- **Real STUN servers.** `stun_discover` is exercised against the local
  `StunResponder`. DNS resolution of the configured servers, a server that is down,
  a server reachable only over IPv6, and a server inside the same NAT are all
  untested paths through `discover.rs`.
- **Real NAT device behaviour**: mapping lifetimes, hairpinning, CGNAT, an
  enterprise firewall that drops UDP/443 outright, or a carrier that rate-limits a
  65k-packet birthday blast. netns proves the *algorithm* against a NAT; it cannot
  predict a Fritz!Box.
- **Path MTU.** Loopback MTU is 65535, so essentially every frame fits a datagram
  and the stream path is reached only by the deliberately oversized 200x60 test
  (`session.rs:742`). On a real ~1200-byte path the datagram/stream split happens
  constantly, and the supersede-and-`RESET_STREAM` logic runs orders of magnitude
  more often than any test drives it.
- **Loss, reordering and real RTT.** Pacing is `clamp(rtt/2, 8ms, 100ms)`
  (`link.rs:180`); on loopback `rtt≈0`, so **only the 8 ms floor has ever run**.
  The 100 ms ceiling and everything between are untested.
- **A real roam across networks.** The existing test changes the client's local IP
  while the peer stays reachable at the same address. A Wi-Fi→LTE roam also
  destroys the punched hole, and the design deliberately does not re-nominate
  within an attach. Whether such a session recovers, hangs, or dies cleanly is
  unknown, and the README should not claim it either way.
- **Rung 0 between two hosts with global IPv6 addresses.**
- **Both ends behind NAT simultaneously** — commit `3863f7e` already flags this:
  no topology NATs both ends at once, and the symmetry argument is reasoning, not
  evidence.

The honest summary for a README: *loopback and network namespaces prove the
mechanisms; only two machines on two real networks prove the product.*

---

## 7. Open questions, risks, and what I did not read

### Questions I could not answer by reading

1. **Who sends `Established`, and does the client need the host's copy?** The
   client is `Controlling` and nominates first; the host learns via the nomination
   indication and settles its own rung from its own agent. So the host does not
   need a client `Established`, and the client already has its own
   `PathDescription` for `announce`. I designed the host as the sender (R10),
   used by the client purely as "the pipes are about to close" — but `Signal`
   documents it as "either direction" and the two ends' `rtt_ms` will differ.
   Someone should decide which copy is canonical for a bug report.
2. **`announce` writes `\n`, not `\r\n`.** `session.rs:308` uses `writeln!`, and it
   is only ever tested against a `Vec<u8>` (`session.rs:902`). In raw mode with
   `ONLCR` off that is a line feed with no carriage return. The following
   `renderer.invalidate()` and full repaint probably hides it, but it has never
   been looked at on a real terminal.
3. **The shell's working directory.** `HostTerm::spawn` takes no cwd, and both the
   current design and mine `chdir("/")` before spawning it. An ssh session that
   starts in `/` rather than `$HOME` is a visible behaviour difference from mosh
   and from ssh. Pre-existing, not introduced by the split, but it should be fixed
   by passing `$HOME` to `HostTerm::spawn`.
4. **Does `Arc<tokio::net::UdpSocket>` refcount reach 1 after the ladder?** It
   matters for nothing in my design (the socket is never converted back), but it
   would matter to alternative (a), and I did not verify what `LadderRunner`'s
   aborted `JoinSet` tasks leave behind.
5. **Idle timeout.** `SessionConfig::idle_timeout` is planned in M3 and does not
   exist in the built code. `host --serve --idle-timeout` appears in the M3 test
   fixtures. Whether the CLI should accept it now is a scope question for the lead.
6. **`ScrollbackReq` and `ControlMsg` are defined and handled nowhere** — commit
   `3863f7e` says so. Nothing in this design needs them; noting it so "wire the
   CLI" is not read as including them.

### Divergences between the plans and the built code (relevant to anyone reading the plans as specification)

- The M4 plan's File Structure places the session loops in
  `crates/oxutrm-host/src/session.rs` and `crates/oxutrm-client/src/session.rs`,
  and the framing in `crates/oxutrm-net/src/xport.rs`. **They shipped in
  `src/session.rs` and `src/link.rs` at the root.** The plan's file table is stale.
- The M3 plan's `Session` type, `ShellHandle`, `StubShell`, `SessionEnd`,
  `serve_attach`, `run_session` and `relay_attach` (Task 12/13) **do not exist**.
  `crates/oxutrm-host/src/` has no `session.rs`. Whatever holds session lifecycle
  state for `--serve` has to be written from scratch, and the design above assumes
  a much thinner version of it.
- The M3 plan's ordering rules for `host --serve` (Task 16) contain the exact
  contradiction this document resolves: rule 1 says handshake with blocking I/O
  before any runtime exists, rule 5 says build the runtime last, and rule 2 says
  daemonize after `Established` — which cannot be produced without a runtime. The
  plan hid it behind the fact that M3 stubbed the ladder ("M4 inserts the ICE
  ladder between the hellos and that message"). It surfaces the moment the stub is
  replaced, which is now.
- `RungRunner` has no production implementation (§5 Step 1).
- Rung 4's relay has no implementation.
- `crates/oxutrm-net/src/pace.rs`, `link.rs`, `tunnel.rs`, `testkit.rs`,
  `input_cursor.rs`, `input_queue.rs`, `pane.rs`, `src/help.rs` — all planned in
  M4, none present. `TerminalGuard` is still `RawGuard`.

### What I did not read

Stated because a review that implies completeness is worth less than one that
names its gaps.

- **`crates/oxutrm-term/`**: read only `pty.rs` and `host.rs` signatures. Did not
  read `blink.rs`, `golden.rs`, `palette.rs`, `grid.rs`, `listener.rs`, `caps.rs`,
  or the bodies of `host.rs`. I cannot speak to emulation fidelity, to whether
  `HostTerm::poll` can block, or to what `detect_caps` actually inspects.
- **`crates/oxutrm-sync/`**: read none of `channel.rs`, `screen.rs`, `input.rs`. I
  relied on the contract and on commits `3bad4d3`/`bbec42b` for the seq-1 and
  ack-owed rules.
- **`crates/oxutrm-client/`**: read `lib.rs` and the first 60 lines of `guard.rs`.
  Did not read `renderer.rs` (811 lines), `color.rs`, or `status.rs`. I assumed
  `Renderer::render` is synchronous and never blocks; that assumption underpins the
  event loop in §4 and I did not verify it.
- **`crates/oxutrm-net/`**: read `ice.rs`, `quic.rs`, `lib.rs`, `birthday.rs`'s
  header and `BirthdayResult`, `bind_socket`, and the signatures of
  `stun_discover`/`local_candidates`/`PortMapping::acquire`. Did not read
  `demux.rs`, `demuxsock.rs`, `der.rs`, `mapping.rs`, `socketfam.rs`,
  `stunmsg.rs`, `stunserver.rs`, `tls.rs`, `candidates.rs`, or the body of
  `discover.rs`.
- **`src/loopback.rs`** (591 lines): not read. Only its use from `main.rs`.
- **Every test file body**: I read file names and, for `session.rs`, its inline
  test module. I did **not** read `crates/oxutrm-host/tests/daemonize.rs`,
  `attach.rs`, `ssh_bootstrap.rs`, `ladder.rs`, `transport.rs`, or
  `crates/oxutrm-net/tests/netns.rs` and its `topology.sh`. My claims about what
  netns proves come from its module header and from commit messages, not from the
  assertions.
- **The design spec** `docs/superpowers/specs/2026-08-25-oxutrm-design.md`: not
  opened. Everything I cite as normative comes from the contract, which the spec
  outranks in the places they disagree — and the M4 plan lists three such places.
- **The plans**: read ~400 of 5,860 lines of M3 and ~250 of 5,500 of M4. Read the
  contract in full. Did not open M1, M2 or the vt100 patch.
- **`git log` bodies**: read two (`3863f7e`, `6152a29`) plus the subject lines.
- **I ran no build and no test**, as instructed. Every claim about what compiles or
  passes is inference from reading, and the API-shape claims in §4 and §5 —
  particularly `AsyncFd` over a dup'd stdin, and the `push_back` slot on
  `FrameSource` — have not been compiled.

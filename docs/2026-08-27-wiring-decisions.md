# Wiring decisions — 2026-08-27

Normative for the CLI-wiring work. Where this file and `cli-wiring-design.md`
disagree, **this file wins**; where either disagrees with the code, **the code
wins**. `docs/superpowers/plans/2026-08-25-oxutrm-contract.md` outranks both.

Derived from a read-only design pass over `main` @ `a0bd05c`. Every line
citation below was verified against the code at that commit; `cli-wiring-design.md`'s
own citations are stale (written at `fc305c9`) and must not be trusted.

## 1. Five corrections to `cli-wiring-design.md` §4

**C1 — the §4 `select!` snippet (`:302-317`) does not compile.** Not style:
borrowck. `tokio::select!` keeps every arm's future alive while the winning
arm's *body* runs, so `self.link.source.recv()` (`&mut self.link.source`) and
`self.link.sink.connection().closed()` (`&self.link.sink`) are live while the
bodies call `self.turn(..)` / `self.resize(..)` taking `&mut self`. E0499/E0502.
**The select must yield a plain event value and touch nothing of `self`**; all
mutation happens after the select expression ends, which is when the losing
futures drop. Clone the `quinn::Connection` out of `self` first — it is `Clone`
— so the `closed()` arm borrows a local.

**C2 — `next_due()` is a 100% busy loop. Do not build it.** This is the most
important correction here, and it invalidates the doc's *argument*, not just its
snippet. `offer_frame` (`src/session.rs:374-394`) sets `last_send` **only when
`make_frame` returned a frame**, and `make_frame` returns `Ok(None)` when
`!state_moved && !ack_owed` (`crates/oxutrm-sync/src/channel.rs:104-111`) —
true whenever both ends are quiet. So `last_send` stays stale, `due()` stays
true forever, `next_due()` is an instant in the past, and `sleep_until` returns
immediately every iteration. The doc proposes this for **both** loops
(`cli-wiring-design.md:347`); on a detached host session — the very case its
argument is built on (`:355-366`) — it is strictly worse than the 4 ms poll it
replaces.

**Instead:** keep `due()` (`src/session.rs:387`) as the *floor* on how often a
frame may be offered, and give the loop its own *wake-up* deadline, recomputed
after every turn as `Instant::now() + self.link.sink.pacing_interval()`. Two
distinct things; conflating them is what produces the spin. Typing and resize
are unaffected — both set `last_send = None` (`:340`, `:404`) and send inside
the same `turn` call. Recompute rather than cache: `pacing_interval()` reads
`conn.rtt()` live (`src/link.rs:294`), and a cached value would survive a
migration that invalidated it — the same argument `FrameSink::send` already
makes for `max_datagram_size` (`src/link.rs:159-169`).

**C3 — tokio's `signal` feature is not enabled, and `process` does not imply
it.** The workspace enables `rt-multi-thread, net, time, macros, io-util, sync,
process`; tokio's `process` feature pulls `signal-hook-registry` but not
`signal`. `tokio::signal::unix::signal` will not resolve. One-line fix in the
**workspace** dependency table, zero new transitive crates — but it is a real
edit the doc never mentions.

**C4 — nothing carries an exit code.** `ClientSession::turn` never sets
`Turn.exited` (`src/session.rs:332-372`); only `HostSession::turn` writes it
(`:203`). There is no `exit` anywhere in `crates/oxutrm-proto/src/`. Design step
L14 (`cli-wiring-design.md:257`) says "print the exit line, `exit(code)`" with
no source for `code`. **This is a hole in the plan, not in its prose.**

**Decision:** the host closes the QUIC connection with the shell's status as the
application error code — `connection().close(VarInt::from_u32(code), ...)`. The
client reads it back from `close_reason()` as
`ConnectionError::ApplicationClosed`. Any other close reason is not a shell exit:
name it in an error and let `main` exit 255, as `ssh` does. Map
`child_exited()`'s `Err`/`-1` path (`crates/oxutrm-term/src/pty.rs:136-139`) to
255, never to a `u32` wrap. **No proto change, no new field**, and the code
travels on the mechanism that *is* the end of the session.

**C5 — `O_NONBLOCK` on fd 0 corrupts the user's shell, and this ships today.**
`src/main.rs:150` sets it on `rustix::stdio::stdin()` and nothing ever clears
it. `O_NONBLOCK` lives on the open **file description**, which is shared with the
parent shell and survives `dup`, so a dup-based workaround does not isolate it
and `RawGuard` (which restores termios only, `crates/oxutrm-client/src/guard.rs:63-68,128-135`)
cannot help. **`oxutrm loopback` — the one fully working path in the program —
returns the user to a shell whose stdin is non-blocking.**

**Decision:** do not restore it after the fact; avoid it. Open
`/dev/tty` afresh and set `O_NONBLOCK` on *that* file description. The user's
fd 0 is never modified, so there is nothing to restore on any exit path,
including `kill -9`. Same terminal, same input queue, no keystroke lost.
`RawGuard::enter` has already asserted `isatty(0)` (`guard.rs:77-81`) before
this point. Fallback if `/dev/tty` will not open: `try_clone_to_owned()` plus an
explicit guard. Neither path needs `unsafe`, so `src/main.rs:1`'s
`#![forbid(unsafe_code)]` holds.

## 2. `ClientSession::run` — shape

`run(&mut self, out: &mut W) -> Result<i32>` is
`run_on(File::open("/dev/tty")?, out)`. **The `run_on` split is required, not a
nicety:** `AsyncFd` cannot be built over a regular file (epoll returns `EPERM`)
and a test binary has no controlling terminal, so without it *every* test of
`run` needs a real tty and there will therefore be none. With it, tests pass the
read end of a `pipe()`. This mirrors `RawGuard::enter` / `enter_on`
(`guard.rs:76`/`:118`), which exists for exactly this reason.

Five readiness sources, all cancel-safe as `select!` requires: `AsyncFd::readable_mut`,
`FrameSource::recv` (`src/link.rs:366` — callerless today, this is its first
caller), `tokio::signal`'s `Signal::recv`, `sleep_until`, and quinn's
`Connection::closed`.

**EOF on the keyboard is a spin hazard.** An `AsyncFd` over an EOF'd descriptor
is *permanently readable*, so that arm would fire forever at full CPU. On
`read` returning `Ok(0)`, drop the `AsyncFd` and disable the arm with an
`if` guard. The session continues — output keeps painting, the user can no
longer type — which is right: killing a live remote shell because the local
terminal went away is precisely what this project exists to avoid.

**`FrameSource::push_back` — build a different thing.** A single slot on the
transport adapter is mutable state that exactly one caller in the tree could
ever use correctly, which is a shape this project has twice recorded as a
mistake. Instead keep the frame in the session: `turn` delegates to
`turn_with(input, first: Option<Frame>, out)`, whose drain loop starts
`next.take().or_else(|| self.link.source.try_recv())`. Four lines, no new
state, no way to strand a frame, `turn`'s signature preserved for every existing
test.

**`RawGuard` sits in `main`, above `run`** — entered after ssh has finished with
the terminal, dropped before the exit line prints. `run` never touches termios.
**stdout**: pass `BufWriter::new(stdout().lock())`, not `Stdout` — `Stdout`'s
`LineWriter` flushes on every `\n` and the renderer emits many, so a repaint
would become many `write(2)` calls. `turn` already flushes explicitly
(`src/session.rs:360`).

**250 Hz: replace it on the client, convert the host separately and
deliberately.** `IDLE_POLL` (`src/session.rs:58`) adds up to 4 ms to every
keystroke on the half a human is watching, and all five client sources are
pollable. The host is *not* a rider on this: there is no `impl AsFd` for `Pty`
or `HostTerm` today (`controller` is private with no accessor), and **the
child's exit is not pollable** — `Pty` holds a `std::process::Child` and
`child_exited` is `try_wait()` (`crates/oxutrm-term/src/pty.rs:132-140`), a
synchronous poll with no future. An event-driven host needs SIGCHLD, or
`tokio::process` in a crate that deliberately has no tokio, or a decision to
treat PTY EOF as the exit signal (usually right, not always — a child can close
its tty and live). And C2 applies with full force: converting the host with
`next_due()` gives a detached session 100% of a core.

## 3. QUIC client authentication

**Sequencing decision: client auth lands BEFORE `ClientSession::run`, as the
opening commit of the wiring work.** It has no dependency on `run` —
`quic_server`/`quic_client` exist today and are exercised by tests — so it is a
self-contained transport change with its own negative tests. The contract says
"in the same change that first wires the data path, never afterwards"; *before*
satisfies that a fortiori and guarantees there is no commit in history in which
a shell is reachable without client auth. Its only dependency is the 32-byte
newtype and base64 decode work.

**The client's certificate** is generated in the root binary before `ClientHello`
is written, with the existing `oxutrm_net::generate_cert()`
(`crates/oxutrm-net/src/tls.rs:80-94`) — the same call the host already makes.
Fresh per attach, key never on disk. No new crate-graph edge: the client cert
belongs to the local side and the root binary already depends on `oxutrm-net`.

**The signalling field.** `Signal::ClientHello` (`crates/oxutrm-proto/src/signal.rs:46-52`)
has no field for it; add `cert_spki_sha256` **as the 32-byte newtype, never a
second `String`**. Client to host, read at remote step R7 — two steps before
`quic_server` at R9, so the ordering the contract relies on is already in the
plan. `PROTO_VERSION` 1 to 2 (`crates/oxutrm-proto/src/lib.rs:117`); the version
test at `signal.rs:296` uses `PROTO_VERSION ± 1` and survives, but the fixtures
at `:283-293` and `:329-334` need the new field.

**`PinnedSpki` is NOT reusable, and must not be made so.** It implements
`rustls::client::danger::ServerCertVerifier` (`tls.rs:113`); the host needs
`rustls::server::danger::ClientCertVerifier` — a different trait, with
`verify_client_cert` (no server name, no OCSP) plus a required
`root_hint_subjects`. A new `PinnedClientSpki`, ~70 lines, in the same file.
Both fingerprints are `[u8; 32]`, so **a swapped argument compiles and produces
a host that pins its own certificate**: introduce role newtypes `HostSpki` and
`ClientSpki` and make the swap a compile error. Factor only the two genuinely
shared bodies — the SPKI comparison, and the three provider-delegating signature
methods copied verbatim (`tls.rs:141-174`); the contract calls out that stubbing
them reproduces the identical hole pointing the other way.

`root_hint_subjects` returns `&[]` — per RFC 8446 §4.2.4 an empty
`certificate_authorities` tells the client to send whatever certificate it has,
which is right when the trust root is a fingerprint carried over ssh, not a CA.

**`offer_client_auth()` and `client_auth_mandatory()` both default to `true`. Do
not override them — and test that they are true**, because a `false` there
silently reverts the entire change while every positive test still passes.

**Make the ordering structural before asserting it.** `quic_server` takes the
fingerprint **by value, required, no `Option`, no setter**. rustls `ServerConfig`
is immutable once the endpoint exists, so "pin it afterwards" becomes
unrepresentable rather than merely wrong — the `Detached`/`DetachPermit` idiom
(`crates/oxutrm-host/src/keys.rs:136-167`) applied here. There is nothing to
pass, so there is no call to write.

**Then assert it in a way that depends on the order.** An error-variant
assertion proves nothing here, because both orderings yield a failed handshake.
Use **accepts-A-rejects-B on ONE endpoint**: mint two independent client certs,
pin the host to A, and assert *both* that B never reaches a shell **and** that A
does, on that same endpoint. A permissive or late-installed pin fails the first
half; a pin wired from the wrong place fails the second. Run it twice with the
roles swapped. **Observe `HostSession::spawn` reached / not reached — a spawn
counter — not `Err(..)` from a handshake.** That is `a40ed8f`'s lesson: the
flood gate's `rejected == 0` passed while the diff path was 100% broken, because
the assertion was blind to the regime it guarded.

**Timing trap:** do **not** assert `quic_client(..).await.is_err()`. In TLS 1.3
the client finishes its handshake before the server has verified the client
certificate, and quinn emits `Connected` as soon as rustls stops handshaking, so
the client's `Connecting` will very likely resolve `Ok` and die a moment later.
**The host side is where the failure is deterministic**: `incoming.await`
resolves to `Err` and no session is created.

**The accept loop must be built with the hardening, because there is no accept
path to add it to later.** Factor R9/R14 into one `accept_one(&Endpoint,
nominated: SocketAddr)` in the root binary: reject any `incoming` whose
`remote_address()` is not the nominated one with **`ignore()`, never
`refuse()`** — a port scanner gets silence; `retry()` when
`!remote_address_validated()`, against spoofed-source floods; and **exactly one
connection per attach, then stop accepting** — under this model a second inbound
connection is always wrong. Roaming is unaffected: a roam reuses the connection
through path validation, not a new handshake.

## 4. Size, and the item the task framing understates

Rough production-line estimate for the full "wire the data path" work:
`ClientSession::run` + `turn_with` + `exit_code` 130-170 (plus ~200 test);
tokio feature and `/dev/tty` ~10; proto field + version bump ~60; client auth +
role newtypes + signature churn across ~11 call sites ~110; four negative tests
~150; `accept_one` ~60; **ladder driver 250-400**; `run_host_serve` ~200;
`run_connect` ~200. **Total ~1000-1500 production lines.**

**The ladder driver is the largest single item — larger than either design
above** — and the task framing treats it as background. It must satisfy the
contract's four properties, including rendering skipped-vs-failed, the one
property that *was* tested and now is not.

## 5. Order of work

1. base64 decode and typed 32-byte values
2. **client auth**: role newtypes, `PinnedClientSpki`, `accept_one`, four negative tests
3. `ClientSession::run` + `turn_with` (+ `run_on` pipe tests)
4. ladder driver in the root binary
5. `run_host --serve` R1-R16
6. `run_connect` L1-L14

`HostSession::run`'s event loop comes after, deliberately, with §1 C2's deadline
rule and an answer to the un-pollable `try_wait`.

## 6. Two smaller things to carry into the work

**L10 can be made structural.** `SshChannel::recv` treats EOF as failure
(`crates/oxutrm-host/src/ssh.rs:246-254`) and nothing stops the client calling it
after `Established`, when the host is about to sever. Do **not** enforce it by
dropping the channel: `kill_on_drop(true)` (`ssh.rs:194`) would SIGKILL the local
ssh, and the struct also owns the stderr drain task (`:212-228`) that must keep
running or a chatty ssh blocks on a full pipe. Add
`SshChannel::into_idle() -> IdleSshChannel`, a wrapper with **no `recv`
method**, holding the channel alive and unpolled. Rung 4 keeps the full
`SshChannel`. Same idiom as `Detached`/`DetachPermit`: the wrong call is not
written because there is nothing to write it on.

**`src/main.rs:21-25`'s `#[allow(dead_code)]` comes off** with steps 3 and 6,
and its own comment says so. Removing it is a real check that the wiring landed.

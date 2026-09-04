# Changes

## Unreleased

### New

- **A running session can be picked up from another terminal.** `oxutrm host
  --list` has always shown live sessions and `--attach` has always refused to
  do anything about them, because the socket every session registers was never
  bound. It is bound now: attaching hands the same shell to the new terminal,
  with the screen it has right now, and tells the terminal that had it that it
  was taken over rather than leaving it to report silence.

### Changed

### Fixed

- **A session whose network came back stayed dead for minutes.** Removing the
  idle timeout in 0.2.0 so a session could outlive an outage also removed,
  unnoticed, the only thing bounding QUIC's exponential probe backoff: the
  probe interval is `pto_base * 2^min(pto_count, 16)`, and normally the idle
  timeout closes such a connection long before that matters. With no idle
  timeout nothing did, so a session whose path had returned waited on a timer
  that had doubled its way into the minutes — both ends alive, host merely
  detached, path perfect, terminal dead. The backoff exponent is now capped at
  6, so the probe interval tops out at 64x the base rather than 65536x. A
  150-second blackout recovers in 1.24 s where it used to take 106.65 s; a
  60-second one in 0.55 s where it took 10.19 s. This costs about three packets
  per second while a path is out and nothing at all on a healthy connection,
  where the probe counter is zero. The cap needs a knob `quinn-proto` does not
  expose, so the build carries a patched copy under `vendor/`; see
  `docs/quinn-pto-backoff.md`.

## 0.2.0 - 2026-08-30

### New

- **The session survives a network outage instead of dying at thirty seconds.**
  The QUIC transport no longer imposes an idle timeout, so silence stops ending
  a session: the client's own state machine decides the host has gone quiet,
  raises the notice at two seconds, holds what is typed blind, and keeps the
  connection for whenever the network comes back.
- **The client follows a local address change.** While the link is silent, a
  route probe asks the routing table which source address it would now use for
  the host — a throwaway UDP socket, `connect`ed, no packet sent. If it moved,
  the session socket is swapped underneath the live connection, which QUIC
  allows because it identifies a connection by connection IDs rather than by
  addresses.

### Changed

- The host now decides for itself when a client has gone away, after thirty
  seconds without a frame, rather than waiting for the transport to give the
  connection up — which it no longer does. Behaviour is unchanged; a session
  whose client vanished still stops building screens nobody will see.

### Fixed

- A frame arriving between `Ctrl-\` and its confirmation letter could make the
  `Confirming` box eat its own confirmation key, silently swallowing `Ctrl-\ d`
  or `Ctrl-\ s` and delivering the keystroke into the held buffer instead.
  `LinkState::heard` cleared `prefix_pending` on every arriving frame; it now
  clears it only when the frame actually changes the phase. Shipped in 0.1.0,
  fixed here.
- **An attach whose host never finishes the handshake now fails instead of
  hanging.** The client's `connect` had no deadline of its own, and quinn arms
  its idle timer only once a packet has been authenticated — so a host that
  never answered left the client waiting for ever, with the terminal already in
  raw mode and nothing on it: no output, no prompt, no `Ctrl-C`. It now gives up
  after thirty seconds, the same deadline the host's accept uses, and says which
  host it waited for and for how long. Shipped in 0.1.0, fixed here.

### Compatibility

- **Both ends must be on this version** for the idle timeout to be gone. QUIC
  negotiates the effective timeout as the minimum of the two peers', so a new
  client against an `0.1.0` host still dies at thirty seconds of silence.
- Reattaching a session you were disconnected from is still not implemented;
  `oxutrm host --attach` says so. That is the next phase.
- **Not yet verified against a real, unreliable network.** The automated
  suite covers this phase, including a real 35 s wall-clock outage test,
  `a_session_outlives_a_silence_that_used_to_kill_it` — too slow for the
  default suite, so it is `#[ignore]`d; run it explicitly with
  `cargo test -j4 --bin oxutrm outlives_a_silence -- --nocapture --ignored --test-threads=1`.
  A shorter sibling, `a_short_silence_raises_and_clears_the_notice_on_a_real_clock`,
  covers the notice-raised-and-cleared half in every default run. What is not
  yet done is a hand test against `thinlinc`'s real network; the method is
  written down with every measurement marked NOT YET TAKEN.

## 0.1.0 - 2026-08-29

### New

- `oxutrm <ssh-target>` connects. The local half drives ssh, exchanges
  candidates, races the connection ladder as the controlling side, brings up
  QUIC on the nominated path and hands over to the session loop.
- `oxutrm host --serve` runs the remote half: it detaches from ssh, mints
  per-attach key material, gathers candidates, races the ladder as the
  controlled side, accepts exactly one connection, settles detachability from
  the rung that won, severs, registers the session and starts the shell.
- A session survives the client going away and appears in `oxutrm host --list`
  as detachable.
- A host that stops answering now says so. The signal was already on the wire:
  the sync engine sends an empty diff purely to move an acknowledgement it
  owes, so every input obliges a reply, and a reply owed for two seconds with
  nothing arriving is a true round-trip failure rather than an inference. The
  client draws a box over the screen reporting how long the host has been
  quiet and what the connection itself has counted. Two seconds of grace,
  because an indicator that fires on every hiccup is the noise it was built to
  remove.
- The box states only what the client can actually see. It never says the
  session is safe, because a dead network and a crashed host are
  indistinguishable from this end, and it never says it is reconnecting,
  because nothing reconnects yet. A box that guesses is worse than one that
  admits.
- Keystrokes typed while the host is not answering are kept rather than
  delivered, and shown back for confirmation when it answers again. Replaying
  blind input against a screen that moved while nobody could watch it is how a
  half-typed command completes into something never intended. `Ctrl-\` is a
  prefix, live only while the box is showing — `q` closes oxutrm here, `s`
  sends what was typed, `d` drops it — so a healthy session passes every byte
  to the host untouched. The buffer stops accepting at 64 KiB instead of
  discarding its oldest bytes: the oldest are the command and the newest are
  the newline, and dropping from the front is precisely how a truncated
  command still runs.
- An idle session notices an outage too. With nothing outstanding, silence and
  calm are indistinguishable, so after five seconds of quiet the client says
  something merely to be answered — otherwise the first sign of trouble would
  be a keystroke pressed into a screen that had been dead for ten minutes.
- Static `x86_64` and `aarch64` Linux builds, as `.tar.gz`, `.deb` and `.rpm`.

### Changed

- The process ssh waits on now stays alive until the session severs, so the
  prompt returns when the session detaches rather than when it forks. sshd
  closes a session's **stdin** as soon as the command it is waiting on exits,
  whatever else still holds the descriptor, and the whole handshake reads from
  it.
- The client paints two layers. The remote framebuffer is one; local UI drawn
  here and never sent anywhere is the other, composited into the renderer's
  grid *before* its diff. Because the diff is what paints, raising the box and
  removing it are both ordinary diffs — no full repaint, nothing to
  invalidate, and it works while the host is unreachable because the model is
  local. `ratatui` supplies the layout and the widgets, headlessly: no
  backend, no terminal ownership, and the renderer remains the only thing in
  the tree that writes to your terminal.
- Repaints are wrapped in synchronized output (DECSET 2026), so the terminal
  shows one at once instead of mid-tear. Emitted unconditionally, because a
  conforming terminal ignores a private mode it does not know — there is
  nothing to detect and nothing to negotiate. A repaint that changes nothing
  still writes nothing.
- Losing the link says what happened and what to try, instead of "the link to
  the host ended without the shell exiting: timed out". It also says that
  reattaching is not implemented yet, because it is not.

### Fixed

- Detaching no longer closes the UDP socket the connection runs over. The
  sever enumerates descriptors at the moment of the fork, before anything of
  ours is open, rather than at the moment it severs — by which time the socket
  the ladder punched is among them, and it cannot be reopened, because the NAT
  mapping belongs to that exact socket.
- Neither signalling reader can be made to allocate on the peer's say-so.
- The client no longer writes diagnostics to its own stderr. That stderr *is*
  the terminal it is painting, so every such message desynchronised the
  renderer's model of the screen and nothing repainted over it on a quiet
  session. A frame that cannot be applied is now a number in the box, which is
  where a diagnostic about the link belonged all along.


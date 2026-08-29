# Changes

## Unreleased

### New

### Changed

### Fixed

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


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
- Static `x86_64` and `aarch64` Linux builds, as `.tar.gz`, `.deb` and `.rpm`.

### Changed

- The process ssh waits on now stays alive until the session severs, so the
  prompt returns when the session detaches rather than when it forks. sshd
  closes a session's **stdin** as soon as the command it is waiting on exits,
  whatever else still holds the descriptor, and the whole handshake reads from
  it.

### Fixed

- Detaching no longer closes the UDP socket the connection runs over. The
  sever enumerates descriptors at the moment of the fork, before anything of
  ours is open, rather than at the moment it severs — by which time the socket
  the ladder punched is among them, and it cannot be reopened, because the NAT
  mapping belongs to that exact socket.
- Neither signalling reader can be made to allocate on the peer's say-so.


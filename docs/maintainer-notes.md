# Maintainer notes

Rationale that the README and the man page leave out. Each section names the
place in the user docs it belongs to.

## Why oxutrm exists (README, intro)

`ssh` plus `tmux` is "reconnect and hope". Close a laptop lid, walk between
Wi-Fi and mobile, or move between offices, and the shell is gone or the
connection hangs until a TCP timeout notices. oxutrm replicates state instead
of streaming it, so a lost packet costs nothing, and the connection is QUIC, so
an IP change is a migration and not a disconnection.

The emulator on both ends is the main difference from Mosh. Mosh predicts what
the screen will look like; the oxutrm client renders from the host's
authoritative state.

## Static binaries (README, Installing)

The far end is a machine the user did not choose and cannot rebuild for. A
dynamically linked binary built on a current distribution refuses to start on
an older one (glibc 2.39 against a host at 2.35 is enough), and it fails inside
a session nobody can see. The release binaries are therefore static.

`ssh <target> command -v oxutrm` is the check because a login shell's `PATH` is
not the one a non-interactive ssh command uses.

## No Windows build (README, Installing)

oxutrm uses `fork`, `setsid`, a pty and a Unix socket. The two places that
were once Linux-only, enumerating the process's descriptors and asking when a
pid started, have macOS implementations.

## UDP/443 (README, How it connects)

Restrictive networks tend to leave UDP/443 open, because blocking it breaks
HTTP/3 for every browser on the network.

## Rung 4 is a warning (README, How it connects)

A session on the ssh tunnel cannot detach and cannot be reattached. Degrading
to it silently would remove both properties the project exists to provide.
oxutrm does not put another party's domain into SNI to disguise its traffic.

## Known limitations, background (README)

- Blink: the emulator parses SGR 5/6/25 and then discards them, so oxutrm
  recovers blink in a parallel plane. A resize reflows the grid, and the plane
  is cleared rather than guessed at.
- Underlines: drawing a curly underline as a straight one was preferred to
  drawing none.
- OSC 1 is dropped by the parser, so an icon-name field could only hold a value
  oxutrm invented.
- Both ends behind NAT: the ladder is symmetric by construction and candidate
  exchange runs both ways, so there is no known reason it would fail, but no
  test proves it.
- QUIC migration lets a client change its own local address; there is no
  mechanism to repoint an established connection at a different remote address.
- The standby filter compares the kernel's reported source address for each
  candidate, not the interface (`src/egress.rs`).

## Security (README)

The client pins the SPKI fingerprint and still verifies the handshake
signature: pinning without signature verification is no authentication at all.
ICE credentials are direction-labelled.

## --workspace in every make rule (README, Building)

The repository root is both a `[package]` and a `[workspace]`, so a bare
`cargo test` selects only the root binary and reports "ok" while running none
of the crates' tests.

## oxutrm-sync has no I/O (README, Layout)

No sockets, files or clocks is what lets the riskiest property of the protocol
(for any sequence of states and any subset of frames dropped, duplicated or
reordered, the two ends converge) be tested exhaustively without a network. A
test asserts it against the crate's own manifest, so the boundary is enforced
and not only remembered.

# Live paths: per-family rungs, continuous discovery, best-path switching

Status: design, awaiting approval · 2026-10-19 · builds on PR #22
(`fix/stun-both-families`)

## 1. Why

On 2026-10-19, oxutrm from a Mac to `thinlinc` only ever came up through a
WireGuard VPN, although both ends sit behind easy, port-preserving NATs and a
hand-made UDP punch between their public addresses worked at once. The cause
(fixed in PR #22) was that the Mac, having global IPv6, learned only its IPv6
STUN mapping and never offered its public IPv4 one. Finding that took a day,
because:

- the ladder does not know address families: IPv4 and IPv6 share every rung,
  a LAN/VPN IPv4 path is labelled "IPv4 punched", and one NAT verdict governs
  both families;
- what each end can offer is gathered once per attach and then frozen; nothing
  notices or exchanges a change during the session, and the host watches
  nothing at all;
- the only alternative path is a single standby, found by a costly search with
  back-off, and the session never moves to a better path voluntarily;
- the measurements and the reasons were invisible (the log cut them off).

The intent: **a session always knows which paths could exist between its two
ends right now, measures them, and runs on the best one** — over its whole
lifetime, while address families, addresses and NAT mappings come and go on
either side.

## 2. Goals

1. **IPv4 and IPv6 are separate rungs.** Every path class exists per family,
   and is planned, tried, measured, reported and failed per family.
2. **Continuous discovery on both ends.** Each end keeps finding out what it
   can offer: which families work, every local address, port mappings, STUN
   mappings — and notices when any of it changes.
3. **Continuous exchange.** Each end tells the other about changes over the
   running session (never over ssh). The host can send unprompted.
4. **Several addresses per host are several paths**, not only standby fodder.
5. **Loss of the current path behaves like any loss of connectivity**: a short
   outage, then the session carries on over whatever still works — whatever
   family that is.
6. **The session moves to a better path while the current one still works.**
7. **A paths screen** shows every path with its measurements and lets the
   user switch, pin, and block (§6).
8. **In scope as well:** a working ssh tunnel (rung 4), router port mapping
   actually requested, and a birthday blast that only aims at a family both
   ends can use.

## 3. Non-goals and anti-goals

- **No protocol compatibility.** There are no deployments; bump
  `PROTO_VERSION`, design the wire cleanly, no compat flags.
- **No path memory across sessions.** Blocks and pins live for one session.
- **Not** a general multipath transport: one path carries the session at a
  time; the others are measured and ready.
- **Anti-goal: ping-pong.** A session that keeps hopping between two similar
  paths is worse than one that stays on a slightly worse one.
- **Anti-goal: silent cost.** Background measuring traffic must be known and
  bounded (§5.4); it is not allowed to just grow with the number of paths.
- **Anti-goal: invisible decisions.** Every move, its reason, and every path
  that cannot exist and why, must be visible (§6, activity log).

## 4. What this replaces

- The standby spec's non-goal "no voluntary switching, no fail-back"
  (`2026-09-25-standby-and-rendezvous-design.md` §1.2) and the manual's
  "It does not switch back" are **reversed** by goal 6.
- The single standby slot becomes a set of measured paths (§5.3). "Standby"
  survives only as a role a path can have.
- The design spec's §5 rung list is replaced by the classes in §5.1.

## 5. How — decided

### 5.1 Path classes and rungs

A **path** is one (local interface, local address, remote address) triple
with a **class**. The interface is part of the identity: two interfaces in the
same subnet (Wi-Fi and Ethernet, both up) are two paths even when the kernel
would route everything out of one of them.
A class is a **family** (IPv4, IPv6) × a **method**:

| method       | what it is                                                     |
|--------------|----------------------------------------------------------------|
| direct       | host candidate ↔ host candidate (global IPv6, LAN, VPN)         |
| port mapped  | through a NAT-PMP / PCP / UPnP-IGD mapping                      |
| punched      | STUN-learned (srflx/prflx) hole punch                          |
| birthday     | birthday-paradox punch, for a symmetric NAT                    |
| ssh tunnel   | QUIC datagrams through the ssh connection; family-less          |

So the rungs are: IPv6 direct, IPv4 direct, IPv6 port mapped, IPv4 port
mapped, IPv6 punched, IPv4 punched, IPv6 birthday, IPv4 birthday, ssh tunnel.
Labels everywhere (status line, report, paths screen, log) name family and
method: "IPv4 direct", "IPv6 punched". A LAN or VPN IPv4 path is "IPv4
direct", never "punched".

- The NAT type is learned **per family** and gates that family's punched and
  birthday rungs only. A symmetric IPv4 NAT no longer suppresses IPv6 punching.
- A family that one end cannot use at all (no address, no route) makes that
  family's rungs **skipped, with the reason** ("thinlinc has no global IPv6"),
  not failed — the existing skipped-vs-failed rule extends per family.
- The birthday blast guesses around a reflexive address **of a family the
  local end can reach**, and only for a family whose NAT is symmetric.
- **Order of trying** at first connect stays cheap-first (direct, port mapped
  and punched raced; birthday, then ssh tunnel only if nothing else
  validated). Order says nothing about which path ends up carrying the
  session: measurement decides that (§5.3).

### 5.2 Continuous discovery and exchange

- **Each end** — client *and* host — keeps an up-to-date list of what it can
  offer: every usable local address per family, its STUN mappings per family,
  its port mappings, and whether each family currently routes at all.
- **A link change re-checks at once** — a new network, a new address, a new
  public mapping, an interface appearing or going — on either end, while the
  current path still looks fine, not after silence. (Observed 2026-10-19:
  the Mac moved onto thinlinc's site network; the session found its new path
  only through a 22 s outage and an ssh rebuild.)
- It **re-checks** when the system reports an address/route change, and
  periodically besides; it refreshes STUN and port mappings before they would
  expire (design spec §5.2/§5.3 "keepalive / re-punch", never built).
- On any change, it **sends the new list to the other end over the running
  session**. The host may send unprompted. Each end keeps the peer's latest
  list and shows its age ("updated 12 s ago").
- A change in either list adds, drops, or re-forms candidate paths (§5.3).

### 5.3 Measuring, choosing, switching

- Every candidate path that validates becomes a **measured path**, measured
  for round-trip time and loss. Up to `network.warm_paths` of them are warm
  (established connections, §5.4); the rest are cold and re-probed. (Because QUIC cannot repoint a connection at another remote, every
  path is its own connection, as the standby is today.)
- **Better = measured quality**: lower round-trip time and lower loss.
  Measured quality wins over method — a punched path at 12 ms beats a direct
  one at 18 ms. Direct beats punched only as a tie-break, when the two
  measure about the same.
- **The comparison rule** (agreed with the user). Typing echo costs about one
  round trip; under ~50 ms it feels fine, and loss hurts as occasional stalls
  rather than as constant delay:
  - **Both paths under 50 ms rtt — the comfortable zone:** lower loss wins;
    if loss is about the same (within 0.5 percentage points), direct beats
    punched; after that, lower rtt wins only if it is at least 10 ms better.
  - **Either path over 50 ms:** compare *felt delay* = rtt + loss weight,
    where **1 % loss counts as 10 ms**; the lower felt delay wins, by a margin
    of at least 10 %.
  - Examples: 10 ms / 2 % vs 20 ms / 0 % → comfortable zone, the lossless one
    wins. 60 ms / 3 % (felt 90) vs 80 ms / 0 % (felt 80) → the lossless one
    wins.
  - The 50 ms line, the 10 ms-per-1 % weight, the 0.5-point loss tie, the
    10 ms and 10 % margins are **named constants**, to be tuned after real
    use. Measurements are taken over a recent window (window length is the
    builder's call), not single samples.
- **Moving to a better path** happens while the current one still works, as
  a handover like today's failover: the new path is already established and
  confirmed; the session moves onto it; the old one stays measured (or is
  dropped if it no longer earns a place, §5.4). The user sees an activity-log
  line ("moved to IPv4 punched: 12 ms vs 18 ms on IPv4 direct").
- **Damping** against ping-pong (anti-goal): a move needs a clear margin,
  held for a while, and a path that just appeared is not trusted at once.
- **Loss of the carrying path** = an outage as today: input is held, then the
  session continues on the best path that still answers. If none does, the
  ssh rebuild runs as today.

### 5.4 Background traffic — decided (2026-10-21)

Measured in stage 1 (`docs/superpowers/notes/2026-10-19-path-cost-spike.md`):
the shortest idle NAT lifetime on the test links is 20–30 s, and a warm path
costs 19 B/s each way for the keepalive alone, 37 B/s with a measurement ping
every 5 s (IPv6 on-wire; IPv4 about a quarter less). The user chose this rule:

- **Keepalive stays at 10 s** on every warm path. It is the floor the NAT
  lifetimes force (shortest lifetime ÷ 2), not a budget knob.
- **At most `network.warm_paths` paths are warm, default 4**, the carrying
  path included. A warm path is an established connection, keepalive plus a
  **measurement ping every 5 s (0.2 Hz)**. At the default: about 150 B/s and
  1.7 datagrams/s each way. `warm_paths = 1` is today's cost (carrying path
  only); the useful upper end is about 8.
- **Paths beyond the cap are cold**: listed with their last measurement and its
  age, and **re-probed about once a minute**. A cold direct path (LAN, VPN,
  unNATed IPv6) is a cheap probe; a cold punched path needs a fresh punch,
  since its NAT mapping is gone after ~30 s. Which paths are warm follows the
  comparison rule of §5.3: the best measured ones stay warm, a cold path that
  re-probes better than the worst warm one takes its place.
- **Measurement speeds up to 1 Hz on the warm paths while the carrying path is
  degrading** (rising loss or rtt by the margins of §5.3), and drops back to
  0.2 Hz once it has recovered or the session has moved.
- *Builder's call:* align the keepalives and pings of warm paths on the same
  interface, so a mobile radio wakes once per interval rather than once per
  path.

### 5.5 Router port mapping

`PortMapping::acquire` exists but is never called. Wire it into discovery on
both ends (per family where the protocol supports it — PCP does IPv6), keep
mappings refreshed for the session's life (design spec §5.2), and release
them at session end. Governed by the existing `network.port_mapping` setting.

### 5.6 ssh tunnel (rung 4)

Build the tunnel: QUIC datagrams carried over the ssh connection's byte
stream, as the last resort when no UDP path validates.

- Framing per the contract: an explicit length prefix, and the length
  validated against the maximum **before** any allocation.
- **Last resort only, decided by the user:** the tunnel is used only when no
  UDP path validates at connect (or at an ssh rebuild), and such a session
  **stays a tunnel session for its whole life**: it does not move onto UDP
  paths that appear later, and a UDP session never falls back to a tunnel.
  So ssh still only starts things for every non-tunnel session, and the
  path set of §5.3 does not have to model the tunnel.
- A tunnel session is not detachable and ends with its ssh connection
  (contract: `detachable = rung != SshTunnel`). It is announced as a warning,
  as the manual already says.
- The paths screen of a tunnel session shows the tunnel with its
  measurements; whether it also lists (unused) UDP candidates is the
  builder's call.

## 6. The paths screen (user's pick: mockup B)

- Opened with **`p`** from the status popup, like `c` (config) and `s`
  (sessions). Title: `oxutrm · paths · <target> ─ auto|pinned`.
- **One block per path**: class label; role (● carrying, standby, measured,
  blocked); rtt with range, loss, time on path; both addresses with the
  local interface (`via utun32`); an rtt sparkline.
- **Same public address on both ends** (both behind one NAT, e.g. at the
  same site): a punch between the two public addresses needs the NAT to
  hairpin, which many do not. Such a punched path is tried, and when it fails
  its reason says so ("both ends share 203.0.113.7 — needs NAT hairpinning");
  the paths that matter there are the direct ones.
- **Paths that cannot exist are listed greyed, with the reason**
  ("IPv6 direct — not possible: thinlinc has no global IPv6").
- Bottom sections **"this end"** and **"<host>"**: what each end currently
  offers per family, and the peer list's age.
- Keys: **↑↓** select · **⏎ switch to it** (pins it) · **a** back to auto ·
  **b** block / unblock · **Esc** back.
- The status popup's standby section shrinks to one summary line (e.g.
  `3 paths · auto · best: IPv4 punched 12 ms`; wording is the builder's).

### 6.1 Pin and block rules

- **Pin**: ⏎ on a path moves the session to it and pins it; automatic moves
  stop. If the pinned path stops answering, the session falls over to the
  best remaining path **and returns to auto**.
- **Block**: `b` blocks **that address pair, for this session**. A blocked
  path is never chosen automatically — **unless it is the last path that
  works**, then it is used anyway. A blocked path keeps being measured so the
  user can see whether its problem went away. `b` again unblocks.
- *Builder's calls:* blocking the carrying path moves the session off it at
  once if anything else works. Pins and blocks live in the client process and
  survive an ssh rebuild within the same run. Only the client chooses and
  switches; the host adopts the new path on its first frame, as it adopts a
  standby today. Port mappings are released when the host session ends, not
  on detach.

## 7. Constraints (from the code and earlier decisions)

- **No remote repointing**: a QUIC connection cannot move to another remote;
  each path is its own QUIC connection (`link.rs`, `quic.rs` docs).
- **Sockets (builder's call, made during design):** "one socket per
  connection" is today's code, not a rule, and it is dropped. Each end keeps
  **long-lived sockets** — one per usable local address and family — whose
  STUN and port mappings it refreshes and advertises (§5.2). Paths are QUIC
  connections **sharing** those sockets through one endpoint per socket, so
  an advertised mapping is directly valid for a new path and keepalive cost
  is per socket, not per path. The birthday blast, which needs its own
  socket fan, is the exception.
- **Sockets are bound to their interface**, not only to an address:
  `IP_BOUND_IF` / `IPV6_BOUND_IF` on macOS, `SO_BINDTODEVICE` (or the
  equivalent) on Linux. Binding to an address alone does not choose the
  interface — on macOS a socket bound to en0's address still leaves through
  en7 when the routing table prefers en7. Without this, a second interface in
  the same subnet can never carry a path.
- **The standby filter by source address goes** (`egress::avoiding_source`):
  paths on different interfaces are different paths by identity, so "a
  second path that leaves differently" is decided by interface. That is what
  the standby spec §3.3 asked for in the first place.
- **Nomination completes before QUIC starts**, per connection
  (`oxutrm-host/src/ladder.rs` docs) — still true per path; the new part is
  that there are many paths over a session's life.
- **NAT mappings are per socket**: a candidate learned on one socket is only
  valid for that socket (`StunDemuxSocket`, ladder docs).
- **`oxutrm-host` must not depend on `oxutrm-net`**; the ladder driver lives in
  the root binary.
- **ssh only starts things**; everything after connect goes over the running
  session. The ssh tunnel is the one deliberate exception, as last resort.
- The **skipped-vs-failed** rule of the ladder report holds per family.
- `client.log` keeps whole reasons and both ends' candidates (PR #22) — the
  same detail must be reachable for every path.

## 8. Build stages

1. **Per-family rungs** (§5.1): classes, labels, per-family NAT type,
   per-family skip reasons, birthday family fix. Plus the **traffic spike**
   (§5.4).
2. **Continuous discovery and exchange** on both ends (§5.2).
3. **Measured paths and best-path switching** (§5.3), replacing the single
   standby; damping.
4. **Paths screen** (§6), pin and block.
5. **Router port mapping** (§5.5).
6. **ssh tunnel** (§5.6), last resort at connect only.

Each stage ships on its own and leaves the session working.

## 9. Left to the builder

- Wire message names and shapes for the capability updates; how the host
  sends unprompted (e.g. a host-opened stream).
- Change detection mechanism (OS notifications vs polling) and periodic
  re-check interval.
- Damping beyond the comparison margins of §5.3: how long a margin must
  hold, and how long a new path is distrusted.
- Measurement method per kept path (QUIC keepalives/pings on each
  connection is the obvious one).
- Rung numbering in reports; serde names (no compat needed).
- Exact wording of labels and of the status popup summary line.

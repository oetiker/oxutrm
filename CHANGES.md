# Changes

## Unreleased

### New

- **A running session can be picked up from another terminal.** `oxutrm host
  --list` has always shown live sessions and `--attach` has always refused to
  do anything about them, because the socket every session registers was never
  bound. It is bound now: attaching hands the same shell to the new terminal,
  with the screen it has right now, and tells the terminal that had it that it
  was taken over rather than leaving it to report silence.

- **Connecting to a host resumes your session there instead of replacing it.**
  `oxutrm <ssh-target>` now asks the far end what is already running before
  either side commits to anything. One session of yours, and it is resumed —
  same shell, same scrollback, same screen — without asking. Several, and
  oxutrm lists them and asks which; `q` leaves without touching any of them.
  None, and you get a new one, exactly as before. `--attach <id>` names one
  directly, by as few as four characters of its id, and `--new` starts a fresh
  session however many are already there. Killing a client and reconnecting
  used to strand the old session: it stayed live, holding your shell, and the
  reconnect started a second one beside it.

  Connecting prints one line saying which of the two happened — `oxutrm:
  resumed session <id>.` or `oxutrm: new session <id>.` — before the terminal
  goes into raw mode. The id is the one thing worth writing down to `--attach`
  back into later, and the first word is there because landing in a two-day-old
  session with a half-typed command already at the prompt should not be
  something you have to work out from the screen. A session that cannot be
  resumed because it tunnels its data through the ssh connection that created
  it is still offered, and refused with that reason rather than quietly
  omitted.

- **A client whose network dies reconnects by itself.** After twenty seconds of
  silence — long enough that a blip is not raced against an outage about to end
  on its own — the client builds a new link back into the same session: a fresh
  ssh, the same handshake a first connect runs. The first attempt goes out
  immediately, because the twenty seconds have already been waited; each
  failure after that backs off, two seconds, then four, then eight, and every
  eight from then on. It never gives up on its own; `q` in the status popup is
  how you stop it.

  The old link is held throughout, so whichever path comes back first wins: a
  frame arriving on it ends the rebuild, and a rebuild landing first swaps the
  transport under a session that never noticed. The status popup says how long
  the host has been quiet, which attempt is next, when it is due, and why the
  last one failed. If the far end answers that the session is gone, that is an
  answer rather than an outage: oxutrm says so and stops. An attempt that gets
  no answer at all — a far end that accepts the connection and then goes quiet
  — is given up after two minutes and tried again, rather than leaving the
  client waiting on it for ever.

  Once an outage is over, it is over: if somebody else takes the session over
  later on, oxutrm tells you and exits, exactly as it would have if no outage
  had ever happened. It used to stay quietly convinced, for the rest of that
  session, that any takeover must be its own reconnection — so it swallowed the
  notice, sat on a connection the host had already closed showing a screen that
  would never change again, and after twenty seconds took the session back off
  whoever had attached to it.

  Those attempts run `ssh -o BatchMode=yes`, so a key that needs a passphrase
  typed will not reconnect: raw mode is held and the screen belongs to the
  renderer, so a prompt would fight it for the terminal. The attempt fails
  cleanly and the box says why, which is the deliberate half of this until the
  askpass work lands. An agent coming back is a real way it resolves.

- **The client keeps a second, warm connection to the session, and fails over
  to it.** While a link is up, the client builds a standby: a second QUIC
  connection over whatever path the kernel would route through a different
  local source address than the primary — a split-tunnel VPN next to the open
  internet, say. The ten-second QUIC keep-alive that already holds a punched
  NAT mapping open keeps this one warm too: one more keep-alive every ten
  seconds on each end, and nothing else while nothing is wrong.
  When the primary goes silent and the standby still answers, the client fails
  over to it about three seconds in, with no ssh and no new handshake prompt —
  never because the standby is faster, only because the primary has stopped
  answering, and there is no switching back once it has. A host advertises the
  capability with `features: ["control", "standby"]` in its hello; an older
  host omits it and the client falls back to the twenty-second ssh rebuild
  exactly as before.

  The filter that keeps the standby off the primary's own path compares
  kernel-reported source addresses, not interfaces (`src/egress.rs`): a
  dual-stack or multi-address NIC can still hand both connections the same
  physical wire under different addresses, so the standby can share the fate
  of the primary it was meant to replace. There is no detection for that case.

  A machine with only one way out (a single IPv4 uplink, say) never finds a
  standby, but it keeps looking: each search runs a full attach exchange on
  both ends, STUN included, and moves the session's attach number on. After
  the first few the searches settle at one every five minutes, so on such a
  machine the attach number `--list` shows climbs by about twelve an hour.

  The two-uplink network-namespace test the design describes (§7) is not
  written; failover here is covered by the in-process relay test and a hand
  test on 2026-10-03: a Mac client attached to a Linux host over a
  split-tunnel VPN kept its session when the VPN was dropped, moving to the
  standby without ssh. The stall was not timed.

- **A status popup shows what the connection is doing.** `Ctrl-\` opens it in
  every phase while it is closed; pressed twice within half a second it sends
  one literal `Ctrl-\` to the remote program instead. It also opens by itself
  two seconds into an outage, and when the link comes back it says so —
  `● LIVE again   outage 4.2 s · IPv4 punched · …` — for three seconds before
  closing, unless you pressed a key in it. It shows the round-trip time now
  and its range over the last minute beside an RTT sparkline with gaps where
  the link was down, loss, throughput, which link of the session this is, the
  standby, during an outage what the standby probe and the ssh rebuild are
  each doing and how long for, and the last things oxutrm did.
  While it is shown it takes every key: `Esc` or `Ctrl-\` closes it, `q`
  quits, and nothing else you type goes anywhere. You can close it during an
  outage too; it then stays closed until that outage ends, what you type
  meanwhile is held as before, and when the host answers again the popup opens
  to ask about it: `s` sends what you typed, `d` drops it; keys pressed in the
  first half second after the question appears do nothing, so typing still in
  flight cannot answer it. `c config` and `s sessions` are shown dimmed; they
  come later.

- **A session opens with the oxutrm logo.** Once the terminal is in raw
  mode, a fresh connect shows the ox head tuning in like a badly received
  TV — about 0.8 s of snow, torn rows and flicker across the whole screen,
  settling into the logo with the name under it, held for 1.5 s — and then
  your remote screen. Under the name it says which session you landed in and
  how it is reached (`resumed session 3ff1218f · IPv4 punched`), since the
  logo covers the line that says so before raw mode. Any key ends it at once and still goes to the remote
  program, so it never costs waiting. If the host's first screen is a moment
  late, the logo stays until it arrives; a host that does not answer at all
  brings up the status popup in its place, as any outage starting meanwhile
  does. A session that ends while it shows ends on the remote's last screen,
  not on the logo. It is drawn in your terminal's own text colour, only on a
  fresh connect (never after a rebuild or a failover), and not at all on a
  screen smaller than 34x20.

- **What oxutrm does to keep a session alive is logged.** Outages and their
  end, standby searches, finds and losses, probes and failovers, rebuild
  attempts and why they failed, held input sent or dropped: each is appended
  to `$XDG_STATE_HOME/oxutrm/client.log`, or `~/.local/state/oxutrm/client.log`,
  one line each, with the time in UTC, the ssh target and the start of the
  session id. Repeats are folded into one line. The file is rotated to
  `client.log.1` before it passes 1 MiB, so the two never hold more than
  2 MiB, and two clients sharing it cannot interleave inside a line. If it
  cannot be written, the popup says so once and the session carries on.

### Compatibility

- **Both ends have to be upgraded together.** The client now runs
  `oxutrm host --connect` on the far end rather than `oxutrm host --serve`, so
  it can be offered the live sessions before choosing one. An older host does
  not know that option and exits with its own usage error, which oxutrm shows
  you as the reason it could not connect rather than reporting a network
  failure — and if it happens while a session is being rebuilt after an outage,
  oxutrm names it as a version mismatch and stops instead of retrying an answer
  that will not change. There is no protocol version bump, because the version
  field lives in the hellos and this exchange happens before them.

### Changed

- **The hellos now say what a peer can do.** `HostHello` and `ClientHello`
  carry a `features` list — empty today except for the host, which advertises
  `control` and `standby`. Absent on either side, it is read as "nothing", so
  an older peer on the other end of the exchange is unaffected and there is no
  `PROTO_VERSION` bump. Three signals, `StandbyRequest`, `Probe` and
  `ProbeAck`, are what the standby link above sends over the wire.

- **Nothing is written over the session any more.** The `standby: …`, `no
  standby path`, `switched to standby …` and `path migrated …` lines are gone:
  they were wiped by the repaint that followed them and survived only in the
  scrollback. Their content is in the popup and the log. The two lines a
  session opens with are still printed, before it takes over the screen: which
  session this is and how it was reached (new or resumed), then the path
  banner.

- **The status popup is sorted into sections.** A hand test found it too much
  text and too little structure: an eight-second failover filled six lines of
  its log. The session id is in the title now, the first line says how the
  link is reached and how long it has been up, the RTT and its sparkline share
  a row, and the standby and the log each have a section under a rule drawn
  into the border. During an outage a block right under the first line has
  one row for the standby probe and one for the ssh rebuild, each with its own
  state and clock (`ssh rebuild   attempt 1 · running 14 s`). The log shows
  one line per outage once it is over — `22:59  outage 8.1 s → switched to
  standby (IPv4 punched)` — in local time, with identical lines folded into
  `×N` and a long line wrapping under its own text. Every step of the outage
  is still in `client.log`, unchanged and in UTC, followed by that summary
  line. A standby search that finds nothing says `no second path found`; the
  file keeps the full reason, rung by rung. The box is only as tall as what it
  has to say, so a short log leaves no empty rows. The attach id, the average
  RTT and the cumulative sent/lost counts are no longer shown.

- **The box that appeared during an outage is the popup now.** What it said —
  how long the host has been silent, what was typed blind, the rebuild attempt
  and its countdown, the question about held input — is a section of the
  popup. Its keys lose the `Ctrl-\` prefix: `q` quits, `s` and `d` answer the
  question. Typing into the popup is no longer held; close it with `Esc` to
  type blind.

### Fixed

- **A standby that answers is used even while ssh is still trying.** Once an
  ssh rebuild had started, 20 s into an outage, the standby was no longer
  probed until the attempt ended, for fear that the host would adopt the
  rebuild and close the standby just switched to. An ssh stuck connecting has
  sent the host nothing it could adopt, so that fear only applies once the
  attempt has asked for the session. Until then the standby is now probed as
  usual, and when it answers the client switches to it and abandons the
  attempt, killing its ssh. A VPN drop on 2026-10-04 lasted 96 s because of
  this: the rebuild's ssh waited out a 75 s TCP connect timeout, and the
  standby answered a second after it gave up.

- **A session no longer ends just after it was rescued.** When an ssh
  rebuild failed at about the moment the client switched to its standby, or
  at about the moment the old link came back by itself, its failure was still
  acted on afterwards. A far end too old for `--connect` then ended the
  session that had just recovered ("this session cannot be resumed"), and an
  ordinary failure was logged after the attempt had already been logged as
  abandoned and pushed the next retry back. The client now only listens to
  the attempt it is still waiting for.

- **A rebuild's ssh gives up on a dead route after 10 s.** ssh has no
  connect timeout of its own by default, so a rebuild whose target could not
  be reached waited for the operating system's, which is 75 s on macOS. A
  rebuild now asks `ssh -G` what it would do and, where no `ConnectTimeout` is
  set, adds `-o ConnectTimeout=10`. A `ConnectTimeout` set in your ssh
  configuration is left alone.

- **The activity log numbers ssh attempts across the whole outage.** After a
  switch to the standby, the next ssh attempt was logged as "attempt 1" again,
  so one outage could log two different attempts both as "attempt 1". The log
  now counts on. The popup's own attempt counter still starts over with each
  new link, as does the retry schedule.

- **Programs that ask the terminal a question get an answer.** The host's
  emulator always worked out the reply to a query such as "where is the
  cursor?" (`CSI 6n`) or "what are you?" (device attributes), and then threw
  it away, so the program asking waited for nothing. atuin's arrow-up search
  gave up every time with "The cursor position could not be read within a
  normal duration". The replies now go back to the program, from the host,
  whose screen is the one being asked about.

- **A standby search's diagnostics no longer garble the screen.** Each standby
  search runs the full connection ladder, birthday blast included, so on a
  machine whose blast misses this printed over the painted raw-mode screen on
  every search during an outage. The blast's miss now carries its numbers back
  as data instead of printing them; a connect-time failure (before any session
  owns a screen) still shows them, in the rung's own failure text. A lint now
  denies `print_stderr`/`print_stdout` in the modules on a client session's
  in-session path, so this cannot come back unnoticed.

- **Reattaching no longer needs `loginctl enable-linger`.** A detached session
  outlives your login on most systems, but the directory it registered itself
  in — `/run/user/<uid>` — does not, so oxutrm used to record sessions in your
  home directory instead. On a networked home that put the session's Unix
  socket on a filesystem where Unix sockets are not reliable, and on a shared
  home it meant `--list` could show sessions belonging to another machine and
  test their process ids against the wrong computer. Sessions are now recorded
  on local storage — `/dev/shm` on Linux, `/var/tmp` elsewhere — which survives
  logout without any administrative setup, and each entry records which boot it
  belongs to, so entries left behind by an earlier boot — or, on Linux, a
  different machine — are recognised instead of believed. If something else
  has taken the directory oxutrm wants, it now says so and stops rather than
  quietly falling back to the home directory.

- **The directory oxutrm records sessions in is now private.** Whatever
  location it ends up using — including one you point `OXUTRM_STATE_DIR` at —
  is created, or corrected, to mode `0700` before anything is written into it.
  Its parent must already exist: oxutrm no longer creates intermediate
  directories on the way there, so an `OXUTRM_STATE_DIR` pointed at a path
  whose parent is missing is refused rather than silently built out.

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


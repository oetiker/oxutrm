# Changes

## [Unreleased]

### New
- **The session selector shows the selected session's screen.** Under the
  list, the last lines of the session's screen up to its cursor appear in
  its colours as you move through the list; `+ new session`, a session of
  another oxutrm version and one that cannot be reattached say why there is
  none. The preview needs a terminal of at least 38 columns and, with a
  few sessions, 15 rows, and is left out below that.

### Changed
- **Host sessions started by an older oxutrm cannot be reached by this
  one.** This release changes the protocol version: the client refuses
  such a session, and the selector lists it as `old version`. End them
  before connecting with this release.

### Fixed
- A program that printed accents or other combining marks without end, such
  as a `cat` of a file of stacked diacritics, made the memory of its session
  on the host grow until the system killed it. A character now keeps only as
  many marks as fit in what the screen shows of it, and the rest are dropped.
- A program that printed the start of a terminal control sequence and never
  ended it, such as a `cat` of a damaged file, or that set a very long window
  title and saved it over and over, made the memory of its session on the host
  grow until the system killed it. Such a sequence is now dropped once it
  passes 512 KiB, a window title once it passes 2 KiB and a hyperlink once it
  passes 4 KiB, and the output that follows shows as usual.
- **Text after a tab no longer appears shifted to the left.** When a line
  containing tabs was redrawn in place, for example while output scrolled,
  the text after each tab moved one more column to the left, and leftover
  characters from the earlier content stayed at the end of the redrawn part
  of the line until the screen was repainted in full.
- **Killing the session you were in from the session selector no longer
  freezes the screen.** If that shell had ever rung the bell or scrolled text
  off the top, the client stayed on its last picture instead of showing the
  blank lobby, and a new session started from there stayed frozen too.

## 0.3.0 - 2026-10-15
### New
- **Connecting to a host with sessions running opens a session selector
  instead of resuming one.** `oxutrm <ssh-target>` starts a new session when
  none of yours runs there; with one or more running, even exactly one, the
  selector opens over a blank screen. `oxutrm --attach <name|id> <ssh-target>`
  goes straight to one session, by its name or by four or more lowercase hex
  characters of its id, and `oxutrm --new [--name <name>] <ssh-target>` always
  starts a new one.

- **A client whose network dies reconnects by itself.** After 20 seconds of
  silence the client runs a new ssh back into the same session, at once and
  then after 2, 4 and 8 seconds and every 8 seconds after that, until `q` in
  the status popup stops it. The old link stays open meanwhile and whichever
  link answers first carries the session; an attempt that gets no answer is
  given up after two minutes and tried again, and a host that answers that
  the session is gone ends the client with a message saying so.

- **A reconnect cannot unlock an ssh key that needs a passphrase.** Reconnect
  attempts run `ssh -o BatchMode=yes`, so with such a key and no agent holding
  it they fail, and the status popup says why.

- **A reconnect gives up on an unreachable host after 10 seconds.** Its ssh
  gets `-o ConnectTimeout=10` unless your ssh configuration sets a
  `ConnectTimeout`; `recovery.connect_timeout` changes the value.

- **The client keeps a standby connection and fails over to it.** While a
  link is up, the client holds a second connection to the session from a
  different local source address, for example the open internet next to a
  split-tunnel VPN, at the cost of one keep-alive every ten seconds. When the
  primary goes silent and the standby still answers, the client switches to it
  after about three seconds, without ssh and also while an ssh reconnect is
  under way, and does not switch back; a host of an older version offers no
  standby, and `network.standby = false` turns it off.

- **On a machine with only one way out, the session's attach number climbs.**
  The client keeps searching for a standby there, with the wait between
  searches growing from thirty seconds to five minutes and a route change
  starting the next one early. Each search counts as an attach, about twelve
  an hour in `oxutrm host --list`.

- **A status popup shows what the connection is doing.** `Ctrl-\` opens it,
  and two presses within half a second send one `Ctrl-\` to the remote
  program. It shows the round-trip time with
  its range over the last minute and a sparkline, loss, throughput, the path
  MTU, which link of the session this is, the standby, during an outage the
  state of the standby probe and of the ssh reconnect, and a log of recent
  events with one line per finished outage.

- **The status popup opens by itself during an outage.** It opens two seconds
  into an outage and shows `● LIVE again` for three seconds once the link is
  back; `popup.auto_open_after` and `popup.linger` change these times.

- **While the status popup is open, it takes every key.** `Esc` or `Ctrl-\`
  closes it, `q` quits, `c` opens the config screen and `s` the session
  selector.

- **`s` in the status popup opens the session selector.** It lists the host's
  sessions with name, shell, start time, size and a mark (`this`, `in use`,
  `?`, `old version`); `⏎` switches, `n` starts a new session, `r` renames and
  `x` kills, and a session that tunnels through its ssh connection or comes
  from another oxutrm version is dimmed and cannot be chosen. A switch travels
  over the live link without ssh, and a switch that fails leaves you where you
  were, with the reason under the list.

- **A session can be taken over from another terminal.** `oxutrm --attach` or
  the selector hands the running shell, with its current screen, to the new
  terminal; the selector asks `y/n` first for a session marked `in use` or `?`.
  The terminal that had the session says it was taken over and exits, also
  when it had just recovered from an outage.

- **Killing a session from the selector.** `x` asks `kill <name>? y/n`, hangs
  up the session's shell and kills what is left of it three seconds later.
  Killing the session you are in leaves the selector open over a blank screen
  until you pick another session, start one, or quit with `q`; only a shell
  that exits ends the client.

- **Sessions can have names.** A name is 1 to 24 characters with at least one
  outside `0-9a-f`, and unique among the sessions of a host. Set it with
  `--new --name` or with `r` in the selector; `oxutrm host --list` shows it in
  a column after the id.

- **Connecting says which session you landed in.** Before the terminal goes
  into raw mode, oxutrm prints `oxutrm: resumed session <id>.`,
  `oxutrm: new session <name> (<id>).`, or how many sessions are running when
  the selector is about to open.

- **A new connection opens with the oxutrm logo.** About 0.8 s of TV snow
  settles into the logo, held for 1.5 s, with the session and the path under
  it; any key ends it and still reaches the remote program. It appears only
  on a fresh connect on a screen of at least 34x20, stays until the host's
  first screen arrives, and `popup.splash = false` turns it off.

- **What oxutrm does to keep a session alive is logged.** Outages, standby
  searches and failovers, reconnect attempts and why they failed, held input
  sent or dropped, and every switch, kill and rename go to
  `$XDG_STATE_HOME/oxutrm/client.log` (default
  `~/.local/state/oxutrm/client.log`), one line each with the UTC time, the
  ssh target and the start of the session id. The file is rotated to
  `client.log.1` at 1 MiB, and if it cannot be written the popup says so once.

- **A config file sets the popup, recovery and network behaviour.**
  `~/.config/oxutrm/config.toml` (or under `$XDG_CONFIG_HOME`) takes
  `popup.*`, `recovery.*` and `network.*` keys, and a `[host."<target>"]` table
  overrides them for one ssh target; `docs/config.md` lists every key. A broken
  file or a bad value is a warning in the popup's log and in `client.log`,
  never a failed connect, and the splash counts the warnings.

- **`c` in the status popup opens a config screen.** It shows every setting
  with its value and where it came from and applies an edit at once where it
  can; `x` resets a setting to the next layer, and `w` saves for all hosts
  (`a`) or for this host only (`h`). Saving changes only the keys you changed
  and keeps comments, hand edits and a symlinked file as they were.

- **oxutrm has a man page.** `man oxutrm` describes every option, the keys
  of the status popup, the session selector and the config screen, the
  config file, and the files and environment variables oxutrm uses. The same
  text is `docs/manual.md` in the repository.

- **oxutrm runs on macOS, from release archives or Homebrew.** Each release
  carries archives for Intel and Apple silicon Macs. `brew tap oetiker/oxutrm
  https://github.com/oetiker/oxutrm` and `brew install oxutrm` install oxutrm
  on macOS and on Linux.

- **oxutrm installs and upgrades from apt and dnf repositories.** The `.deb`
  and `.rpm` packages are published at `https://gitea.oetiker.ch/oposs`, so
  `apt upgrade` and `dnf upgrade` pick up new releases; the README shows the
  setup. The packages stay attached to each GitHub release as well.

### Compatibility
- **Upgrade the client and every host together, and end old sessions first.**
  The client refuses a session started by an older oxutrm, with both version
  numbers in the message, and the selector lists it as `old version` until its
  shell ends. Against a host too old for this client, a connect fails with
  that host's usage error, and a reconnect stops with a version-mismatch
  message instead of retrying.

- **Against an older host, `network.birthday = false` stops only the client's
  half of the birthday punch.** An upgraded host stops its half too. Behind a
  symmetric NAT, turning it off makes a connect that cannot punch through fail.

### Changed
- **The release archives contain the man page instead of `CHANGES.md`.**
  Each archive holds `oxutrm`, `man/oxutrm.1`, `README.md` and `LICENSE`. The
  `.deb` and `.rpm` packages install the man page too, and still put
  `CHANGES.md` in `/usr/share/doc/oxutrm`.

- **New shells are login shells started in your home directory**, as ssh
  starts them: your profile is read, and the shell starts in `$HOME` instead
  of `/`. This holds for a first connect and for sessions started from the
  selector.

- **The outage box is now part of the status popup.** Its keys no longer take
  the `Ctrl-\` prefix: `q` quits, and `s` and `d` answer the question about
  input typed during the outage, which ignores keys for its first half second.
  Keys typed into the open popup are not held; close it with `Esc` to type
  blind, and it stays closed until the outage ends.

- **Nothing is written over the session any more.** The `path migrated …` line
  and its kind are gone; their content is in the popup and in `client.log`.
  The two lines printed before the session takes over the screen stay: which
  session this is, and the path it took.

- **`oxutrm host --list` never shows a half-written session entry.**

### Fixed
- **`q` no longer says "oxutrm: the shell exited (0)."** The shell keeps
  running after `q` in the status popup, so the client now says that the
  session detached and shows the `oxutrm --attach` command that resumes it.

- **The client gives the screen back as it found it.** After `q`, a shell
  that exited or an error, the remote screen and the status popup stayed on
  the local terminal and the local prompt was drawn over them. The client now
  draws on the terminal's alternate screen, so what was there before the
  session reappears when it ends.

- **A session left with `q` is no longer `in use` for 30 seconds.**
  Connecting again right after `q` listed the session as `in use` and asked
  `take over <id> from its other client? y/n`. The host now learns of the
  quit at once, so the selector lists the session as detached and resumes it
  without the question.

- **A session whose network came back could stay dead for minutes.** After a
  long outage the connection waited on a retry timer that had grown into the
  minutes, although both ends and the path were fine. A 150-second outage now
  recovers in about a second instead of 107 seconds.

- **Programs that ask the terminal a question get an answer.** Cursor position
  (`CSI 6n`) and device attribute queries went unanswered, so for example
  atuin's arrow-up search failed with "The cursor position could not be read
  within a normal duration". The host now answers them.

- **Direct IPv6 connections work.** Every session fell back to IPv4 while still
  offering its IPv6 addresses, so the `IPv6 direct` path never succeeded.

- **Reattaching no longer needs `loginctl enable-linger`.** Sessions are
  recorded on local storage, `/dev/shm` on Linux and `/var/tmp` elsewhere,
  instead of in the home directory, where a networked or shared home made
  their sockets unreliable and `oxutrm host --list` could show another
  machine's sessions. Entries left by an earlier boot, or on Linux by another
  machine, are recognised as stale.

- **oxutrm stops when its session directory is taken.** If something else
  holds the directory oxutrm wants, it says so and stops instead of falling
  back to the home directory.

- **The session directory is private.** Whatever directory oxutrm uses,
  including one named by `OXUTRM_STATE_DIR`, is set to mode `0700` before
  anything is written into it. Its parent must exist; oxutrm no longer creates
  missing parent directories.

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


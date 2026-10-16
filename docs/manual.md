---
title: OXUTRM
section: 1
header: oxutrm manual
footer: oxutrm
date: 2026-10-14
---

# NAME

oxutrm - remote terminal that survives bad networks, address changes and NAT on both ends

# SYNOPSIS

| **oxutrm** [**\--attach** *name*|*id*] *ssh-target*
| **oxutrm** **\--new** [**\--name** *name*] *ssh-target*
| **oxutrm host** **\--connect**
| **oxutrm host** **\--serve** [**\--name** *name*]
| **oxutrm host** **\--list**
| **oxutrm host** **\--attach** *id*
| **oxutrm loopback** [*shell*]
| **oxutrm** **-h** | **\--help** | **help**
| **oxutrm** **-V** | **\--version**

# CONFIGURATION

The client reads `$XDG_CONFIG_HOME/oxutrm/config.toml`,
else `~/.config/oxutrm/config.toml`, when it connects.
A missing file means the built-in defaults.
A file that does not parse, an unknown key or a value out of range never stops a connect:
each problem is a warning in the status popup's log and in `client.log`,
and the setting falls back to the next layer down.

Every setting resolves through its built-in default,
then the top-level section,
then a `[host."<target>"]` table, where `<target>` is the ssh target exactly as typed on the command line.
Durations are strings such as `"20s"`, `"1m"` or `"1m 30s"`.

- `popup.key = "ctrl-\\"`: The key that opens the status popup.
  Accepts `ctrl-a` to `ctrl-z` except `ctrl-h`, `ctrl-i`, `ctrl-j` and `ctrl-m`;
  `ctrl-\`, `ctrl-]`, `ctrl-^`, `ctrl-_`; or `off`.
  With `off` the popup opens only by itself during an outage.
  `off` together with `popup.auto_open_after = "off"` is refused.
- `popup.auto_open_after = "2s"`: Silence on the link before the popup opens by itself, 0s to 10m, or `off`.
  A value below `recovery.silent_after` is raised to it.
- `popup.linger = "3s"`: How long the popup stays open after the link is back, 0s to 1m.
- `popup.splash = true`: Show the startup splash. Takes effect at the next connect.
- `recovery.silent_after = "2s"`: Silence before the link counts as an outage, 1s to 1m.
  From then on, typed input is held and the standby is probed.
- `recovery.rebuild_after = "20s"`: Silence before an ssh rebuild starts, 5s to 10m.
  A value below `recovery.silent_after` is raised to it.
- `recovery.connect_timeout = "10s"`: The ssh `ConnectTimeout` of a rebuild, 1s to 2m, in whole seconds.
  Added only where `ssh -G` reports no `ConnectTimeout` of its own.
  Takes effect at the next rebuild attempt.
- `network.standby = true`: Keep a second path to the session ready to fail over to.
  On a host that did not offer a standby, turning it on waits for the next connect.
- `network.stun_servers = [...]`: Up to 16 STUN servers as `host:port` or `[v6]:port`.
  The default is `stun.cloudflare.com:3478`, `stun.l.google.com:19302`, `stun.nextcloud.com:443` and `stun.sipgate.net:3478`.
  An empty list is allowed; then no public address is learned.
  Takes effect at the next rebuild attempt or standby search.
- `network.port_mapping = true`: Ask the router for a port mapping through UPnP, NAT-PMP or PCP.
  Takes effect at the next rebuild attempt or standby search.
- `network.birthday = true`: Use the birthday-paradox NAT punch.
  `false` also asks the host not to send its half; a host older than the client ignores the request.
  Behind a symmetric NAT, `false` makes a connect that cannot punch through fail.
  Takes effect at the next rebuild attempt or standby search.
- `terminal.clipboard = true`: Pass a remote program's clipboard copy (OSC 52) to the local terminal,
  which may still ask or refuse by its own settings.
  Copies over 256 KiB are dropped.
  A remote program can never read the local clipboard.
- `terminal.hyperlinks = true`: Show a remote program's hyperlinks (OSC 8) as links in the local terminal.
  `file://` links name the remote host and are passed on unchanged.
  A link whose URI is longer than 2083 bytes or holds anything but printable ASCII is shown as plain text.

Settings without a note take effect at once.
The config screen (see **KEYS**) changes a setting for the running session and saves it to this file,
keeping the rest of the file as written.
A symlinked file stays a symlink.
`docs/config.md` in the source repository lists the same keys as a table.

Example:

```toml
[popup]
key = "ctrl-]"

[recovery]
rebuild_after = "30s"

[host."thinlinc"]
network.standby = false
```

# DESCRIPTION

**oxutrm** runs a shell on a remote machine and keeps the session across network outages,
changes of IP address on either end and NAT on both ends.
The host keeps the authoritative screen and sends the client the differences.
The connection is QUIC over UDP.
ssh starts a session and rebuilds a lost link; it carries no terminal data unless every UDP path fails.

The same binary runs on both machines.
**oxutrm** *ssh-target* runs `ssh <ssh-target> oxutrm host --connect`,
so **oxutrm** must be on a `PATH` that a non-interactive ssh to *ssh-target* searches.
**oxutrm** does not read `~/.ssh/config`;
whatever makes `ssh <ssh-target>` work also serves **oxutrm**.

## Connecting

With no session of the user running on the host, **oxutrm** *ssh-target* starts a new one.
With one or more running, it opens the session selector over a blank screen;
it never resumes a session without asking.
**\--attach** goes to one session directly and **\--new** always starts a new one.

Before the terminal goes into raw mode, the client prints one line:
which session it resumed, the name and id of the new session,
or how many sessions are running when the selector is about to open.
The path the link took follows in a second line,
for example `oxutrm  IPv6 direct  ·  11 ms  ·  mtu 1452`.

The host listens on UDP port 443 where it may, else on another port.
The client and the host then try these paths in order, and the first that validates carries the session:

1. IPv6 direct, when both ends have a global IPv6 address.
2. A port mapping on the router, through NAT-PMP, PCP or UPnP-IGD.
3. STUN and a UDP hole punch.
4. A birthday-paradox punch, for symmetric NAT.
5. A tunnel inside the ssh connection, when no UDP path works.

A session on the ssh tunnel is reported as a warning.
It ends with its ssh connection and cannot be reattached.

## Outages and recovery

After `recovery.silent_after` of silence (2 s) the link counts as an outage.
Typed input is held, the status popup opens after `popup.auto_open_after`,
and the client probes its standby link if it has one.
When the standby answers and the primary does not, the client switches to the standby, without ssh.
It does not switch back.

After `recovery.rebuild_after` of silence (20 s) the client rebuilds the link:
a new `ssh -o BatchMode=yes <ssh-target> oxutrm host --connect`, back into the same session.
The first attempt starts at once.
After a failure the next waits 2 s, then 4 s, then 8 s, and every 8 s after that, without limit.
An attempt that gets no answer is abandoned after two minutes and tried again.
The old link stays open throughout, and whichever link answers first carries the session.
A host that reports the session as gone ends the client.
A key that needs a passphrase cannot be unlocked during a rebuild,
so such an attempt fails and the popup shows why.

When the host answers again with input held, the popup asks whether to send it (`s`) or drop it (`d`).

While a link is up, the client looks for a standby:
a second connection to the same session from a different local source address.
On a machine with a single way out it finds none, and repeats the search
at intervals that grow from 30 seconds to 5 minutes; a route change starts the next search early.
Each search is an attach, so the session's attach number in **host \--list** grows.

## Status popup

The popup key (`Ctrl-\` by default) opens the status popup.
Two presses within half a second send one literal popup key to the remote program.
The popup shows the round-trip time and its range over the last minute,
loss, throughput, the path MTU, which link of the session this is,
the standby, during an outage the state of the standby probe and of the ssh rebuild,
and a log of the last things **oxutrm** did.
While the popup is open, every key belongs to it.

## Sessions

A host can run several sessions, each one process with one shell.
**host \--list** lists them.
Every new shell is a login shell (`argv[0]` starts with `-`) taken from `$SHELL`, started in the home directory.

A session has an id of 32 lowercase hexadecimal characters and an optional name.
A name is 1 to 24 printable characters, with no space at either end,
and has at least one character outside `0-9a-f`.
Names are unique among the live sessions of a host.
**\--attach** tries the name first, then the id prefix, which needs at least four characters and is case-sensitive.
A prefix that matches several sessions is refused with the list of the sessions it matches.

A session whose client is gone detaches 30 seconds later and keeps running.
A client that quits with `q` detaches its session at once.
Attaching to a session that another client holds takes it over;
the other client reports that the session was taken over and exits.

The client and every host session speak one protocol version.
The client refuses a session started by a different version of **oxutrm**,
and the selector marks it `old version`.

## Session selector

The selector opens by itself at connect when sessions are running,
and from the status popup with `s` while the link is up.
Each row shows a session's name or the start of its id, its shell, its start time
(the time today, the date before), its size, and a mark:
`this` for the session of this client, `in use` for one attached to another client,
`?` for one that did not answer in time, `old version` for one of another version.
A session that cannot be reattached and one of another version are dimmed,
and choosing one shows why instead of switching.
Rows are in order of start, oldest first, followed by `+ new session`.

Where the terminal has room, a pane under the list shows the selected session's screen:
its leftmost columns and the rows ending at its cursor, or its top rows when the cursor is hidden,
in its own colours and at most 68 columns by 18 rows.
The screen is fetched when the selection arrives on a row and again on a return after 2 seconds,
not while the selection rests, and is forgotten when the selector closes.
The row of this client's session shows its own screen.
A row with no screen to show, such as `+ new session`, says why in the pane.
The pane needs a terminal of at least 38 columns, and with three sessions 15 rows;
on a smaller one the selector has no pane.

A switch uses the live link and no ssh.
The new link is built while the client stays in the old session,
and a switch that fails leaves the client where it was, with the reason under the list.
The session left behind detaches and stays listed.

When the selector opens at connect, or after the session of this client is killed,
the client is in a lobby: a session process with no shell and a blank screen, with the selector always open.
Starting a new session in a lobby turns the lobby into that session.
A lobby whose client is gone, or silent for 30 seconds, ends.

Killing a session hangs up its shell and kills what remains three seconds later.
Killing the session of this client leaves the selector open in a lobby.
Killing never ends the client; a shell that exits does.

# OPTIONS

## Client

**\--attach**, **\--new** and **\--name** come before *ssh-target*, in any order.

- `--attach <name|id>`: Attach to the session with this name, or whose id starts with this prefix.
  Refused when no session matches.
  Cannot be combined with **\--new**.
- `--new`: Start a new session, also when sessions are running.
- `--name <name>`: Name the new session. Requires **\--new**.
  A name that is taken is refused before the session starts.
- `-h, --help, help`: Print a usage summary and exit.
- `-V, --version`: Print the version and exit.

## host

**oxutrm host** is the remote half.
The client starts it over ssh; it is not normally typed by hand.

- `--connect`: Offer the running sessions to the client on standard output,
  read its choice from standard input, then serve a new session or attach to a running one.
  This is the command the client runs over ssh.
- `--serve [--name <name>]`: Create a session, optionally named, and hand it to the client on standard input and output.
- `--list`: List the sessions on this machine, oldest first, and remove entries whose process is gone.
  Each line shows the id, the name (`-` for none), the process id, the size, the attach number,
  the shell, and whether the session is detachable.
- `--attach <id>`: Relay a client on standard input and output into the running session with this id.
- `-h, --help`: Print a usage summary of **oxutrm host** and exit.

## loopback

**oxutrm loopback** runs the host and the client in one process with no network between them:
a shell on a pty, the terminal emulator, the screen differences encoded and decoded, and the renderer.
It needs a terminal.
It does not pass a remote program's clipboard copies (OSC 52) to the local terminal.

- `shell`: The program to run. The default is `$SHELL`, else `/bin/sh`.
- `-h, --help`: Print a usage summary of **oxutrm loopback** and exit.

# KEYS

## Status popup

- `Esc, popup key`: Close the popup.
  Closed during an outage, it stays closed until the outage ends.
- `q`: Quit the client. The session detaches and keeps running; a lobby ends.
- `c`: Open the config screen.
- `s`: Open the session selector. Only while the link is up.

When the popup asks about held input:

- `s`: Send the input typed during the outage.
- `d`: Drop it.

Keys pressed in the first half second after the question appears do nothing.

## Session selector

- `Up, k`: Move up.
- `Down, j`: Move down.
- `Enter`: Switch to the session under the cursor.
  On `+ new session`, start a new session; on `this`, close the popup.
- `n`: Start a new session.
- `r`: Name or rename the session in place.
  `Enter` saves, `Esc` cancels, and an empty name removes the name.
- `x`: Kill the session, after a `y/n` question.
- `q, Esc`: Back to the status popup, or quit the client when there is no session to go back to.

A session that is `in use` or `?` is taken over only after a `y/n` question.
`y` does nothing during the first half second of a question.
While the link is down the selector stays open, says so in its header, and refuses every action.

## Config screen

- `Up, k`: Move up.
- `Down, j`: Move down.
- `Enter`: Edit the setting.
  In a text field `Enter` accepts and `Esc` cancels.
  For `popup.key`, the next key pressed becomes the popup key, and `Backspace` sets it to `off`.
- `x`: Reset the setting, so that the next layer down applies.
- `w`: Save, then `a` for all hosts or `h` for this host only. `Esc` cancels.
- `Esc`: Back to the status popup. Changes stay applied.
- `q`: Quit the client.
- `popup key`: Close the popup.

In the list of STUN servers, `+` adds a server, `-` removes the one under the cursor,
`Enter` edits it and `Esc` returns to the settings.

# EXIT STATUS

The client exits with the exit status of the remote shell when the shell ends,
and with 0 when `q` detaches it from a session.

- `0`: Success, also after `q`.
- `1`: The session could not be started or reached,
  or the link ended without the shell exiting, for example because another client took the session over.
- `2`: A usage error, such as an unknown option, **\--name** without **\--new**,
  or an **\--attach** that matches no session.

# ENVIRONMENT

- `XDG_CONFIG_HOME`: Base directory of the config file. The default is `~/.config`.
- `XDG_STATE_HOME`: Base directory of `client.log`. The default is `~/.local/state`.
- `HOME`: Used for the defaults above and as the last place to record sessions.
- `XDG_RUNTIME_DIR`: On the host, the first place to record sessions, used when lingering is enabled for the user.
- `OXUTRM_STATE_DIR`: On the host, the directory to record sessions in, in place of the automatic choice.
  Its parent must exist.
- `SHELL`: The shell of a new session, and the default program of **oxutrm loopback**. The default is `/bin/sh`.
- `TERM, COLORTERM`: Decide the colours and features the client terminal is assumed to support.
  `COLORTERM=truecolor` or `24bit` enables 24-bit colour.

# FILES

- `~/.config/oxutrm/config.toml`: The client configuration. See **CONFIGURATION**.
- `~/.local/state/oxutrm/client.log`: What the client did to keep sessions alive:
  outages, standby searches, failovers, rebuild attempts, held input sent or dropped, switches, kills and renames.
  One line per event, in UTC, with the ssh target and the start of the session id.
  It is rotated to `client.log.1` before it reaches 1 MiB.
- `$XDG_RUNTIME_DIR/oxutrm/`: The session registry on the host, when lingering is enabled for the user.
- `/dev/shm/oxutrm-<uid>/oxutrm/`: The session registry otherwise, where `/dev/shm` exists.
- `/var/tmp/oxutrm-<uid>/oxutrm/`: The session registry when neither of the above is usable.
- `~/.local/state/oxutrm/`: The session registry when no local directory is usable.
  **host \--list** then prints a warning.

The host uses the first registry directory that is usable and sets its mode to `0700`.
When something else has taken a directory, **oxutrm** stops rather than use the next one.
`OXUTRM_STATE_DIR` replaces the whole choice.
In the registry, each session has a directory named by its id,
holding `meta.json` and the Unix socket `sock`.

# BUGS

- A rebuild cannot ask for an ssh key passphrase; it needs an agent or a key without one.
- A session taken over by another client is reported as
  "the host closed the session without the shell exiting".
- Scrollback is kept on the host but not sent to the client.
- There is no local echo; typed characters appear after the round trip.
- Bandwidth adaptation is a fixed pacing interval of half the round-trip time, between 8 ms and 100 ms.
- Blinking text loses its blink after a resize.
- Double, curly, dotted and dashed underlines are drawn as a plain underline.
- The icon name (OSC 1) is ignored.
- Text pushed into scrollback by narrowing the window does not return when it is widened again.
- A path found after connect is used only from the next attach.
- A standby may use the same physical link as the primary under a different source address,
  and then fails together with it.
- The network tests put only one end behind NAT.
- Sixel, Kitty and iTerm2 inline images are not supported.

# EXAMPLES

Connect, and choose from the running sessions:

```sh
oxutrm build.example.org
```

Start a new session called `logs`, then return to it later:

```sh
oxutrm --new --name logs build.example.org
oxutrm --attach logs build.example.org
```

Check that the far end can find **oxutrm**:

```sh
ssh build.example.org command -v oxutrm
```

# AUTHORS

Tobias Oetiker <tobi@oetiker.ch>,
Claude (Anthropic).

# REPORTING BUGS

<https://github.com/oetiker/oxutrm/issues>

# COPYRIGHT

Copyright (c) 2026 Tobias Oetiker.
Released under the Apache License 2.0, see
<https://github.com/oetiker/oxutrm/blob/main/LICENSE>.

# SEE ALSO

Project home: <https://github.com/oetiker/oxutrm>

**ssh**(1),
**ssh_config**(5),
**mosh**(1),
**tmux**(1)

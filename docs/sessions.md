# Sessions and the session selector

A host can run several oxutrm sessions at once: each is one process with one
shell, listed by `oxutrm host --list`. The **session selector** is how you move
between them from the client.

## Connecting

| You type | With no session running | With sessions running |
|---|---|---|
| `oxutrm <target>` | a new session | the selector, over a blank screen |
| `oxutrm --new [--name <name>] <target>` | a new session | a new session |
| `oxutrm --attach <name\|id> <target>` | refused: nothing to attach to | that session |

Even exactly one running session opens the selector: nothing is resumed
silently. `--attach` takes a session's exact name, or at least four
characters of its id; ids are lowercase hex and a prefix is matched exactly as
written, so `3FF1` finds nothing where `3ff1` does. A name always contains a
character outside `0-9a-f`, so a name and an id prefix can never be confused.
`--name` without `--new` is refused.

## The selector

`s` in the status popup opens it on a live link (it is dimmed during an
outage); after a bare connect to a host with sessions it opens by itself once
the splash is down.

```
╭ sessions on thinlinc ─────────────────────╮
│   a3f9c01e  fish  Oct 05  80×24   in use  │
│ ▸ build     bash  09:14   120×40  this    │
│   logs      zsh   11:02   120×40          │
│   + new session                           │
├───────────────────────────────────────────┤
│ ⏎ switch  n new  r rename  x kill  q back │
╰───────────────────────────────────────────╯
```

Each row is a session: its name (or the start of its id), its shell, when it
started (the time today, the date before), its size, and a mark -- `this` for
the session you are in, `in use` for one attached to another client, `?` for
one that did not answer in time, `old version` for one started by another
version of oxutrm. A session that cannot be reattached (it tunnels its data
through the ssh connection that created it) and one of another version are
dimmed, and `⏎` on them says why instead of switching.

| Key | Does |
|---|---|
| `↑` `↓` (`k` `j`) | move |
| `⏎` | switch to that session; on `+ new session`, start one; on `this`, close |
| `n` | start a new session |
| `r` | name or rename the session in place: `⏎` saves, `Esc` cancels, an empty name clears it |
| `x` | kill the session, after `kill build? y/n` |
| `q` `Esc` | back to the popup -- or, with no session to go back to, quit |

Switching to a session that is `in use` asks first, then takes it over: its
other client is told it was taken over, as with `--attach`. A question's
`y` does nothing for the first half second, so typing already in flight
cannot answer it.

A switch travels over the live link -- never over ssh. The new session's link
is built while you stay in the old one, and only once it is up does the client
move; a switch that fails leaves you where you were, with the reason under the
list and in the popup's log. The session you left is detached, as if your
client had gone away, and is still listed.

Killing a session hangs its shell up the way a closing terminal does, and
kills what is left of it three seconds later. Killing the session you are in
keeps the selector open over a blank screen -- with only `+ new session` if it
was the last one -- until you pick another, start one, or quit. Killing never
ends the client by itself; a shell that exits on its own (`exit`, a crash)
still does.

While the link is down the selector stays open, says so in its header, and
refuses its actions until the link is back.

## Names

A name is 1 to 24 characters, printable, with no space at either end, and at
least one character outside `0-9a-f`. Names are unique among a host's live
sessions. Set one with `--new --name`, or with `r` in the selector.

## New shells

Every new shell -- from a first connect, from the selector, after a kill -- is
a login shell (`-bash`) started in your home directory, as ssh would start it.

## Versions

The client and every host session speak one protocol version. After an
upgrade, end the sessions an older binary started before connecting with the
new client: it refuses them, and the selector lists them as `old version`.

# oxutrm — Config file and config screen

Status: draft 2026-10-06, design agreed with the user in chat (three sections);
revised the same day after two pushback reviews (§1.1 last seven rows,
§2.2–§2.4, §4.1–§4.4, §5, §6).
Sub-project **C** of four: A (no diagnostics on the screen, merged), B (status
popup, merged), **C (this)**, D (session switcher). Builds on B
(`docs/superpowers/specs/2026-10-03-status-popup-design.md`), whose key bar
already carries a greyed-out `c config` entry for this.

---

## 1. Purpose

Everything the client decides about timing, keys and network features is a
constant today. Some of them are wrong for some networks (a 20 s wait before an
ssh rebuild is long on a flaky VPN, the birthday blast is unwelcome on some
corporate networks), and some are matters of taste (the popup key, the splash).

C gives the client one config file and one screen to change it from inside a
running session. A change applies to the session at once where it can, and can
be saved — for every host or for this one host only — without losing anything
written in the file by hand.

**Success:** a user on a network where the standby is useless turns it off for
that host from inside the session, presses `w`, `h`, and never sees it again on
that host; a missing or broken config file never stops a connect.

### 1.1 Decided with the user

| Question | Decision |
|---|---|
| What is configurable | Popup and splash, recovery timings, network features, and per-host overrides of any of them. |
| What the screen does | Browse, edit, apply live, save to the file globally or for this host. The file stays the source of truth; hand edits keep working. |
| Architecture | A settings **table**: one row per setting drives loading, layering, validation, the screen and the docs (approach 1 of 3; per-setting hand-written code and a separate `oxutrm config` command were rejected). |
| Host matching | Exact ssh target as typed. No patterns — ssh's own config already does aliases. |
| Duration syntax | Strings (`"20s"`, `"1m"`), parsed with jiff. |
| Unsaved edits on quit | Dropped silently; they were only ever for this session. |
| Where config warnings survive (review #1) | As shown entries in the popup's activity log, plus `config: N warnings` in the splash caption and the config screen header; and in `client.log`. |
| Saving to all hosts over a host override (review #2) | `w` → `a` also removes this target's override of the same key, so the saved value takes effect here. |
| `config.toml` is a symlink (review #3) | Follow it: write beside the real target and rename there. |
| `network.birthday = false` (review #8) | Tells the host too, through a `ClientHello` feature, so neither end blasts. |
| An outage while a field is open (review #9) | The config screen stays, with the outage in its header. |
| `x` on a global value, saved with `h` (2nd review #3) | Becomes a host override set to the built-in default; the global key is untouched. "This host only" never changes another host. |
| The screen after a save (2nd review #2) | The text written is re-resolved for display only (origins, layers, warning count); the settings in effect are not re-applied, so other hand edits still wait for the next connect. |

### 1.2 Not in C

- The session switcher (D); `s sessions` stays greyed out.
- Host-side settings. The host reads no config file. (The one thing the host
  learns is `no-birthday`, from the client, per connection — §2.4.)
- Watching the file for changes. The file is read at connect and re-read
  before every save; a hand edit made during a session applies at the next
  connect.
- Patterns or wildcards in host keys.
- The remaining hard-coded constants (`HEARTBEAT_IDLE`, `PROBE_*`,
  `FAILOVER_GRACE`, `STANDBY_DELAY`, birthday socket counts, `prefer_port`,
  `gather_timeout`, `DOUBLE_PRESS`, `ANSWER_GUARD`, splash timings). A row in
  the table makes any of them a setting later.

---

## 2. The file

`$XDG_CONFIG_HOME/oxutrm/config.toml`, else `~/.config/oxutrm/config.toml`.
Located by a pure `config_path(xdg, home)` written like `activity::state_path`
(relative or empty `XDG_CONFIG_HOME` ignored, per XDG). Code that reads or
writes the file takes its directory as a parameter, so every test runs on a
temp dir. A missing file is all defaults and no warning.

```toml
[popup]
key = "ctrl-\\"           # a ctrl-key (not h i j m), or "off"
auto_open_after = "2s"    # since the last frame heard; never before silent_after; "off" = never
linger = "3s"
splash = true

[recovery]
silent_after = "2s"       # silence before it is an outage: input held, standby probed
rebuild_after = "20s"     # silence before an ssh rebuild starts
connect_timeout = "10s"   # whole seconds; added to the rebuild ssh only where `ssh -G` has none

[network]
standby = true
stun_servers = [
  "stun.cloudflare.com:3478",
  "stun.l.google.com:19302",
  "stun.nextcloud.com:443",
  "stun.sipgate.net:3478",
]
port_mapping = true       # UPnP / NAT-PMP / PCP
birthday = true           # the birthday-paradox NAT punch, at both ends

[host."thinlinc"]         # any subset of the sections above
network.standby = false
recovery.rebuild_after = "30s"
```

### 2.1 Settings

Defaults are today's constants, so a missing file behaves exactly as the
client does now.

| Key | Kind | Default | Range | Takes effect |
|---|---|---|---|---|
| `popup.key` | key | `ctrl-\` | ctrl-a … ctrl-z except h, i, j, m (they are Backspace, Tab, LF, Enter); ctrl-\, ctrl-], ctrl-^, ctrl-_; or `off` | now |
| `popup.auto_open_after` | duration or `off` | 2 s | 0 – 10 min (effective value never below `silent_after`, §2.2) | now |
| `popup.linger` | duration | 3 s | 0 – 1 min | now |
| `popup.splash` | bool | true | | next connect |
| `recovery.silent_after` | duration | 2 s | 1 s – 1 min | now |
| `recovery.rebuild_after` | duration | 20 s | 5 s – 10 min (effective value never below `silent_after`, §2.2) | now |
| `recovery.connect_timeout` | duration, whole seconds | 10 s | 1 s – 2 min | next attempt |
| `network.standby` | bool | true | | now (see §2.3) |
| `network.stun_servers` | list of `host:port` or `[v6]:port` | the four above | 0–16 entries | next attempt / search |
| `network.port_mapping` | bool | true | | next attempt / search |
| `network.birthday` | bool | true | | next attempt / search (both ends, §2.4) |

"Next attempt / search" means the next ssh rebuild attempt or standby search;
the first connect always uses the loaded values.

### 2.2 Layering and validation

Each key resolves through **built-in default → top-level section →
`[host."<target>"]`**, where `<target>` is the ssh target exactly as typed on
the command line. The resolver keeps **every layer's value** per key —
`Layers { default, global: Option<_>, host: Option<_> }` — not only the
winner: the screen shows the origin (`Default`, `Global`, `Host`), and `x`
must be able to show the next layer through without re-reading the file.
Keys may be written as dotted keys, as subtables (`[host."t".network]`) or as
inline tables; all three resolve alike.

- A value of the wrong type or outside its range is a warning, and that key
  falls back to the next layer down.
- An unknown key or section is a warning (typos must not be silent).
- A file that is not valid TOML is one warning, and everything is default.
- `auto_open_after` and `rebuild_after` are stored and saved as written.
  Only their **effective** values are `max(value, silent_after)`, recomputed
  in `apply` whenever either changes (the popup cannot open, and a rebuild
  should not start, before there is an outage). The screen marks a raised
  value: `1s (2s: silent_after)`.
- `popup.key = "off"` leaves the popup reachable only by auto-open; the
  config screen is then reachable only during an outage. Allowed, documented.
  **`popup.key = "off"` together with `auto_open_after = "off"`** would leave
  the popup unreachable: in the file it is a warning and `auto_open_after`
  falls back to the next layer down (ultimately its default). In the screen
  the check is on the **effective result of any pending change** — an edit,
  an `x`, or a global save that removes a host override — and a change that
  would produce it is refused.
- `connect_timeout` with a fraction of a second (`1.5s`) is out of range:
  ssh's `ConnectTimeout` takes whole seconds.

**Warnings** cannot be printed: anything written before raw mode is cleared
by the first paint within milliseconds. Each warning is instead recorded as a
**shown** activity entry (it appears in the popup's log and in `client.log`),
and the splash caption and the config screen header carry
`config: N warnings` while there are any. Once a session owns the screen,
nothing prints (the in-session `deny(print_stderr)` rule holds for every
module C touches).

### 2.3 Standby on and off

A `Standby` is created whenever the **host offers one** at connect
(`src/connect.rs` ~227), whatever the setting says; `network.standby` only
sets its **`enabled` flag**. So "can standby be turned on here" is simply
`standby.is_some()`; on a host that offered none, the screen shows the
setting as `next connect`. The `Standby` is never dropped and rebuilt, so its
search counter carries on.

The search and probe tasks and the watched standby connection are locals of
the run loop (`src/session.rs` ~2324, 2560–2600), out of `apply`'s reach, so
the **loop** applies an edit that turns standby off:

1. aborts a search or probe task in flight and drops the watched connection;
2. disables the `Standby`, which does what `new_primary()` does
   (`searching = false`, `search += 1`) and also sets `probing = false` and
   `probe = Idle` — an aborted probe never reports, and a `probing` left
   `true` would stop every later probe (`src/standby.rs` ~208);
3. closes the parked standby.

While disabled, `step()` returns `Nothing`, and `found()` **closes** any link
it is handed (a `Found` may already sit in the two-deep events channel when
the task is aborted; the bumped `search` makes it stale, and stale is
closed). Turning it back on sets `next_search = now + STANDBY_DELAY`, as
after a failover.

### 2.4 Birthday at both ends

The client's `enable_birthday` only stops its own half of the blast; the host
builds `NetConfig::default()` and would still spray guessed ports at the
client's NAT. With `network.birthday = false` the client adds the feature
`no-birthday` (a new `oxutrm_proto::FEATURE_NO_BIRTHDAY`) to its
`ClientHello.features`, and a host that sees it clears `enable_birthday` for
that exchange — first connect, rebuild attempt and standby search alike.

There is one place on each side. The client: every exchange goes through
`connect::establish` (standby searches via `src/control.rs` ~97, rebuilds
too), which adds the feature from `cfg.enable_birthday`. The host: all three
paths go through `run_attach_exchange` → `exchange_hellos` (`src/serve.rs`
~96, `src/listener.rs` ~104, the control door through the listener), which
clears the flag on a **per-exchange clone** of its `NetConfig` — the
listener holds one `cfg` for all its attaches (`src/listener.rs` ~66), and
one client's choice must not stick to the next. No
`PROTO_VERSION` bump: `features` is `#[serde(default)]` and an older host
ignores a feature it does not know, so against an old host only the client's
half stops (the docs say so). This is the only host-side change in C.

The help line and docs warn: behind a **symmetric NAT** the birthday blast is
the last rung that can work (rung 4 is not implemented,
`src/ladder.rs` ~396–410), so `birthday = false` there means a connect that
cannot punch through fails.

---

## 3. Units

| Unit | Where | One job |
|---|---|---|
| Settings table | `src/config.rs` | `static SETTINGS: &[Setting]`: key, kind, default, range, help line, `Applies::{Now, NextAttempt, NextConnect}`, get/set between `Settings` and a generic value. |
| `Settings` | `src/config.rs` | The typed values everything else uses. No string lookups at runtime. |
| Resolver | `src/config.rs` | `resolve(toml_text, target) -> (Settings, Layers, Vec<Warning>)`. Pure. |
| Saver | `src/config.rs` | `save(dir, target, level, edits) -> Result<String>`: re-read the file, change only the named keys with `toml_edit`, write a temp file and rename it into place (§4.4), return the text written. `Level::{Global, Host}`. An edit is `Set(value)` or `Remove(level)`. |
| Config mode | `src/ui.rs` | `Mode::Config { cursor, scroll, editing }` in the popup state machine. |
| Config view | `src/view.rs` → `ConfigView` in `crates/oxutrm-client/src/popup.rs` | Rows built from the table + `Settings` + `Layers` + pending edits; drawn with `Paragraph` lines inside the existing box. No new ratatui features. |
| `apply` | `src/session.rs` | `ClientSession::apply(&Settings)`: the one way values reach the session, at startup and after every edit. |
| Docs | `docs/config.md` | Generated from the table; a test fails when it is stale. |

New dependencies: `serde` (already in the workspace) and `toml_edit`.

---

## 4. The screen

```
┌─ oxutrm · config · thinlinc ──────────────────────────────────┐
│ popup ─────────────────────────────────────────────────────── │
│   key               ctrl-\                                    │
│   auto_open_after   2s                                        │
│   linger            3s                                        │
│   splash            on                    next connect        │
│ recovery ──────────────────────────────────────────────────── │
│ ▸ rebuild_after     30s *            host                     │
│   connect_timeout   10s                   next attempt        │
│ network ───────────────────────────────────────────────────── │
│   standby           off              host                     │
│   …                                                           │
│ ───────────────────────────────────────────────────────────── │
│ silence before an ssh rebuild starts · 5s–10m                 │
│ ↑↓ move  ⏎ edit  x reset  w save  Esc back                    │
└───────────────────────────────────────────────────────────────┘
```

- Sections under rules drawn into the border, as in the status view. Same
  72×24 cap; the list scrolls when the terminal is smaller.
- Columns: name, value, `*` when changed and not saved, origin (`host` /
  `global`, blank for default), and the takes-effect note when it is not
  `now`.
- The line above the key bar is the selected setting's help and range, or the
  reason an edit was refused.

### 4.1 Keys

| Key | Does |
|---|---|
| `c` (status view) | Opens the config screen. Not while the "send / drop held input?" question is up (`Confirming` owns the keys); `c config` is enabled in the key bar everywhere else. |
| ↑ ↓ (and `k` `j`) | Move. |
| Enter | Edit: a bool flips; a duration opens a text field on its line (Enter accepts, Esc cancels); `stun_servers` opens a sub-list (one row per server; Enter edits, `+` adds, `-` removes, Esc back); `popup.key` asks for the new key to be pressed — h, i, j, m are refused with the reason, Backspace/DEL (never a valid key) sets `off`, Esc cancels. |
| `x` | Marks the selected key's override for removal **at the level it came from**, so the next layer shows through. Unsaved until `w`. |
| `w` | "save for **a**ll hosts or **h** <target> only"; Esc cancels. **`a`** writes every pending `Set` globally and **also removes this target's override** of the same key, so the value saved is the value in effect here next time; a `Remove` happens at its own level. **`h`** writes every pending `Set` to this host's table; a `Remove` of a host value removes it there, and a `Remove` of a **global** value becomes a host override set to the built-in default — `h` never changes another host. |
| Esc | Back to the status view. Pending edits stay applied and marked. |
| `q` | Quits, as everywhere in the popup. Pending edits are dropped. |

Every accepted edit calls `apply` at once, so "now" settings act before they
are saved. An edit is checked against the table first; a refused value keeps
the field open with the reason on the help line.

**While a text field or key capture is open, every byte belongs to it**: `q`,
`x`, `w`, `k`, `j` are text, and the prefix key is text (or, in capture, the
key being captured). Esc cancels a field or capture (ESC is never a valid
popup key, so nothing is lost). The only exception is the `Confirming`
takeover below. A newline in an unbracketed paste accepts the field and the
rest of that read is dropped, never run as commands.

A successful save clears the pending markers. The text the saver returns is
re-resolved **for display only** — origins, `Layers` and the
`config: N warnings` count are rebuilt from it — while the settings in effect
are not re-applied: a hand edit made elsewhere during the session still waits
for the next connect (§1.2). A failed save leaves the edits pending, shows
the reason on the help line, and is recorded in `client.log`.

### 4.2 The config screen and the link

Today auto-open never changes a popup that is already open (`Ui::tick`,
`src/ui.rs` ~150). With the config screen open:

- **An outage, no field open:** at the moment auto-open would fire (the
  effective `auto_open_after`; never when it is `off`), the popup switches to
  the status view **in `Open` mode** — it stays until closed, it does not
  linger away when the link returns. Decided once per outage, as `dismissed`
  is: closing a field later in the same outage does not switch.
- **An outage, a field open:** the screen stays, and its header shows the
  outage (`no reply 4 s`).
- **`Confirming`** (the link came back with held input): the question always
  takes over — the popup switches to the status view and an open field is
  dropped, so `s`/`d` can never land in it.

Pending edits stay applied in every case.

### 4.3 Key decoding in config mode

Today `Ui::keys` drops any read that starts with ESC and is not a lone ESC
(`src/ui.rs` ~189). The config screen needs more:

- `ESC [ A` / `ESC [ B` and `ESC O A` / `ESC O B` move; other sequences are
  still ignored.
- Bracketed paste (`ESC [200~ … ESC [201~`, mirrored to the local terminal)
  goes into an open field with the wrappers stripped and control bytes
  dropped; with no field open it is ignored.
- `0x7f` and `0x08` delete one character; input is UTF-8.
- With a field or key capture open, bytes go to it **before** every other
  rule — before `q` → quit and before the prefix/ESC close rule
  (`shown_key`, `src/ui.rs` ~224–239); a lone ESC cancels.
- `Mode` is `Copy` today; the text field's buffer lives beside it in `Ui`,
  not inside `Mode`.

### 4.4 Writing the file safely

- **Symlinks are followed**: the link is resolved with `read_link` (not
  `canonicalize`, which fails on a dangling link), the temp file is created
  beside the real target and renamed over it, so a dotfile manager keeps
  tracking it. A dangling link's target path is written (created). The
  symlink itself is never replaced.
- **A read-only target is refused**, not replaced: a rename needs only the
  directory to be writable, so the saver checks the target with
  `rustix::fs::access(W_OK)` first and fails the save with the reason.
- The temp file has a unique name (pid + random), is created with
  `create_new`, and gets the original file's mode before the rename.
- The first save creates the config directory (`create_dir_all`).
- If the re-read finds the file is not valid TOML (a hand edit broke it), the
  save is **refused** with the parse error; it never falls back to an empty
  document.
- toml_edit's indexing panics when an item is not a table; the saver uses
  `get_mut` / `as_table_like_mut` only, and a type conflict (`network = "x"`)
  is a failed save, never a panic.

---

## 5. Wiring: constants become fields

Each constant becomes a field initialised from `Settings`; its old value is the
default in the table.

| Consumer | Today | With C |
|---|---|---|
| `Ui` (`src/ui.rs`) | `PREFIX`, `LINGER`; auto-open on the first outage lap | fields `key: Option<u8>`, `linger`, `auto_open_after`; `tick` opens `Auto` only once the silence reaches `auto_open_after`. With `key = None` the prefix byte is ordinary input. |
| `LinkState` (`src/linkstate.rs`) | `SILENT_AFTER`, `REBUILD_AFTER` | fields, set by `retune(silent_after, rebuild_after)`; takes effect on the next `evaluate`. |
| `Rebuild` (`src/rebuild.rs`) | own `NetConfig::default()`, `CONNECT_TIMEOUT` const | `cfg` and `connect_timeout` set from `Settings`; read when the next attempt begins. |
| `Standby` (`src/standby.rs`) | `cfg` cloned from connect | `set_cfg`; read when the next search begins. Off/on per §2.3. |
| `connect()` (`src/connect.rs`) | `NetConfig::default()`, splash always | loads the config (target known, before `NetConfig` is built), builds `NetConfig` from it, gates the splash, hands `Settings` + `Layers` + warnings to the session (which records them, §2.2). |
| `view.rs` | the "ssh rebuild in …" countdown uses `REBUILD_AFTER` (~260) | the facts carry the effective `rebuild_after`. |
| The run loop (`src/session.rs`) | — | applies a standby-off edit to the search / probe tasks and the watched connection it owns (§2.3). |
| Host ladder (`src/attach_exchange.rs`, `src/listener.rs`, `src/control.rs`) | `enable_birthday` from `NetConfig::default()` | cleared for an exchange whose `ClientHello` carries `no-birthday` (§2.4). |

`NetConfig` gains nothing; `Settings` fills its existing
`stun_servers`, `enable_port_mapping` and `enable_birthday` fields.

---

## 6. Testing

Test-first throughout, as for B.

- **Resolver:** layering and per-layer values with a host override; dotted,
  subtable and inline-table spellings resolve alike; a host key for a
  different target is ignored; the effective `auto_open_after` is raised to
  `silent_after` while the stored value is kept; both keys `off` warns and
  falls back; wrong type, out of range (incl. `ctrl-m`, `1.5s` timeout),
  unknown key and broken TOML each warn and fall back; a missing file is
  defaults with no warning.
- **`config_path`:** XDG set, unset, relative, empty.
- **Saver:** round trip keeps comments, ordering and unrelated keys; global
  and host saves; a global save removes this target's override; `x` removes a
  key at its own level and an emptied host table; edits to keys written
  dotted, as subtables and inline; a file changed on disk between load and
  save keeps the other change; through a symlink the link survives and the
  target changes; a dangling link's target is created and the link kept; a
  read-only target is refused and untouched; the mode is kept; a missing dir
  is created; an invalid
  file on re-read and a type conflict (`network = "x"`) fail the save without
  touching the file or panicking.
- **Config mode:** move by arrow sequences (CSI and SS3), edit and accept,
  edit and cancel, backspace, a bracketed paste into a field, refused value,
  key capture (incl. the prefix and ESC captured, `ctrl-m` refused), the
  `stun_servers` sub-list, `x`, `w` → `a`/`h`, Esc back, `c` refused under
  `Confirming`; `q`/`x`/`w`/`k`/`j` typed into a field stay text; Esc
  cancels key capture; Backspace in capture sets `off`; an outage with a
  field open keeps the screen and shows the outage; without one it switches
  to status in `Open` mode only when auto-open would fire (never with it
  `off`), once per outage; `Confirming` always switches and drops the field;
  both keys `off` refused, including when reached by `x` or by a global save
  removing a host override; `x` on a global value saved with `h` writes a
  host override at the default and leaves the global key alone; after a save
  the origins and warning count are rebuilt and other settings are not
  re-applied.
- **Snapshots:** config screen at 80×24 and at a small size (insta, beside the
  status view's).
- **`apply`:** each consumer takes the new value — e.g. with
  `rebuild_after = 30s` the outage reaches `Recovering` at 30 s, not 20 s,
  and the popup at 25 s of silence reads `in 0:05`; `popup.key = ctrl-]`
  opens the popup on 0x1d and passes 0x1c through; standby off closes the
  parked link and aborts a search in flight; a `Found` already queued when it
  was turned off is closed, not parked; after off → on a probe still runs
  (an aborted probe does not leave `probing` stuck); with `rebuild_after`
  below `silent_after` the rebuild waits for the outage.
- **Birthday:** `birthday = false` puts `no-birthday` in every `ClientHello`
  (connect, rebuild, standby search); a host exchange that receives it runs
  no blast; the next exchange on the same listener without it still blasts
  (per-exchange clone); an old-style hello without it still blasts.
- **Warnings:** a config warning appears as a shown entry in the popup's log
  and in the splash caption's `config: N warnings`.
- **Docs:** `docs/config.md` equals what the table generates.
- **Changelog** entry.

---

## 7. Build order

One branch, `feat/config`, four changes, each with its own implementer and
reviewer:

1. `config.rs` (table, `Settings`, resolver with `Layers`, `config_path`),
   loading at connect, warnings as activity entries, `NetConfig` and splash
   from it, generated docs. No screen.
2. Constants become fields (incl. the view's countdown);
   `ClientSession::apply`; standby off/on in the loop; `no-birthday` at both
   ends.
3. The config screen (mode, view, keys, live apply).
4. Saving (`toml_edit`, levels, `x`, atomic write).

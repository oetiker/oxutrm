# oxutrm — Config file and config screen

Status: draft 2026-10-06, design agreed with the user in chat (three sections).
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

### 1.2 Not in C

- The session switcher (D); `s sessions` stays greyed out.
- Host-side settings. The host reads no config file.
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
key = "ctrl-\\"           # ctrl-a … ctrl-z, ctrl-\, ctrl-], ctrl-^, ctrl-_, or "off"
auto_open_after = "2s"    # since the last frame heard; "off" = never auto-open
linger = "3s"
splash = true

[recovery]
silent_after = "2s"       # silence before it is an outage: input held, standby probed
rebuild_after = "20s"     # silence before an ssh rebuild starts
connect_timeout = "10s"   # added to the rebuild ssh only where `ssh -G` has none

[network]
standby = true
stun_servers = [
  "stun.cloudflare.com:3478",
  "stun.l.google.com:19302",
  "stun.nextcloud.com:443",
  "stun.sipgate.net:3478",
]
port_mapping = true       # UPnP / NAT-PMP / PCP
birthday = true           # the birthday-paradox NAT punch

[host."thinlinc"]         # any subset of the sections above
network.standby = false
recovery.rebuild_after = "30s"
```

### 2.1 Settings

Defaults are today's constants, so a missing file behaves exactly as the
client does now.

| Key | Kind | Default | Range | Takes effect |
|---|---|---|---|---|
| `popup.key` | key | `ctrl-\` | see §2 comment, or `off` | now |
| `popup.auto_open_after` | duration or `off` | 2 s | `silent_after` – 10 min | now |
| `popup.linger` | duration | 3 s | 0 – 1 min | now |
| `popup.splash` | bool | true | | next connect |
| `recovery.silent_after` | duration | 2 s | 1 s – 1 min | now |
| `recovery.rebuild_after` | duration | 20 s | 5 s – 10 min | now |
| `recovery.connect_timeout` | duration | 10 s | 1 s – 2 min | next attempt |
| `network.standby` | bool | true | | now (see §2.3) |
| `network.stun_servers` | list of `host:port` | the four above | 0–16 entries | next attempt / search |
| `network.port_mapping` | bool | true | | next attempt / search |
| `network.birthday` | bool | true | | next attempt / search |

"Next attempt / search" means the next ssh rebuild attempt or standby search;
the first connect always uses the loaded values.

### 2.2 Layering and validation

Each key resolves through **built-in default → top-level section →
`[host."<target>"]`**, where `<target>` is the ssh target exactly as typed on
the command line. The resolver records the **origin** of every value
(`Default`, `Global`, `Host`) for the screen.

- A value of the wrong type or outside its range is a warning, and that key
  falls back to the next layer down.
- An unknown key or section is a warning (typos must not be silent).
- A file that is not valid TOML is one warning, and everything is default.
- `auto_open_after` below `silent_after` is raised to it (no warning — the
  popup cannot open before there is an outage to show).
- `popup.key = "off"` leaves the popup reachable only by auto-open; the
  config screen is then reachable only during an outage. Allowed, documented.

Warnings are printed by `connect()` **before raw mode**, one line each, in the
same place the `resumed session` line is printed, and recorded in
`client.log`. Once a session owns the screen, nothing prints (the in-session
`deny(print_stderr)` rule holds for every module C touches).

### 2.3 Standby on and off

Turning `network.standby` off mid-session closes the parked standby (as
`Standby::forget` closes a corpse) and drops the `Standby`. Turning it back on
creates a new `Standby` only if the host offered one at connect —
`ClientSession` keeps that feature flag, which today is consulted once and
thrown away. On a host that did not offer one, the screen shows it as
`next connect`.

---

## 3. Units

| Unit | Where | One job |
|---|---|---|
| Settings table | `src/config.rs` | `static SETTINGS: &[Setting]`: key, kind, default, range, help line, `Applies::{Now, NextAttempt, NextConnect}`, get/set between `Settings` and a generic value. |
| `Settings` | `src/config.rs` | The typed values everything else uses. No string lookups at runtime. |
| Resolver | `src/config.rs` | `resolve(toml_text, target) -> (Settings, Origins, Vec<Warning>)`. Pure. |
| Saver | `src/config.rs` | `save(dir, target, level, edits)`: re-read the file, change only the named keys with `toml_edit`, write a temp file and rename it into place. `Level::{Global, Host}`. Removing a key (`x`) is an edit too. |
| Config mode | `src/ui.rs` | `Mode::Config { cursor, scroll, editing }` in the popup state machine. |
| Config view | `src/view.rs` → `ConfigView` in `crates/oxutrm-client/src/popup.rs` | Rows built from the table + `Settings` + `Origins` + pending edits; drawn with `Paragraph` lines inside the existing box. No new ratatui features. |
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
| Enter | Edit: a bool flips; a duration or list opens a text field on its line (Enter accepts, Esc cancels); `popup.key` asks for the new key to be pressed. |
| `x` | Removes the selected key's override at the level it came from, so the next layer shows through. Unsaved until `w`. |
| `w` | "save for **a**ll hosts or **h** <target> only"; `a`/`h` saves every pending edit to that level, Esc cancels. |
| Esc | Back to the status view. Pending edits stay applied and marked. |
| `q` | Quits, as everywhere in the popup. Pending edits are dropped. |

Every accepted edit calls `apply` at once, so "now" settings act before they
are saved. An edit is checked against the table first; a refused value keeps
the field open with the reason on the help line.

If an outage begins while the config screen is open, the popup switches to the
status view (as auto-open does today); pending edits stay applied. A failed
save (unwritable file, disk full) leaves the edits pending and shows the
reason on the help line; it is also recorded in `client.log`.

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
| `connect()` (`src/connect.rs`) | `NetConfig::default()`, splash always | loads the config (target known, before `NetConfig` is built), builds `NetConfig` from it, gates the splash, prints warnings, hands `Settings` + `Origins` to the session. |

`NetConfig` gains nothing; `Settings` fills its existing
`stun_servers`, `enable_port_mapping` and `enable_birthday` fields.

---

## 6. Testing

Test-first throughout, as for B.

- **Resolver:** layering and origins with a host override; a host key for a
  different target is ignored; raising `auto_open_after` to `silent_after`;
  wrong type, out of range, unknown key and broken TOML each warn and fall
  back; a missing file is defaults with no warning.
- **`config_path`:** XDG set, unset, relative, empty.
- **Saver:** round trip keeps comments, ordering and unrelated keys; global
  and host saves; `x` removes a key and an emptied host table; a file changed
  on disk between load and save keeps the other change; the write is atomic
  (temp + rename in the same dir).
- **Config mode:** move, edit and accept, edit and cancel, refused value, key
  capture, `x`, `w` → `a`/`h`, Esc back, `c` refused under `Confirming`,
  outage switching back to status.
- **Snapshots:** config screen at 80×24 and at a small size (insta, beside the
  status view's).
- **`apply`:** each consumer takes the new value — e.g. with
  `rebuild_after = 30s` the outage reaches `Recovering` at 30 s, not 20 s;
  `popup.key = ctrl-]` opens the popup on 0x1d and passes 0x1c through;
  standby off closes the parked link.
- **Docs:** `docs/config.md` equals what the table generates.
- **Changelog** entry.

---

## 7. Build order

One branch, `feat/config`, four changes, each with its own implementer and
reviewer:

1. `config.rs` (table, `Settings`, resolver, `config_path`), loading at
   connect, warnings, `NetConfig` and splash from it, generated docs. No
   screen.
2. Constants become fields; `ClientSession::apply`; standby off/on.
3. The config screen (mode, view, keys, live apply).
4. Saving (`toml_edit`, levels, `x`, atomic write).

# oxutrm configuration

<!-- Generated from src/config.rs by `OXUTRM_BLESS=1 cargo test the_docs_are_generated_from_the_table`. Do not edit by hand. -->

The client reads `$XDG_CONFIG_HOME/oxutrm/config.toml`, else `~/.config/oxutrm/config.toml`, when it connects. A missing file is all defaults; a broken one never stops a connect: each problem is a warning in the status popup's log and in `client.log`, and the setting it is about falls back to the next layer down.

Every setting resolves through its built-in default, then the top-level section, then `[host."<target>"]`, where `<target>` is the ssh target exactly as typed. Durations are strings: `"20s"`, `"1m"`, `"1m 30s"`.

The config screen (`c` in the status popup) changes a setting for the running session and saves it, for every host (`w` `a`) or for this host only (`w` `h`), keeping everything else in the file as written.

| Key | Default | Range | Takes effect | |
|---|---|---|---|---|
| `popup.key` | `"ctrl-\\"` | ctrl-a … ctrl-z except h i j m; ctrl-\ ctrl-] ctrl-^ ctrl-_; off | now | the key that opens this popup |
| `popup.auto_open_after` | `"2s"` | 0s–10m or off | now | silence before the popup opens by itself |
| `popup.linger` | `"3s"` | 0s–1m | now | how long the popup stays once the link is back |
| `popup.splash` | `true` | true or false | next connect | the startup splash |
| `recovery.silent_after` | `"2s"` | 1s–1m | now | silence before it is an outage: input held, standby probed |
| `recovery.rebuild_after` | `"20s"` | 5s–10m | now | silence before an ssh rebuild starts |
| `recovery.connect_timeout` | `"10s"` | 1s–2m | next rebuild attempt | ssh ConnectTimeout for a rebuild, where ssh has none of its own |
| `network.standby` | `true` | true or false | now | keep a second path ready to fail over to |
| `network.stun_servers` | `["stun.cloudflare.com:3478", "stun.l.google.com:19302", "stun.nextcloud.com:443", "stun.sipgate.net:3478"]` | 0–16 host:port | next rebuild attempt or standby search | STUN servers that tell this side its public address |
| `network.port_mapping` | `true` | true or false | next rebuild attempt or standby search | ask the router for a port mapping (UPnP / NAT-PMP / PCP) |
| `network.birthday` | `true` | true or false | next rebuild attempt or standby search | the birthday-paradox NAT punch, at both ends |

**`popup.key`** -- ctrl-h, ctrl-i, ctrl-j and ctrl-m are refused: they are Backspace, Tab, line feed and Enter. `off` leaves the popup reachable only by opening by itself during an outage, and the config screen with it; `off` together with `popup.auto_open_after = "off"` is refused.

**`popup.auto_open_after`** -- Counted from the last frame heard. Never earlier than `recovery.silent_after`: a smaller value is kept as written and raised to it. `off` never opens it.

**`recovery.rebuild_after`** -- Counted from the last frame heard. Never earlier than `recovery.silent_after`: a smaller value is kept as written and raised to it.

**`recovery.connect_timeout`** -- Whole seconds: ssh's `ConnectTimeout` takes nothing finer. Added to the rebuild's ssh only where `ssh -G` reports no `ConnectTimeout` of your own.

**`network.standby`** -- Only on a host that offers a standby; on one that did not, turning it on waits for the next connect.

**`network.stun_servers`** -- `host:port` or `[v6]:port`, at most 16. An empty list is allowed: no public address is learned, and only paths that need none can work.

**`network.birthday`** -- `false` asks the host not to blast either; a host older than this client ignores the request, so only this side's half stops. Behind a symmetric NAT the blast is the last rung that can work: there, `false` means a connect that cannot punch through fails.

## Example

```toml
[popup]
key = "ctrl-]"

[recovery]
rebuild_after = "30s"

[host."thinlinc"]
network.standby = false
```

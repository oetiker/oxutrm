//! The client's config file: one table of settings drives loading,
//! layering, validation, the config screen and `docs/config.md`.
//!
//! `$XDG_CONFIG_HOME/oxutrm/config.toml`, else `~/.config/oxutrm/config.toml`.
//! Every key resolves through **built-in default -> top-level section ->
//! `[host."<target>"]`**, and every layer's value is kept ([`Layers`]), so
//! the screen can say where a value came from and `x` can show the next
//! layer through without reading the file again. Nothing here prints: a
//! problem with the file is a warning the session records.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use toml_edit::{DocumentMut, Item};

mod save;

/// The file's name inside the config directory.
pub(crate) const FILE: &str = "config.toml";

/// Where the config directory is, from the two variables that decide it.
/// A relative or empty value is ignored, as the XDG base-directory spec
/// requires -- the same rule as `activity::state_path`.
pub(crate) fn config_dir(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let base = match absolute(xdg) {
        Some(p) => p,
        None => absolute(home)?.join(".config"),
    };
    Some(base.join("oxutrm"))
}

/// The popup opens by itself this long after the last frame heard, unless
/// the file says otherwise.
const AUTO_OPEN_AFTER: Duration = Duration::from_secs(2);

/// What the client runs with: typed values, no string lookups at runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    /// The byte that opens the popup; `None` is `off`.
    pub(crate) popup_key: Option<u8>,
    /// As written; `None` is `off`. See [`Settings::effective_auto_open`].
    pub(crate) auto_open_after: Option<Duration>,
    pub(crate) linger: Duration,
    pub(crate) splash: bool,
    pub(crate) silent_after: Duration,
    /// As written. See [`Settings::effective_rebuild_after`].
    pub(crate) rebuild_after: Duration,
    /// Whole seconds: ssh's `ConnectTimeout` takes nothing finer.
    pub(crate) connect_timeout: Duration,
    pub(crate) standby: bool,
    pub(crate) stun_servers: Vec<String>,
    pub(crate) port_mapping: bool,
    pub(crate) birthday: bool,
}

impl Default for Settings {
    /// Today's constants, so a missing file behaves exactly as the client
    /// did before there was one.
    fn default() -> Settings {
        let net = oxutrm_net::NetConfig::default();
        Settings {
            popup_key: Some(crate::ui::PREFIX),
            auto_open_after: Some(AUTO_OPEN_AFTER),
            linger: crate::ui::LINGER,
            splash: true,
            silent_after: crate::linkstate::SILENT_AFTER,
            rebuild_after: crate::linkstate::REBUILD_AFTER,
            connect_timeout: crate::rebuild::DEFAULT_CONNECT_TIMEOUT,
            standby: true,
            stun_servers: net.stun_servers,
            port_mapping: net.enable_port_mapping,
            birthday: net.enable_birthday,
        }
    }
}

impl Settings {
    /// When the popup opens by itself: never before there is an outage,
    /// so never before `silent_after` (spec §2.2). `None` is never.
    pub(crate) fn effective_auto_open(&self) -> Option<Duration> {
        self.auto_open_after.map(|d| d.max(self.silent_after))
    }

    /// When a rebuild starts: never before there is an outage either.
    pub(crate) fn effective_rebuild_after(&self) -> Duration {
        self.rebuild_after.max(self.silent_after)
    }

    /// Whether the popup could never be opened: no key and no auto-open.
    pub(crate) fn unreachable(&self) -> bool {
        self.popup_key.is_none() && self.auto_open_after.is_none()
    }

    /// The network configuration these settings ask for. `NetConfig` gains
    /// nothing: the rest stays its default.
    pub(crate) fn net_config(&self) -> oxutrm_net::NetConfig {
        oxutrm_net::NetConfig {
            stun_servers: self.stun_servers.clone(),
            enable_port_mapping: self.port_mapping,
            enable_birthday: self.birthday,
            ..oxutrm_net::NetConfig::default()
        }
    }
}

/// One setting's value, whatever its kind: what the table's `get` and
/// `set` trade in, and what a layer holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    Bool(bool),
    /// `None` is `off`.
    Duration(Option<Duration>),
    /// `None` is `off`.
    Key(Option<u8>),
    List(Vec<String>),
}

/// What a setting's values look like, and which of them are allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    Key,
    Duration {
        min: Duration,
        max: Duration,
        /// `off` is a value.
        off: bool,
        /// A fraction of a second is out of range.
        whole_secs: bool,
    },
    Bool,
    Servers,
}

/// When a changed value acts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Applies {
    Now,
    /// The next ssh rebuild attempt or standby search.
    NextAttempt,
    NextConnect,
}

impl Applies {
    /// The screen's takes-effect column: blank for `Now`.
    pub(crate) fn note(self) -> &'static str {
        match self {
            Applies::Now => "",
            Applies::NextAttempt => "next attempt",
            Applies::NextConnect => "next connect",
        }
    }
}

/// One row of the table.
pub(crate) struct Setting {
    pub(crate) section: &'static str,
    pub(crate) name: &'static str,
    pub(crate) shape: Shape,
    pub(crate) applies: Applies,
    /// One line, for the screen's help line and the docs' table.
    pub(crate) help: &'static str,
    /// More, for the docs only; empty for most.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "read by `docs`, which runs only as the docs test")
    )]
    pub(crate) doc: &'static str,
    pub(crate) get: fn(&Settings) -> Value,
    pub(crate) set: fn(&mut Settings, Value),
}

impl Setting {
    /// `section.name`, as the file spells it.
    #[cfg(test)]
    pub(crate) fn key(&self) -> String {
        format!("{}.{}", self.section, self.name)
    }

    /// The built-in default, which is the value [`Settings::default`] has.
    pub(crate) fn default_value(&self) -> Value {
        (self.get)(&Settings::default())
    }
}

const fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Where `popup.key` is in [`SETTINGS`].
#[cfg(test)]
pub(crate) const POPUP_KEY: usize = 0;
/// Where `popup.auto_open_after` is in [`SETTINGS`].
pub(crate) const AUTO_OPEN: usize = 1;
/// Where `network.standby` is in [`SETTINGS`].
pub(crate) const STANDBY: usize = 7;

/// Every setting, in the order the file, the screen and the docs show them.
pub(crate) static SETTINGS: &[Setting] = &[
    Setting {
        section: "popup",
        name: "key",
        shape: Shape::Key,
        applies: Applies::Now,
        help: "the key that opens this popup",
        doc: "ctrl-h, ctrl-i, ctrl-j and ctrl-m are refused: they are Backspace, Tab, \
              line feed and Enter. `off` leaves the popup reachable only by opening \
              by itself during an outage, and the config screen with it; `off` \
              together with `popup.auto_open_after = \"off\"` is refused.",
        get: |s| Value::Key(s.popup_key),
        set: |s, v| {
            if let Value::Key(k) = v {
                s.popup_key = k;
            }
        },
    },
    Setting {
        section: "popup",
        name: "auto_open_after",
        shape: Shape::Duration {
            min: secs(0),
            max: secs(600),
            off: true,
            whole_secs: false,
        },
        applies: Applies::Now,
        help: "silence before the popup opens by itself",
        doc: "Counted from the last frame heard. Never earlier than \
              `recovery.silent_after`: a smaller value is kept as written and \
              raised to it. `off` never opens it.",
        get: |s| Value::Duration(s.auto_open_after),
        set: |s, v| {
            if let Value::Duration(d) = v {
                s.auto_open_after = d;
            }
        },
    },
    Setting {
        section: "popup",
        name: "linger",
        shape: Shape::Duration {
            min: secs(0),
            max: secs(60),
            off: false,
            whole_secs: false,
        },
        applies: Applies::Now,
        help: "how long the popup stays once the link is back",
        doc: "",
        get: |s| Value::Duration(Some(s.linger)),
        set: |s, v| {
            if let Value::Duration(Some(d)) = v {
                s.linger = d;
            }
        },
    },
    Setting {
        section: "popup",
        name: "splash",
        shape: Shape::Bool,
        applies: Applies::NextConnect,
        help: "the startup splash",
        doc: "",
        get: |s| Value::Bool(s.splash),
        set: |s, v| {
            if let Value::Bool(b) = v {
                s.splash = b;
            }
        },
    },
    Setting {
        section: "recovery",
        name: "silent_after",
        shape: Shape::Duration {
            min: secs(1),
            max: secs(60),
            off: false,
            whole_secs: false,
        },
        applies: Applies::Now,
        help: "silence before it is an outage: input held, standby probed",
        doc: "",
        get: |s| Value::Duration(Some(s.silent_after)),
        set: |s, v| {
            if let Value::Duration(Some(d)) = v {
                s.silent_after = d;
            }
        },
    },
    Setting {
        section: "recovery",
        name: "rebuild_after",
        shape: Shape::Duration {
            min: secs(5),
            max: secs(600),
            off: false,
            whole_secs: false,
        },
        applies: Applies::Now,
        help: "silence before an ssh rebuild starts",
        doc: "Counted from the last frame heard. Never earlier than \
              `recovery.silent_after`: a smaller value is kept as written and \
              raised to it.",
        get: |s| Value::Duration(Some(s.rebuild_after)),
        set: |s, v| {
            if let Value::Duration(Some(d)) = v {
                s.rebuild_after = d;
            }
        },
    },
    Setting {
        section: "recovery",
        name: "connect_timeout",
        shape: Shape::Duration {
            min: secs(1),
            max: secs(120),
            off: false,
            whole_secs: true,
        },
        applies: Applies::NextAttempt,
        help: "ssh ConnectTimeout for a rebuild, where ssh has none of its own",
        doc: "Whole seconds: ssh's `ConnectTimeout` takes nothing finer. Added to \
              the rebuild's ssh only where `ssh -G` reports no `ConnectTimeout` \
              of your own.",
        get: |s| Value::Duration(Some(s.connect_timeout)),
        set: |s, v| {
            if let Value::Duration(Some(d)) = v {
                s.connect_timeout = d;
            }
        },
    },
    Setting {
        section: "network",
        name: "standby",
        shape: Shape::Bool,
        applies: Applies::Now,
        help: "keep a second path ready to fail over to",
        doc: "Only on a host that offers a standby; on one that did not, turning \
              it on waits for the next connect.",
        get: |s| Value::Bool(s.standby),
        set: |s, v| {
            if let Value::Bool(b) = v {
                s.standby = b;
            }
        },
    },
    Setting {
        section: "network",
        name: "stun_servers",
        shape: Shape::Servers,
        applies: Applies::NextAttempt,
        help: "STUN servers that tell this side its public address",
        doc: "`host:port` or `[v6]:port`, at most 16. An empty list is allowed: \
              no public address is learned, and only paths that need none can \
              work.",
        get: |s| Value::List(s.stun_servers.clone()),
        set: |s, v| {
            if let Value::List(l) = v {
                s.stun_servers = l;
            }
        },
    },
    Setting {
        section: "network",
        name: "port_mapping",
        shape: Shape::Bool,
        applies: Applies::NextAttempt,
        help: "ask the router for a port mapping (UPnP / NAT-PMP / PCP)",
        doc: "",
        get: |s| Value::Bool(s.port_mapping),
        set: |s, v| {
            if let Value::Bool(b) = v {
                s.port_mapping = b;
            }
        },
    },
    Setting {
        section: "network",
        name: "birthday",
        shape: Shape::Bool,
        applies: Applies::NextAttempt,
        help: "the birthday-paradox NAT punch, at both ends",
        doc: "`false` asks the host not to blast either; a host older than this \
              client ignores the request, so only this side's half stops. Behind \
              a symmetric NAT the blast is the last rung that can work: there, \
              `false` means a connect that cannot punch through fails.",
        get: |s| Value::Bool(s.birthday),
        set: |s, v| {
            if let Value::Bool(b) = v {
                s.birthday = b;
            }
        },
    },
];

/// The most `network.stun_servers` may hold.
const MAX_SERVERS: usize = 16;

/// Where `section.name` is in [`SETTINGS`].
fn index_of(section: &str, name: &str) -> Option<usize> {
    SETTINGS
        .iter()
        .position(|s| s.section == section && s.name == name)
}

/// A key's name, as the file and the screen write it.
pub(crate) fn key_name(key: Option<u8>) -> String {
    match key {
        None => "off".to_string(),
        Some(b @ 0x01..=0x1a) => format!("ctrl-{}", char::from(b'a' + b - 1)),
        Some(0x1c) => "ctrl-\\".to_string(),
        Some(0x1d) => "ctrl-]".to_string(),
        Some(0x1e) => "ctrl-^".to_string(),
        Some(0x1f) => "ctrl-_".to_string(),
        Some(b) => format!("0x{b:02x}"),
    }
}

/// Whether `byte` may open the popup: a ctrl key that is not also a key
/// every terminal user needs.
pub(crate) fn check_key(byte: u8) -> Result<Option<u8>, String> {
    match byte {
        0x08 => Err("ctrl-h is Backspace".to_string()),
        0x09 => Err("ctrl-i is Tab".to_string()),
        0x0a => Err("ctrl-j is a line feed".to_string()),
        0x0d => Err("ctrl-m is Enter".to_string()),
        0x1b => Err("ctrl-[ is Esc".to_string()),
        0x01..=0x1f => Ok(Some(byte)),
        _ => Err(format!("{:?} is not a ctrl key", char::from(byte))),
    }
}

fn parse_key(text: &str) -> Result<Option<u8>, String> {
    if text == "off" {
        return Ok(None);
    }
    let bad = || {
        format!(
            "{text:?} is not a key; write ctrl-<letter>, ctrl-\\, ctrl-], ctrl-^, ctrl-_ or off"
        )
    };
    let rest = text.strip_prefix("ctrl-").ok_or_else(bad)?;
    let mut chars = rest.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Err(bad());
    };
    let byte = match c.to_ascii_lowercase() {
        l @ 'a'..='z' => l as u8 - b'a' + 1,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        _ => return Err(bad()),
    };
    check_key(byte)
}

/// A duration as the file and the screen write it: `20s`, `1m 30s`.
pub(crate) fn show_duration(d: Duration) -> String {
    jiff::SignedDuration::try_from(d).map_or_else(|_| format!("{d:?}"), |s| format!("{s:#}"))
}

fn parse_duration(text: &str, shape: Shape) -> Result<Option<Duration>, String> {
    let Shape::Duration {
        min,
        max,
        off,
        whole_secs,
    } = shape
    else {
        unreachable!("only durations are parsed as durations")
    };
    if off && text == "off" {
        return Ok(None);
    }
    let signed: jiff::SignedDuration = text
        .parse()
        .map_err(|_| format!("{text:?} is not a duration; write e.g. \"20s\" or \"1m\""))?;
    let d = Duration::try_from(signed).map_err(|_| format!("{text} is negative"))?;
    if whole_secs && d.subsec_nanos() != 0 {
        return Err(format!("{text} is not a whole number of seconds"));
    }
    if d < min || d > max {
        return Err(format!("{} is outside {}", show_duration(d), range(shape)));
    }
    Ok(Some(d))
}

/// Whether `server` is `host:port` or `[v6]:port`.
fn check_server(server: &str) -> Result<(), String> {
    let bad = || format!("{server:?} is not host:port or [v6]:port");
    let (host, port) = server.rsplit_once(':').ok_or_else(bad)?;
    port.parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(bad)?;
    let bracketed = host.starts_with('[') && host.ends_with(']') && host.len() > 2;
    let plain = !host.is_empty() && !host.contains(['[', ']', ':', ' ']);
    if bracketed || plain {
        Ok(())
    } else {
        Err(bad())
    }
}

/// A whole `network.stun_servers` list, checked.
pub(crate) fn check_servers(list: Vec<String>) -> Result<Value, String> {
    if list.len() > MAX_SERVERS {
        return Err(format!("{} servers is more than {MAX_SERVERS}", list.len()));
    }
    for s in &list {
        check_server(s)?;
    }
    Ok(Value::List(list))
}

/// A value typed on the screen, checked against `shape`. Not for
/// `stun_servers`, which the screen edits one entry at a time.
pub(crate) fn parse_text(shape: Shape, text: &str) -> Result<Value, String> {
    match shape {
        Shape::Key => parse_key(text).map(Value::Key),
        Shape::Duration { .. } => parse_duration(text, shape).map(Value::Duration),
        Shape::Bool => match text {
            "on" | "true" => Ok(Value::Bool(true)),
            "off" | "false" => Ok(Value::Bool(false)),
            _ => Err(format!("{text:?} is not on or off")),
        },
        Shape::Servers => check_servers(
            text.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
    }
}

/// A value from the file, checked against `shape`.
fn parse_item(shape: Shape, item: &Item) -> Result<Value, String> {
    match shape {
        Shape::Bool => item
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| "expected true or false".to_string()),
        Shape::Key => {
            let text = item
                .as_str()
                .ok_or_else(|| "expected a string such as \"ctrl-]\"".to_string())?;
            parse_text(shape, text)
        }
        Shape::Duration { .. } => {
            let text = item
                .as_str()
                .ok_or_else(|| "expected a string such as \"20s\"".to_string())?;
            parse_text(shape, text)
        }
        Shape::Servers => {
            let array = item
                .as_array()
                .ok_or_else(|| "expected a list of \"host:port\" strings".to_string())?;
            let list = array
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| "expected a list of \"host:port\" strings".to_string())?;
            check_servers(list)
        }
    }
}

/// A value as the screen shows it.
pub(crate) fn show(value: &Value) -> String {
    match value {
        Value::Bool(true) => "on".to_string(),
        Value::Bool(false) => "off".to_string(),
        Value::Duration(None) => "off".to_string(),
        Value::Duration(Some(d)) => show_duration(*d),
        Value::Key(k) => key_name(*k),
        Value::List(l) => match l.as_slice() {
            [] => "none".to_string(),
            [one] => one.clone(),
            [first, rest @ ..] => format!("{first} +{}", rest.len()),
        },
    }
}

/// What a value may be, in words: the screen's help line and the docs.
pub(crate) fn range(shape: Shape) -> String {
    match shape {
        Shape::Key => {
            "ctrl-a … ctrl-z except h i j m; ctrl-\\ ctrl-] ctrl-^ ctrl-_; off".to_string()
        }
        Shape::Duration { min, max, off, .. } => {
            let span = format!("{}–{}", show_duration(min), show_duration(max));
            if off { format!("{span} or off") } else { span }
        }
        Shape::Bool => "on or off".to_string(),
        Shape::Servers => format!("0–{MAX_SERVERS} host:port"),
    }
}

/// Which file layer a value is written at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    /// The top-level sections: every host.
    Global,
    /// `[host."<target>"]`: this host only.
    Host,
}

/// Where a value in effect came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Default,
    Global,
    Host,
}

impl Origin {
    /// The file layer this origin is written at; the built-in default is in
    /// no file.
    pub(crate) fn level(self) -> Option<Level> {
        match self {
            Origin::Default => None,
            Origin::Global => Some(Level::Global),
            Origin::Host => Some(Level::Host),
        }
    }
}

/// Every layer's value of one key, not only the winner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyLayers {
    pub(crate) default: Value,
    pub(crate) global: Option<Value>,
    pub(crate) host: Option<Value>,
}

impl KeyLayers {
    /// The value in effect, and where it came from.
    pub(crate) fn winner(&self) -> (Origin, &Value) {
        match (&self.host, &self.global) {
            (Some(v), _) => (Origin::Host, v),
            (None, Some(v)) => (Origin::Global, v),
            (None, None) => (Origin::Default, &self.default),
        }
    }

    /// The value with `level` taken out: what `x` shows through.
    pub(crate) fn without(&self, level: Level) -> (Origin, &Value) {
        match (level, &self.host, &self.global) {
            (Level::Global, Some(v), _) => (Origin::Host, v),
            (Level::Host, _, Some(v)) => (Origin::Global, v),
            _ => (Origin::Default, &self.default),
        }
    }

    fn at_mut(&mut self, level: Level) -> &mut Option<Value> {
        match level {
            Level::Global => &mut self.global,
            Level::Host => &mut self.host,
        }
    }
}

/// [`KeyLayers`] for every row of [`SETTINGS`], in its order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Layers(pub(crate) Vec<KeyLayers>);

impl Layers {
    /// Nothing written anywhere.
    pub(crate) fn defaults() -> Layers {
        Layers(
            SETTINGS
                .iter()
                .map(|s| KeyLayers {
                    default: s.default_value(),
                    global: None,
                    host: None,
                })
                .collect(),
        )
    }

    /// The winners, as typed settings.
    pub(crate) fn settings(&self) -> Settings {
        in_effect(self, &Pending::new())
    }
}

/// What the file says for one target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub(crate) settings: Settings,
    pub(crate) layers: Layers,
    /// One line each, for the activity log. Never printed.
    pub(crate) warnings: Vec<String>,
}

/// Resolve the file's text for `target`, the ssh target exactly as typed.
/// `None` is a missing file: all defaults, and no warning. Pure.
pub(crate) fn resolve(text: Option<&str>, target: &str) -> Resolved {
    let mut layers = Layers::defaults();
    let mut warnings = Vec::new();
    if let Some(text) = text {
        match text.parse::<DocumentMut>() {
            Ok(doc) => read_document(&doc, target, &mut layers, &mut warnings),
            Err(e) => warnings.push(format!(
                "{FILE} is not valid TOML ({}), so every setting is its default",
                parse_error(&e)
            )),
        }
    }
    keep_the_popup_reachable(&mut layers, &mut warnings);
    Resolved {
        settings: layers.settings(),
        layers,
        warnings,
    }
}

/// Read `dir/config.toml` and resolve it for `target`. A missing file, or
/// no directory to look in, is all defaults and no warning; a file that
/// cannot be read is one warning and all defaults.
pub(crate) fn load(dir: Option<&Path>, target: &str) -> Resolved {
    let Some(dir) = dir else {
        return resolve(None, target);
    };
    match std::fs::read_to_string(dir.join(FILE)) {
        Ok(text) => resolve(Some(&text), target),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => resolve(None, target),
        Err(e) => {
            let mut r = resolve(None, target);
            r.warnings.insert(
                0,
                format!("{FILE} could not be read ({e}), so every setting is its default"),
            );
            r
        }
    }
}

/// toml_edit's error, on one line: where, and what.
fn parse_error(e: &toml_edit::TomlError) -> String {
    let at = e.to_string().lines().next().unwrap_or_default().to_string();
    let at = at
        .strip_prefix("TOML parse error ")
        .unwrap_or(&at)
        .to_string();
    format!("{at}: {}", e.message())
}

/// `target` as a TOML key, for warnings: quoted.
fn quoted(target: &str) -> String {
    format!("{target:?}")
}

fn read_document(doc: &DocumentMut, target: &str, layers: &mut Layers, warnings: &mut Vec<String>) {
    for (key, item) in doc.as_table().iter() {
        if key != "host" {
            read_section("", key, item, Level::Global, layers, warnings);
            continue;
        }
        let Some(hosts) = item.as_table_like() else {
            warnings.push("host: expected a table of hosts".to_string());
            continue;
        };
        // Only this target's table is read: another host's keys are checked
        // when connecting to that host.
        let Some(ours) = hosts.get(target) else {
            continue;
        };
        let at = format!("host.{}", quoted(target));
        let Some(ours) = ours.as_table_like() else {
            warnings.push(format!("{at}: expected a table"));
            continue;
        };
        for (section, item) in ours.iter() {
            read_section(&at, section, item, Level::Host, layers, warnings);
        }
    }
}

fn read_section(
    prefix: &str,
    section: &str,
    item: &Item,
    level: Level,
    layers: &mut Layers,
    warnings: &mut Vec<String>,
) {
    let at = if prefix.is_empty() {
        section.to_string()
    } else {
        format!("{prefix}.{section}")
    };
    if !SETTINGS.iter().any(|s| s.section == section) {
        warnings.push(format!("{at}: unknown section"));
        return;
    }
    let Some(table) = item.as_table_like() else {
        warnings.push(format!("{at}: expected a table"));
        return;
    };
    for (name, item) in table.iter() {
        let Some(i) = index_of(section, name) else {
            warnings.push(format!("{at}.{name}: unknown setting"));
            continue;
        };
        match parse_item(SETTINGS[i].shape, item) {
            Ok(v) => *layers.0[i].at_mut(level) = Some(v),
            Err(why) => warnings.push(format!("{at}.{name}: {why}")),
        }
    }
}

/// `popup.key = "off"` with `popup.auto_open_after = "off"` would leave the
/// popup unreachable: the auto-open falls back a layer until it is not.
fn keep_the_popup_reachable(layers: &mut Layers, warnings: &mut Vec<String>) {
    while layers.settings().unreachable() {
        let auto = &mut layers.0[AUTO_OPEN];
        // The default opens it; never reached.
        let Some(level) = auto.winner().0.level() else {
            return;
        };
        *auto.at_mut(level) = None;
        warnings.push(
            "popup.auto_open_after = \"off\" with popup.key = \"off\" would leave the \
             popup unreachable; the next layer's auto_open_after is used"
                .to_string(),
        );
    }
}

/// One unsaved change on the screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Edit {
    /// A new value, for whichever level it is saved to.
    Set(Value),
    /// Take the value out at this level, so the next one shows through.
    Remove(Level),
}

/// The unsaved changes, by row of [`SETTINGS`].
pub(crate) type Pending = BTreeMap<usize, Edit>;

/// What a row shows with `edit` pending: its value, and where that value
/// comes from (`None` for a value that is only on the screen).
pub(crate) fn shown_with(layers: &KeyLayers, edit: Option<&Edit>) -> (Option<Origin>, Value) {
    match edit {
        Some(Edit::Set(v)) => (None, v.clone()),
        Some(Edit::Remove(level)) => {
            let (o, v) = layers.without(*level);
            (Some(o), v.clone())
        }
        None => {
            let (o, v) = layers.winner();
            (Some(o), v.clone())
        }
    }
}

/// The settings in effect with `pending` applied.
pub(crate) fn in_effect(layers: &Layers, pending: &Pending) -> Settings {
    let mut s = Settings::default();
    for (i, (row, l)) in SETTINGS.iter().zip(&layers.0).enumerate() {
        (row.set)(&mut s, shown_with(l, pending.get(&i)).1);
    }
    s
}

/// The popup could not be reached: the reason a change is refused.
pub(crate) const UNREACHABLE: &str =
    "refused: with popup.key and auto_open_after both off the popup could never open";

/// The config as the session holds it: the file's layers for this target,
/// and what has been changed on the screen since.
#[derive(Clone, Debug)]
pub(crate) struct ConfigState {
    /// Where the file is; `None` when there is nowhere to save it.
    pub(crate) dir: Option<PathBuf>,
    pub(crate) target: String,
    pub(crate) layers: Layers,
    /// How many warnings the file produced, for the screen's header.
    pub(crate) warnings: usize,
    pub(crate) pending: Pending,
    /// The settings the session runs with. Not derived from `layers`: a
    /// save re-reads the file into `layers` for the screen, and a hand edit
    /// made there since the connect must not ride in on the next edit.
    pub(crate) applied: Settings,
}

impl ConfigState {
    pub(crate) fn new(dir: Option<PathBuf>, target: &str, resolved: &Resolved) -> ConfigState {
        ConfigState {
            dir,
            target: target.to_owned(),
            layers: resolved.layers.clone(),
            warnings: resolved.warnings.len(),
            pending: Pending::new(),
            applied: resolved.layers.settings(),
        }
    }

    /// No file, no target: a session that was not reached over ssh.
    pub(crate) fn defaults() -> ConfigState {
        ConfigState::new(None, "", &resolve(None, ""))
    }

    /// Row `i` as the screen shows it: where its value comes from, the
    /// value, and whether it is changed and not saved.
    pub(crate) fn shown(&self, i: usize) -> (Option<Origin>, Value, bool) {
        let edit = self.pending.get(&i);
        let (origin, value) = shown_with(&self.layers.0[i], edit);
        (origin, value, edit.is_some())
    }

    /// `next` instead of the pending edits, which differ from them in row
    /// `i` only, unless it leaves the popup unreachable. Only row `i`
    /// changes in what is applied: whatever else the layers say since a
    /// save waits for the next connect. Returns the settings now in effect.
    fn commit(&mut self, i: usize, next: Pending) -> Result<Settings, String> {
        let mut s = self.applied.clone();
        (SETTINGS[i].set)(&mut s, shown_with(&self.layers.0[i], next.get(&i)).1);
        if s.unreachable() {
            return Err(UNREACHABLE.to_string());
        }
        self.pending = next;
        self.applied = s.clone();
        Ok(s)
    }

    /// Row `i` set to `value` on the screen. Setting it back to what the
    /// file says is no change at all.
    pub(crate) fn set(&mut self, i: usize, value: Value) -> Result<Settings, String> {
        let mut next = self.pending.clone();
        if *self.layers.0[i].winner().1 == value {
            next.remove(&i);
        } else {
            next.insert(i, Edit::Set(value));
        }
        self.commit(i, next)
    }

    /// `w`: the pending edits written to the file at `level`. The text
    /// written is resolved again for the screen only -- origins, layers and
    /// the warning count -- and nothing is applied from it: a hand edit made
    /// elsewhere meanwhile still waits for the next connect (spec §4.1). A
    /// failed save leaves the edits pending; `applied` is never touched.
    /// Returns the warnings of the text written, for the log.
    // Called by the screen's `w` from Task 13, which removes this attribute.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn save(&mut self, level: Level) -> anyhow::Result<Vec<String>> {
        if self.pending.is_empty() {
            anyhow::bail!("nothing has changed");
        }
        let Some(dir) = self.dir.as_deref() else {
            anyhow::bail!("there is no config directory: neither XDG_CONFIG_HOME nor HOME is set");
        };
        let text = save::save(dir, &self.target, level, &self.pending)?;
        let r = resolve(Some(&text), &self.target);
        self.layers = r.layers;
        self.warnings = r.warnings.len();
        self.pending.clear();
        Ok(r.warnings)
    }

    /// `x` on row `i`: an unsaved change is dropped; otherwise the value is
    /// marked for removal at the level it came from.
    pub(crate) fn reset(&mut self, i: usize) -> Result<Settings, String> {
        let mut next = self.pending.clone();
        if next.remove(&i).is_none() {
            let Some(level) = self.layers.0[i].winner().0.level() else {
                return Err("already the default".to_string());
            };
            next.insert(i, Edit::Remove(level));
        }
        self.commit(i, next)
    }
}

/// `docs/config.md`, generated from the table. Run by the test that keeps
/// the file in step with it.
#[cfg(test)]
pub(crate) fn docs() -> String {
    let mut out = String::from(
        "# oxutrm configuration\n\
         \n\
         <!-- Generated from src/config.rs by `OXUTRM_BLESS=1 cargo test \
         the_docs_are_generated_from_the_table`. Do not edit by hand. -->\n\
         \n\
         The client reads `$XDG_CONFIG_HOME/oxutrm/config.toml`, else \
         `~/.config/oxutrm/config.toml`, when it connects. A missing file is all \
         defaults; a broken one never stops a connect: each problem is a warning \
         in the status popup's log and in `client.log`, and the setting it is \
         about falls back to the next layer down.\n\
         \n\
         Every setting resolves through its built-in default, then the top-level \
         section, then `[host.\"<target>\"]`, where `<target>` is the ssh target \
         exactly as typed. Durations are strings: `\"20s\"`, `\"1m\"`, \
         `\"1m 30s\"`.\n\
         \n\
         The config screen (`c` in the status popup) changes a setting for the \
         running session and saves it, for every host (`w` `a`) or for this host \
         only (`w` `h`), keeping everything else in the file as written.\n\
         \n\
         | Key | Default | Range | Takes effect | |\n\
         |---|---|---|---|---|\n",
    );
    for s in SETTINGS {
        let effect = match s.applies {
            Applies::Now => "now",
            Applies::NextAttempt => "next rebuild attempt or standby search",
            Applies::NextConnect => "next connect",
        };
        // As the file would spell it.
        let default = match s.default_value() {
            Value::Bool(b) => b.to_string(),
            Value::List(l) => format!("{l:?}"),
            v => format!("{:?}", show(&v)),
        };
        out.push_str(&format!(
            "| `{}` | `{}` | {} | {} | {} |\n",
            s.key(),
            default.replace('|', "\\|"),
            match s.shape {
                Shape::Bool => "true or false".to_string(),
                shape => range(shape),
            }
            .replace('|', "\\|"),
            effect,
            s.help
        ));
    }
    for s in SETTINGS.iter().filter(|s| !s.doc.is_empty()) {
        out.push_str(&format!("\n**`{}`** -- {}\n", s.key(), s.doc));
    }
    out.push_str(
        "\n## Example\n\
         \n\
         ```toml\n\
         [popup]\n\
         key = \"ctrl-]\"\n\
         \n\
         [recovery]\n\
         rebuild_after = \"30s\"\n\
         \n\
         [host.\"thinlinc\"]\n\
         network.standby = false\n\
         ```\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(key: &str) -> usize {
        SETTINGS
            .iter()
            .position(|s| s.key() == key)
            .unwrap_or_else(|| panic!("no setting {key}"))
    }

    fn d(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    /// `docs/config.md` is what the table generates. Run with
    /// `OXUTRM_BLESS=1` to write it after changing the table.
    #[test]
    fn the_docs_are_generated_from_the_table() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/config.md");
        if std::env::var_os("OXUTRM_BLESS").is_some() {
            std::fs::write(&path, docs()).expect("writing docs/config.md");
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            on_disk == docs(),
            "docs/config.md is not what the table generates; run \
             `OXUTRM_BLESS=1 cargo test --workspace the_docs_are_generated_from_the_table`"
        );
    }

    #[test]
    fn every_setting_has_one_line_of_help_and_says_when_it_acts() {
        for s in SETTINGS {
            assert!(!s.help.contains('\n'), "{}", s.key());
            assert!(
                (1..=64).contains(&s.help.chars().count()),
                "{}: the help line must fit the box with its range",
                s.key()
            );
            assert!(s.doc.is_empty() || s.doc.ends_with('.'), "{}", s.key());
        }
        assert_eq!(SETTINGS[STANDBY].applies.note(), "");
        assert_eq!(SETTINGS[at("popup.splash")].applies.note(), "next connect");
        assert_eq!(
            SETTINGS[at("recovery.connect_timeout")].applies.note(),
            "next attempt"
        );
    }

    #[test]
    fn the_index_constants_name_their_rows() {
        assert_eq!(SETTINGS[POPUP_KEY].key(), "popup.key");
        assert_eq!(SETTINGS[AUTO_OPEN].key(), "popup.auto_open_after");
        assert_eq!(SETTINGS[STANDBY].key(), "network.standby");
    }

    #[test]
    fn a_missing_file_is_the_defaults_and_no_warning() {
        let r = resolve(None, "thinlinc");
        assert_eq!(r.settings, Settings::default());
        assert!(r.warnings.is_empty());
        assert_eq!(r.layers, Layers::defaults());
    }

    #[test]
    fn the_defaults_are_todays_constants() {
        let s = Settings::default();
        assert_eq!(s.popup_key, Some(0x1c));
        assert_eq!(s.auto_open_after, Some(d(2)));
        assert_eq!(s.linger, d(3));
        assert_eq!(
            (s.silent_after, s.rebuild_after, s.connect_timeout),
            (d(2), d(20), d(10))
        );
        assert!(s.splash && s.standby && s.port_mapping && s.birthday);
        assert_eq!(
            s.stun_servers,
            oxutrm_net::NetConfig::default().stun_servers
        );
    }

    #[test]
    fn a_host_override_wins_and_every_layer_is_kept() {
        let r = resolve(
            Some(
                "[recovery]\nrebuild_after = \"25s\"\n\
                 [host.\"thinlinc\"]\nrecovery.rebuild_after = \"30s\"\n",
            ),
            "thinlinc",
        );
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert_eq!(r.settings.rebuild_after, d(30));
        let l = &r.layers.0[at("recovery.rebuild_after")];
        assert_eq!(l.default, Value::Duration(Some(d(20))));
        assert_eq!(l.global, Some(Value::Duration(Some(d(25)))));
        assert_eq!(l.host, Some(Value::Duration(Some(d(30)))));
        assert_eq!(l.winner().0, Origin::Host);
        assert_eq!(
            l.without(Level::Host),
            (Origin::Global, &Value::Duration(Some(d(25))))
        );
    }

    #[test]
    fn dotted_subtable_and_inline_spellings_resolve_alike() {
        for text in [
            "[host.\"t\"]\nnetwork.standby = false\n",
            "[host.\"t\".network]\nstandby = false\n",
            "[host.\"t\"]\nnetwork = { standby = false }\n",
            "host.\"t\".network.standby = false\n",
        ] {
            let r = resolve(Some(text), "t");
            assert!(r.warnings.is_empty(), "{text}: {:?}", r.warnings);
            assert!(!r.settings.standby, "{text}");
            assert_eq!(r.layers.0[STANDBY].winner().0, Origin::Host, "{text}");
        }
    }

    #[test]
    fn a_host_table_for_another_target_is_ignored() {
        let r = resolve(
            Some("[host.\"other\"]\nnetwork.standby = false\n"),
            "thinlinc",
        );
        assert!(r.settings.standby);
        assert!(r.warnings.is_empty());
        // As typed: no aliasing, no case folding.
        let r = resolve(
            Some("[host.\"ThinLinc\"]\nnetwork.standby = false\n"),
            "thinlinc",
        );
        assert!(r.settings.standby);
    }

    /// A target named like a setting must not read a global section as its
    /// own host table.
    #[test]
    fn a_target_named_like_a_setting_does_not_read_a_global_section() {
        let r = resolve(Some("[popup]\nkey = \"ctrl-a\"\n"), "key");
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert_eq!(r.settings.popup_key, Some(0x01));
    }

    #[test]
    fn the_effective_values_are_raised_to_silent_after_and_the_written_ones_kept() {
        let r = resolve(
            Some(
                "[popup]\nauto_open_after = \"1s\"\n\
                 [recovery]\nsilent_after = \"8s\"\nrebuild_after = \"5s\"\n",
            ),
            "t",
        );
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert_eq!(r.settings.auto_open_after, Some(d(1)));
        assert_eq!(r.settings.effective_auto_open(), Some(d(8)));
        assert_eq!(r.settings.rebuild_after, d(5));
        assert_eq!(r.settings.effective_rebuild_after(), d(8));
    }

    #[test]
    fn both_keys_off_warns_and_auto_open_falls_back_a_layer() {
        let r = resolve(
            Some("[popup]\nkey = \"off\"\nauto_open_after = \"off\"\n"),
            "t",
        );
        assert_eq!(r.settings.popup_key, None);
        assert_eq!(
            r.settings.auto_open_after,
            Some(d(2)),
            "fell back to the default"
        );
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("unreachable"), "{:?}", r.warnings);

        let r = resolve(
            Some(
                "[popup]\nkey = \"off\"\nauto_open_after = \"5s\"\n\
                 [host.\"t\"]\npopup.auto_open_after = \"off\"\n",
            ),
            "t",
        );
        assert_eq!(
            r.settings.auto_open_after,
            Some(d(5)),
            "fell back to the global"
        );
    }

    #[test]
    fn a_bad_value_warns_and_falls_back_to_the_next_layer_down() {
        let cases = [
            ("[popup]\nkey = \"ctrl-m\"\n", "popup.key", "Enter"),
            ("[popup]\nkey = \"x\"\n", "popup.key", "not a key"),
            ("[popup]\nlinger = 3\n", "popup.linger", "expected a string"),
            ("[popup]\nlinger = \"2m\"\n", "popup.linger", "outside"),
            (
                "[popup]\nsplash = \"yes\"\n",
                "popup.splash",
                "true or false",
            ),
            (
                "[recovery]\nconnect_timeout = \"1.5s\"\n",
                "recovery.connect_timeout",
                "whole",
            ),
            (
                "[recovery]\nsilent_after = \"-3s\"\n",
                "recovery.silent_after",
                "negative",
            ),
            (
                "[recovery]\nrebuild_after = \"soon\"\n",
                "recovery.rebuild_after",
                "not a duration",
            ),
            (
                "[network]\nstun_servers = [\"nope\"]\n",
                "network.stun_servers",
                "host:port",
            ),
            (
                "[network]\nstun_servers = \"a:1\"\n",
                "network.stun_servers",
                "list",
            ),
        ];
        for (text, key, why) in cases {
            let r = resolve(Some(text), "t");
            assert_eq!(r.settings, Settings::default(), "{text}");
            assert_eq!(r.warnings.len(), 1, "{text}: {:?}", r.warnings);
            assert!(
                r.warnings[0].starts_with(key) && r.warnings[0].contains(why),
                "{text}: {:?}",
                r.warnings
            );
        }
        // Falls back to the global, not to the default, under a bad host value.
        let r = resolve(
            Some("[popup]\nlinger = \"5s\"\n[host.\"t\"]\npopup.linger = \"forever\"\n"),
            "t",
        );
        assert_eq!(r.settings.linger, d(5));
        assert_eq!(r.warnings.len(), 1);
    }

    #[test]
    fn unknown_keys_and_sections_warn() {
        let r = resolve(
            Some("[popup]\nlingr = \"5s\"\n[recover]\nx = 1\n[host.\"t\".popup]\nkeys = \"off\"\n"),
            "t",
        );
        assert_eq!(
            r.warnings,
            [
                "popup.lingr: unknown setting",
                "recover: unknown section",
                "host.\"t\".popup.keys: unknown setting",
            ]
        );
    }

    #[test]
    fn a_file_that_is_not_toml_is_one_warning_and_the_defaults() {
        let r = resolve(Some("[popup\nkey = \"off\"\n"), "t");
        assert_eq!(r.settings, Settings::default());
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(r.warnings[0].contains("not valid TOML"), "{:?}", r.warnings);
        assert!(!r.warnings[0].contains('\n'), "{:?}", r.warnings);
    }

    /// Review focus 2. An editor that saves with a byte-order mark and CRLF
    /// line endings writes a file that is read as written.
    #[test]
    fn a_file_with_a_bom_and_crlf_line_endings_is_read_as_written() {
        let r = resolve(Some("\u{feff}[popup]\r\nlinger = \"5s\"\r\n"), "t");
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert_eq!(r.settings.linger, d(5));
    }

    #[test]
    fn keys_parse_and_print_alike() {
        for (text, byte) in [
            ("ctrl-a", Some(0x01)),
            ("ctrl-\\", Some(0x1c)),
            ("ctrl-]", Some(0x1d)),
            ("ctrl-^", Some(0x1e)),
            ("ctrl-_", Some(0x1f)),
            ("off", None),
        ] {
            assert_eq!(parse_key(text), Ok(byte), "{text}");
            assert_eq!(key_name(byte), text);
        }
        assert_eq!(parse_key("ctrl-A"), Ok(Some(0x01)));
        for refused in [
            "ctrl-h", "ctrl-i", "ctrl-j", "ctrl-m", "ctrl-[", "alt-x", "ctrl-ab", "",
        ] {
            assert!(parse_key(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn durations_parse_in_jiffs_friendly_format() {
        let linger = SETTINGS[at("popup.linger")].shape;
        assert_eq!(parse_text(linger, "20s"), Ok(Value::Duration(Some(d(20)))));
        assert_eq!(parse_text(linger, "1m"), Ok(Value::Duration(Some(d(60)))));
        assert_eq!(
            parse_text(linger, "1.5s"),
            Ok(Value::Duration(Some(Duration::from_millis(1500))))
        );
        assert!(parse_text(linger, "off").is_err(), "linger has no off");
        let auto = SETTINGS[AUTO_OPEN].shape;
        assert_eq!(parse_text(auto, "off"), Ok(Value::Duration(None)));
        assert_eq!(show_duration(d(90)), "1m 30s");
        assert_eq!(show_duration(Duration::from_millis(1500)), "1s 500ms");
    }

    #[test]
    fn servers_must_name_a_port() {
        assert!(
            check_servers(vec![
                "stun.example.net:3478".into(),
                "[2001:db8::1]:3478".into()
            ])
            .is_ok()
        );
        for bad in [
            "stun.example.net",
            ":3478",
            "a:0",
            "a:99999",
            "2001:db8::1:3478",
            "[]:1",
        ] {
            assert!(check_servers(vec![bad.into()]).is_err(), "{bad}");
        }
        assert!(check_servers(vec![]).is_ok(), "an empty list is allowed");
        assert!(check_servers(vec!["a:1".into(); 17]).is_err());
    }

    #[test]
    fn the_config_dir_prefers_xdg_and_falls_back_to_home() {
        let p = |x: Option<&str>, h: Option<&str>| config_dir(x.map(Into::into), h.map(Into::into));
        assert_eq!(
            p(Some("/x/cfg"), Some("/home/u")),
            Some(PathBuf::from("/x/cfg/oxutrm"))
        );
        assert_eq!(
            p(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/oxutrm"))
        );
        assert_eq!(
            p(Some("rel/cfg"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/oxutrm"))
        );
        assert_eq!(
            p(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/oxutrm"))
        );
        assert_eq!(p(Some(""), Some("")), None);
        assert_eq!(p(None, None), None);
    }

    #[test]
    fn load_reads_the_file_in_the_directory_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(Some(dir.path()), "t").settings, Settings::default());
        std::fs::write(dir.path().join(FILE), "[popup]\nsplash = false\n").unwrap();
        assert!(!load(Some(dir.path()), "t").settings.splash);
        assert_eq!(load(None, "t").settings, Settings::default());
    }

    /// Review focus 4. A `config.toml` that is a directory is one warning
    /// and the defaults, not a failed connect.
    #[test]
    fn a_config_file_that_cannot_be_read_is_one_warning() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(FILE)).unwrap();
        let r = load(Some(dir.path()), "t");
        assert_eq!(r.settings, Settings::default());
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(
            r.warnings[0].contains("could not be read"),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn the_net_config_carries_the_three_network_settings() {
        let s = Settings {
            stun_servers: vec!["a:1".into()],
            port_mapping: false,
            birthday: false,
            ..Settings::default()
        };
        let c = s.net_config();
        assert_eq!(c.stun_servers, ["a:1"]);
        assert!(!c.enable_port_mapping && !c.enable_birthday);
        assert_eq!(c.prefer_port, oxutrm_net::NetConfig::default().prefer_port);
    }

    #[test]
    fn every_default_is_inside_its_own_range() {
        for s in SETTINGS {
            let v = s.default_value();
            let text = match &v {
                Value::List(l) => l.join(","),
                other => show(other),
            };
            assert_eq!(parse_text(s.shape, &text), Ok(v), "{}", s.key());
        }
    }

    // ---- the screen's edits ----

    fn state(text: &str) -> ConfigState {
        ConfigState::new(None, "t", &resolve(Some(text), "t"))
    }

    #[test]
    fn an_edit_is_in_effect_and_marked_until_it_is_set_back() {
        let mut c = state("");
        let row = at("recovery.rebuild_after");
        let s = c.set(row, Value::Duration(Some(d(30)))).unwrap();
        assert_eq!(s.rebuild_after, d(30));
        assert_eq!(c.shown(row), (None, Value::Duration(Some(d(30))), true));
        c.set(row, Value::Duration(Some(d(20)))).unwrap();
        assert_eq!(
            c.shown(row),
            (Some(Origin::Default), Value::Duration(Some(d(20))), false)
        );
    }

    #[test]
    fn x_removes_at_the_level_the_value_came_from() {
        let mut c = state("[popup]\nlinger = \"5s\"\n[host.\"t\"]\npopup.linger = \"7s\"\n");
        let row = at("popup.linger");
        let s = c.reset(row).unwrap();
        assert_eq!(s.linger, d(5), "the global shows through");
        assert_eq!(c.pending.get(&row), Some(&Edit::Remove(Level::Host)));
        assert_eq!(c.shown(row).0, Some(Origin::Global));
        // x again drops the unsaved removal.
        c.reset(row).unwrap();
        assert!(c.pending.is_empty());
        let mut c = state("");
        assert!(c.reset(row).is_err(), "x on a default");
    }

    #[test]
    fn a_change_that_leaves_the_popup_unreachable_is_refused() {
        let mut c = state("[popup]\nkey = \"off\"\n");
        assert_eq!(
            c.set(AUTO_OPEN, Value::Duration(None)),
            Err(UNREACHABLE.to_string())
        );
        assert!(c.pending.is_empty());
        // Reached by x: the host's key override removed leaves the global off.
        let mut c = state(
            "[popup]\nkey = \"off\"\nauto_open_after = \"off\"\n\
             [host.\"t\"]\npopup.key = \"ctrl-]\"\n",
        );
        assert_eq!(c.reset(POPUP_KEY), Err(UNREACHABLE.to_string()));
    }

    /// `w` (spec §4.1): what was written is resolved again for the screen
    /// only -- layers, origins, the warning count -- and a save that fails
    /// leaves the edits pending.
    #[test]
    fn a_save_rebuilds_the_screen_from_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, "[popup]\nlinger = \"1s\"\n").unwrap();
        let r = load(Some(dir.path()), "t");
        let mut c = ConfigState::new(Some(dir.path().to_path_buf()), "t", &r);
        let linger = at("popup.linger");
        let five = Value::Duration(Some(Duration::from_secs(5)));
        c.set(linger, five.clone()).unwrap();
        // A hand edit made elsewhere since the connect, with a typo in it.
        std::fs::write(
            &path,
            "[popup]\nlinger = \"1s\"\nsplash = false\nsplosh = 1\n",
        )
        .unwrap();
        let warnings = c.save(Level::Host).unwrap();
        assert!(c.pending.is_empty());
        assert_eq!(c.shown(linger), (Some(Origin::Host), five, false));
        assert_eq!(c.shown(at("popup.splash")).0, Some(Origin::Global));
        assert_eq!(c.warnings, 1, "the typo was not counted");
        // Returned for the log: the typo's warning names the key.
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("splosh"), "{warnings:?}");

        assert!(c.save(Level::Global).is_err(), "saved nothing");

        let seven = Value::Duration(Some(Duration::from_secs(7)));
        c.set(linger, seven.clone()).unwrap();
        std::fs::write(&path, "[popup\n").unwrap();
        assert!(c.save(Level::Global).is_err(), "a broken file was replaced");
        assert_eq!(c.pending.len(), 1, "a failed save dropped the edit");

        let mut nowhere = ConfigState::defaults();
        nowhere.set(linger, seven).unwrap();
        let e = nowhere.save(Level::Global).unwrap_err();
        assert!(format!("{e:#}").contains("no config directory"), "{e:#}");
    }

    /// After a save the written text is re-resolved for the screen only
    /// (spec §4.1): a hand edit of another key that the save brought in is
    /// shown, but the next edit on the screen does not apply it -- it waits
    /// for the next connect.
    #[test]
    fn a_hand_edit_a_save_brought_in_is_shown_but_not_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, "[popup]\nlinger = \"1s\"\n").unwrap();
        let r = load(Some(dir.path()), "t");
        let mut c = ConfigState::new(Some(dir.path().to_path_buf()), "t", &r);
        let rebuild = at("recovery.rebuild_after");
        let before = r.settings.rebuild_after;
        c.set(at("popup.linger"), Value::Duration(Some(d(5))))
            .unwrap();
        // Meanwhile, by hand, another key.
        std::fs::write(
            &path,
            "[popup]\nlinger = \"1s\"\n[recovery]\nrebuild_after = \"40s\"\n",
        )
        .unwrap();
        c.save(Level::Global).unwrap();
        assert_eq!(
            c.shown(rebuild),
            (Some(Origin::Global), Value::Duration(Some(d(40))), false)
        );
        // A third key edited on the screen.
        let s = c.set(at("popup.splash"), Value::Bool(false)).unwrap();
        assert!(!s.splash);
        assert_eq!(s.linger, d(5), "the saved edit is still in effect");
        assert_eq!(s.rebuild_after, before, "the hand edit was applied");
        assert_eq!(c.applied, s);
    }
}

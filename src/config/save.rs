//! Writing the config file back: only the keys the screen changed, at the
//! level the user chose, with every comment and every other key kept as
//! written, through a temporary file renamed into place (spec §4.4).

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item};

use super::{
    Edit, FILE, Level, Pending, SETTINGS, Value, key_name, parse_error, quoted, show_duration,
};

/// A value as the file writes it.
fn to_toml(value: &Value) -> toml_edit::Value {
    match value {
        Value::Bool(b) => (*b).into(),
        Value::Duration(None) | Value::Key(None) => "off".into(),
        Value::Duration(Some(d)) => show_duration(*d).into(),
        Value::Key(k) => key_name(*k).into(),
        Value::List(l) => l.iter().collect::<toml_edit::Array>().into(),
    }
}

/// One change to the file.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Change {
    Write(Level, usize, Value),
    Remove(Level, usize),
}

/// What saving `pending` at `level` writes (spec §4.1, `w`).
fn changes(level: Level, pending: &Pending) -> Vec<Change> {
    let mut out = Vec::new();
    for (&i, edit) in pending {
        match (level, edit) {
            // For every host -- and so for this one next time too: this
            // target's own override of the key goes with it.
            (Level::Global, Edit::Set(v)) => {
                out.push(Change::Write(Level::Global, i, v.clone()));
                out.push(Change::Remove(Level::Host, i));
            }
            (Level::Host, Edit::Set(v)) => out.push(Change::Write(Level::Host, i, v.clone())),
            (_, Edit::Remove(Level::Host)) => out.push(Change::Remove(Level::Host, i)),
            (Level::Global, Edit::Remove(Level::Global)) => {
                out.push(Change::Remove(Level::Global, i));
            }
            // "This host only" never changes another host: the global value
            // stays where it is, and this host gets the default in front.
            (Level::Host, Edit::Remove(Level::Global)) => {
                out.push(Change::Write(Level::Host, i, SETTINGS[i].default_value()));
            }
        }
    }
    out
}

/// Write `pending` into `dir/config.toml` at `level`, changing nothing
/// else in it, and return the text written.
///
/// The file is read again first, so a hand edit made since the connect is
/// kept. A file that no longer parses is refused rather than replaced.
pub(crate) fn save(
    dir: &Path,
    target: &str,
    level: Level,
    pending: &Pending,
) -> anyhow::Result<String> {
    use anyhow::Context as _;
    let path = real_path(&dir.join(FILE))?;
    let old = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc = match old.as_deref() {
        None => DocumentMut::new(),
        Some(text) => text.parse::<DocumentMut>().map_err(|e| {
            anyhow::anyhow!(
                "{FILE} is not valid TOML ({}); fix it by hand first",
                parse_error(&e)
            )
        })?,
    };
    for change in changes(level, pending) {
        apply_change(&mut doc, target, &change)?;
    }
    let text = doc.to_string();
    write_atomically(&path, old.is_some(), &text)?;
    Ok(text)
}

/// The file a save replaces: `path`, or what its chain of symlinks ends at.
/// `read_link` and not `canonicalize`, which fails on a dangling link --
/// and a dangling link's target is the file to create.
fn real_path(path: &Path) -> anyhow::Result<PathBuf> {
    let mut path = path.to_path_buf();
    for _ in 0..16 {
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                let to = std::fs::read_link(&path)?;
                // A relative link is relative to the link's own directory;
                // `join` with an absolute one is that one.
                path = match path.parent() {
                    Some(dir) => dir.join(to),
                    None => to,
                };
            }
            _ => return Ok(path),
        }
    }
    anyhow::bail!("{} is a chain of more than 16 symlinks", path.display())
}

/// How a table the saver has to create is written.
#[derive(Clone, Copy)]
enum NewTable {
    /// `[host."t"]`.
    Header,
    /// `host`: only there to hold `[host."t"]`, never written on its own.
    Implicit,
    /// `network.standby = false` under the header it is in.
    Dotted,
}

/// The table at `key` in `parent`, created as `create` says if it is not
/// there, or `None` when it is not there and nothing is to be created.
/// `get_mut` and `as_table_like_mut` only: toml_edit's indexing panics on
/// a value that is not a table, and a hand-written `network = "x"` is a
/// failed save, never a panic.
fn table_in<'a>(
    parent: &'a mut dyn toml_edit::TableLike,
    key: &str,
    create: Option<NewTable>,
    at: &str,
) -> anyhow::Result<Option<&'a mut dyn toml_edit::TableLike>> {
    if parent.get(key).is_none() {
        let Some(kind) = create else {
            return Ok(None);
        };
        let mut t = toml_edit::Table::new();
        match kind {
            NewTable::Header => {}
            NewTable::Implicit => t.set_implicit(true),
            NewTable::Dotted => t.set_dotted(true),
        }
        parent.insert(key, Item::Table(t));
    }
    match parent.get_mut(key).and_then(Item::as_table_like_mut) {
        Some(t) => Ok(Some(t)),
        None => anyhow::bail!("{at} in {FILE} is not a table, so it cannot be changed"),
    }
}

/// `value` at `name` in `table`. An existing value is replaced in place, so
/// the comments around it stay: toml_edit's `insert` drops them.
fn put(table: &mut dyn toml_edit::TableLike, name: &str, value: toml_edit::Value) {
    if let Some(old) = table.get_mut(name).and_then(Item::as_value_mut) {
        let decor = old.decor().clone();
        *old = value;
        *old.decor_mut() = decor;
    } else {
        table.insert(name, Item::Value(value));
    }
}

fn apply_change(doc: &mut DocumentMut, target: &str, change: &Change) -> anyhow::Result<()> {
    let (level, i) = match change {
        Change::Write(level, i, _) | Change::Remove(level, i) => (*level, *i),
    };
    let row = &SETTINGS[i];
    let write = matches!(change, Change::Write(..));
    let create = |kind| write.then_some(kind);
    let root: &mut dyn toml_edit::TableLike = doc.as_table_mut();
    let section = match level {
        Level::Global => table_in(root, row.section, create(NewTable::Header), row.section)?,
        Level::Host => {
            let at = format!("host.{}", quoted(target));
            let Some(hosts) = table_in(root, "host", create(NewTable::Implicit), "host")? else {
                return Ok(());
            };
            let Some(ours) = table_in(hosts, target, create(NewTable::Header), &at)? else {
                return Ok(());
            };
            let at = format!("{at}.{}", row.section);
            table_in(ours, row.section, create(NewTable::Dotted), &at)?
        }
    };
    let Some(section) = section else {
        return Ok(());
    };
    match change {
        Change::Write(_, _, v) => put(section, row.name, to_toml(v)),
        Change::Remove(..) => {
            section.remove(row.name);
        }
    }
    if level == Level::Host && !write {
        prune(doc, target, row.section);
    }
    Ok(())
}

/// After a removal from this host's table: a section left empty goes, then
/// the host's table if that left it empty, then `host` itself. Only the
/// host's own tables: a global section keeps its header and its comments.
fn prune(doc: &mut DocumentMut, target: &str, section: &str) {
    let root = doc.as_table_mut();
    let Some(hosts) = root.get_mut("host").and_then(Item::as_table_like_mut) else {
        return;
    };
    if let Some(ours) = hosts.get_mut(target).and_then(Item::as_table_like_mut) {
        if ours
            .get(section)
            .and_then(Item::as_table_like)
            .is_some_and(|t| t.is_empty())
        {
            ours.remove(section);
        }
        if ours.is_empty() {
            hosts.remove(target);
        }
    }
    if hosts.is_empty() {
        root.remove("host");
    }
}

/// Write `text` to `path` through a temporary file renamed over it, so a
/// crash leaves the old file or the new one and never half of either.
fn write_atomically(path: &Path, exists: bool, text: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    // A rename needs only the directory to be writable, so a read-only file
    // would be replaced without complaint. It is refused instead.
    let mode = if exists {
        rustix::fs::access(path, rustix::fs::Access::WRITE_OK)
            .with_context(|| format!("{} is read-only", path.display()))?;
        Some(std::fs::metadata(path)?.permissions().mode())
    } else {
        None
    };
    let dir = path.parent().context("the config file has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(
        ".{name}.{}.{:016x}.tmp",
        std::process::id(),
        random()
    ));
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        if let Some(mode) = mode {
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        // Our own temporary file, never the config.
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

/// A number for the temporary file's name that another save, by this
/// process or another, is very unlikely to pick: std's per-process random
/// hash keys over the clock.
fn random() -> u64 {
    use std::hash::BuildHasher as _;
    std::collections::hash_map::RandomState::new().hash_one(std::time::SystemTime::now())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::{Origin, STANDBY, parse_text, resolve};

    fn at(key: &str) -> usize {
        SETTINGS
            .iter()
            .position(|s| format!("{}.{}", s.section, s.name) == key)
            .unwrap_or_else(|| panic!("no setting {key}"))
    }

    fn pending(edits: &[(&str, Edit)]) -> Pending {
        edits.iter().map(|(k, e)| (at(k), e.clone())).collect()
    }

    fn set(key: &'static str, text: &str) -> (&'static str, Edit) {
        let v = parse_text(SETTINGS[at(key)].shape, text).unwrap();
        (key, Edit::Set(v))
    }

    fn saved(before: &str, target: &str, level: Level, edits: &[(&str, Edit)]) -> String {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), before).unwrap();
        let text = save(dir.path(), target, level, &pending(edits)).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(FILE)).unwrap(),
            text
        );
        text
    }

    fn rebuild_after(text: &str) -> Edit {
        Edit::Set(parse_text(SETTINGS[at("recovery.rebuild_after")].shape, text).unwrap())
    }

    #[test]
    fn a_save_changes_the_key_and_keeps_everything_else_as_written() {
        let before = "# my settings\n\n[popup]\n# the key\nkey = \"ctrl-]\" # not ctrl-\\\\\n\n\
                      [recovery]\n# long VPN settle\nrebuild_after = \"25s\" # was 20\nsilent_after = \"2s\"\n";
        let after = saved(
            before,
            "t",
            Level::Global,
            &[("recovery.rebuild_after", rebuild_after("40s"))],
        );
        assert_eq!(after, before.replace("\"25s\"", "\"40s\""));
    }

    #[test]
    fn a_global_save_writes_the_section_and_drops_this_targets_override() {
        let before = "[host.\"t\"]\nrecovery.rebuild_after = \"30s\"\nnetwork.standby = false\n\
                      [host.\"other\"]\nrecovery.rebuild_after = \"50s\"\n";
        let after = saved(
            before,
            "t",
            Level::Global,
            &[("recovery.rebuild_after", rebuild_after("40s"))],
        );
        let r = resolve(Some(&after), "t");
        assert_eq!(r.settings.rebuild_after, Duration::from_secs(40));
        assert_eq!(
            r.layers.0[at("recovery.rebuild_after")].winner().0,
            Origin::Global
        );
        assert!(!r.settings.standby, "another key of this host was lost");
        let other = resolve(Some(&after), "other");
        assert_eq!(
            other.settings.rebuild_after,
            Duration::from_secs(50),
            "another host changed"
        );
    }

    #[test]
    fn a_host_save_writes_dotted_keys_under_the_hosts_own_header() {
        let after = saved(
            "",
            "thinlinc",
            Level::Host,
            &[("network.standby", Edit::Set(Value::Bool(false)))],
        );
        assert_eq!(after, "[host.thinlinc]\nnetwork.standby = false\n");
        assert!(!resolve(Some(&after), "thinlinc").settings.standby);
    }

    /// Review focus 1. An ssh target is not a bare TOML key -- a user, a
    /// domain, a port, an IPv6 literal, even a quote. What is saved for it is
    /// read back for it, and for nothing else.
    #[test]
    fn a_target_that_needs_quoting_round_trips() {
        for target in [
            "tobi@bastion.example.com",
            "bastion.example.com:2222",
            "[2001:db8::7]",
            "say \"hi\"",
            "host.\"x\"",
        ] {
            let after = saved(
                "",
                target,
                Level::Host,
                &[("network.standby", Edit::Set(Value::Bool(false)))],
            );
            let r = resolve(Some(&after), target);
            assert!(r.warnings.is_empty(), "{target}: {:?}", r.warnings);
            assert!(!r.settings.standby, "{target}: {after}");
            assert!(
                resolve(Some(&after), "bastion").settings.standby,
                "{target}: {after}"
            );
            // And a second save finds the table the first one wrote.
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(FILE), &after).unwrap();
            let again = save(
                dir.path(),
                target,
                Level::Host,
                &pending(&[("network.standby", Edit::Remove(Level::Host))]),
            )
            .unwrap();
            assert_eq!(again.trim(), "", "{target}: {again}");
        }
    }

    #[test]
    fn x_removes_a_key_at_its_own_level_and_an_emptied_host_table_goes() {
        let before = "[network]\nstandby = true\n\n[host.\"t\"]\nnetwork.standby = false\n";
        let after = saved(
            before,
            "t",
            Level::Host,
            &[("network.standby", Edit::Remove(Level::Host))],
        );
        assert_eq!(after, "[network]\nstandby = true\n");
        let after = saved(
            before,
            "t",
            Level::Global,
            &[("network.standby", Edit::Remove(Level::Global))],
        );
        assert_eq!(
            after,
            "[network]\n\n[host.\"t\"]\nnetwork.standby = false\n"
        );
    }

    /// `x` on a global value, saved for this host only: a host override set
    /// to the default, and the global key untouched.
    #[test]
    fn x_on_a_global_value_saved_for_this_host_writes_the_default_there() {
        let before = "[network]\nstandby = false\n";
        let after = saved(
            before,
            "t",
            Level::Host,
            &[("network.standby", Edit::Remove(Level::Global))],
        );
        assert!(after.starts_with(before), "{after}");
        let r = resolve(Some(&after), "t");
        assert!(r.settings.standby);
        assert_eq!(r.layers.0[STANDBY].winner().0, Origin::Host);
        assert!(
            !resolve(Some(&after), "other").settings.standby,
            "another host changed"
        );
    }

    #[test]
    fn keys_written_dotted_as_subtables_or_inline_are_edited_where_they_are() {
        for before in [
            "popup.linger = \"1s\"\n",
            "[popup]\nlinger = \"1s\"\n",
            "popup = { linger = \"1s\" }\n",
            "[host.\"t\".popup]\nlinger = \"1s\"\n",
            "[host.\"t\"]\npopup = { linger = \"1s\" }\n",
            "host = { t = { popup = { linger = \"1s\" } } }\n",
        ] {
            let level = if before.contains("host") {
                Level::Host
            } else {
                Level::Global
            };
            let after = saved(before, "t", level, &[set("popup.linger", "5s")]);
            assert_eq!(after.matches("linger").count(), 1, "{before} -> {after}");
            let r = resolve(Some(&after), "t");
            assert!(
                r.warnings.is_empty(),
                "{before} -> {after}: {:?}",
                r.warnings
            );
            assert_eq!(
                r.settings.linger,
                Duration::from_secs(5),
                "{before} -> {after}"
            );
        }
    }

    #[test]
    fn a_file_changed_since_the_connect_keeps_the_other_change() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "[popup]\nsplash = false\n").unwrap();
        let text = save(
            dir.path(),
            "t",
            Level::Global,
            &pending(&[set("popup.linger", "5s")]),
        )
        .unwrap();
        let r = resolve(Some(&text), "t");
        assert!(!r.settings.splash, "the hand edit was lost: {text}");
        assert_eq!(r.settings.linger, Duration::from_secs(5));
    }

    #[test]
    fn a_save_through_a_symlink_changes_the_target_and_keeps_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        std::fs::write(dotfiles.join("oxutrm.toml"), "[popup]\nsplash = false\n").unwrap();
        let cfg = dir.path().join("cfg");
        std::fs::create_dir(&cfg).unwrap();
        std::os::unix::fs::symlink("../dotfiles/oxutrm.toml", cfg.join(FILE)).unwrap();

        save(
            &cfg,
            "t",
            Level::Global,
            &pending(&[set("popup.linger", "5s")]),
        )
        .unwrap();
        assert!(
            std::fs::symlink_metadata(cfg.join(FILE))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let text = std::fs::read_to_string(dotfiles.join("oxutrm.toml")).unwrap();
        assert_eq!(text, "[popup]\nsplash = false\nlinger = \"5s\"\n");
        let leftovers: Vec<_> = std::fs::read_dir(&dotfiles).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "a temporary file was left behind");
    }

    #[test]
    fn a_dangling_symlinks_target_is_created_and_the_link_kept() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere/oxutrm.toml");
        std::os::unix::fs::symlink(&target, dir.path().join(FILE)).unwrap();
        save(
            dir.path(),
            "t",
            Level::Global,
            &pending(&[set("popup.linger", "5s")]),
        )
        .unwrap();
        assert!(
            std::fs::symlink_metadata(dir.path().join(FILE))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "[popup]\nlinger = \"5s\"\n"
        );
    }

    #[test]
    fn a_read_only_file_is_refused_and_untouched() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return; // root writes through any mode bits
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, "[popup]\nsplash = false\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let e = save(
            dir.path(),
            "t",
            Level::Global,
            &pending(&[set("popup.linger", "5s")]),
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("read-only"), "{e:#}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[popup]\nsplash = false\n"
        );
    }

    #[test]
    fn the_files_mode_is_kept() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        save(
            dir.path(),
            "t",
            Level::Global,
            &pending(&[set("popup.linger", "5s")]),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn the_first_save_creates_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config/oxutrm");
        save(
            &cfg,
            "t",
            Level::Global,
            &pending(&[set("popup.linger", "5s")]),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(cfg.join(FILE)).unwrap(),
            "[popup]\nlinger = \"5s\"\n"
        );
    }

    #[test]
    fn an_invalid_file_or_a_type_conflict_fails_the_save_and_touches_nothing() {
        for (before, why) in [
            ("[popup\n", "not valid TOML"),
            ("network = \"x\"\n", "not a table"),
            ("popup = 3\n", "not a table"),
            ("host = \"x\"\n", "not a table"),
            ("[host]\nt = 1\n", "not a table"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(FILE);
            std::fs::write(&path, before).unwrap();
            let edits = pending(&[
                set("popup.linger", "5s"),
                ("network.standby", Edit::Set(Value::Bool(false))),
            ]);
            let level = if before.contains("host") {
                Level::Host
            } else {
                Level::Global
            };
            let e = save(dir.path(), "t", level, &edits).unwrap_err();
            assert!(format!("{e:#}").contains(why), "{before}: {e:#}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
    }

    /// Review focus 4, the save half: a `config.toml` that is a directory
    /// fails the save with a reason.
    #[test]
    fn a_config_file_that_is_a_directory_fails_the_save() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(FILE)).unwrap();
        let e = save(
            dir.path(),
            "t",
            Level::Global,
            &pending(&[set("popup.linger", "5s")]),
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("reading"), "{e:#}");
    }
}

//! The activity log: what oxutrm did to keep the session alive.
//!
//! Kept twice: a ring of the last [`RING`] entries for the status popup, and
//! a size-capped file for afterwards. The network crates never log -- they
//! return reasons -- and the session records them here. [`Activity::record`]
//! is the only way in.
//!
//! Times in the file are UTC. The root crate forbids `unsafe`, std has no
//! time zone, and UTC is unambiguous on any machine that reads the file.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many entries the popup can scroll back through.
pub(crate) const RING: usize = 200;

/// The size at which `client.log` is rotated to `client.log.1`, so the two
/// together never hold more than twice this.
pub(crate) const LOG_CAP: u64 = 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Link,
    Standby,
    Failover,
    Rebuild,
    Input,
    Log,
}

impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::Link => "link",
            Kind::Standby => "standby",
            Kind::Failover => "failover",
            Kind::Rebuild => "rebuild",
            Kind::Input => "input",
            Kind::Log => "log",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Entry {
    /// When it last happened.
    pub(crate) at: SystemTime,
    /// When its run of identical entries began; `at` until it repeats.
    pub(crate) since: SystemTime,
    pub(crate) kind: Kind,
    /// Legible already: escaped and cut to one line when it was recorded.
    pub(crate) text: String,
    /// How many times it happened again after the first.
    pub(crate) repeats: u32,
}

pub(crate) struct Activity {
    ring: VecDeque<Entry>,
    file: Option<LogFile>,
    /// `<target> <session-id prefix>`: what every file line carries after
    /// its time, so two sessions sharing the file can be told apart.
    tag: String,
}

impl Activity {
    /// A log with no file: the ring alone.
    pub(crate) fn new() -> Activity {
        Activity {
            ring: VecDeque::new(),
            file: None,
            tag: "- -".to_string(),
        }
    }

    /// A log that also appends to `file`. A file that could not be opened
    /// costs one `log` entry and nothing else.
    pub(crate) fn with_file(
        file: std::io::Result<LogFile>,
        target: &str,
        session_id: &str,
    ) -> Activity {
        let prefix: String = session_id.chars().take(8).collect();
        let prefix = if prefix.is_empty() {
            "-".to_string()
        } else {
            oxutrm_client::legible(&prefix)
        };
        let mut a = Activity {
            ring: VecDeque::new(),
            file: None,
            tag: format!("{} {prefix}", oxutrm_client::legible(target)),
        };
        match file {
            Ok(f) => a.file = Some(f),
            Err(e) => a.file_off(&e),
        }
        a
    }

    pub(crate) fn record(&mut self, kind: Kind, text: &str) {
        self.record_at(kind, text, SystemTime::now());
    }

    /// [`Activity::record`], at a given time.
    pub(crate) fn record_at(&mut self, kind: Kind, text: &str, at: SystemTime) {
        let text = oxutrm_client::summarised(text);
        if let Some(last) = self.ring.back_mut()
            && last.kind == kind
            && last.text == text
        {
            last.repeats = last.repeats.saturating_add(1);
            last.at = at;
            return;
        }
        self.end_run();
        let line = self.line(at, kind, &text, None);
        self.push(Entry {
            at,
            since: at,
            kind,
            text,
            repeats: 0,
        });
        self.write(&line);
    }

    /// Oldest first.
    pub(crate) fn entries(&self) -> std::collections::vec_deque::Iter<'_, Entry> {
        self.ring.iter()
    }

    fn push(&mut self, e: Entry) {
        self.ring.push_back(e);
        while self.ring.len() > RING {
            self.ring.pop_front();
        }
    }

    /// The newest entry's repeats, written as one line now that its run is
    /// over. Its first occurrence is on disk already.
    fn end_run(&mut self) {
        let Some(last) = self.ring.back() else {
            return;
        };
        if last.repeats == 0 {
            return;
        }
        let line = self.line(
            last.at,
            last.kind,
            &last.text,
            Some((last.repeats, last.since)),
        );
        self.write(&line);
    }

    fn line(
        &self,
        at: SystemTime,
        kind: Kind,
        text: &str,
        run: Option<(u32, SystemTime)>,
    ) -> String {
        let mut l = format!("{} {} {} {text}", rfc3339_utc(at), self.tag, kind.name());
        if let Some((n, since)) = run {
            l.push_str(&format!(" (repeated {n}\u{d7} since {})", hh_mm_utc(since)));
        }
        l
    }

    fn write(&mut self, line: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(e) = file.write_line(line) {
            self.file = None;
            self.file_off(&e);
        }
    }

    /// The file is gone for the rest of the session: said once, in the ring.
    fn file_off(&mut self, e: &std::io::Error) {
        let at = SystemTime::now();
        self.push(Entry {
            at,
            since: at,
            kind: Kind::Log,
            text: oxutrm_client::summarised(&format!("log file off: {e}")),
            repeats: 0,
        });
    }
}

impl Drop for Activity {
    /// A run still folding when the session ends is written out, so the
    /// file says how the session ended.
    fn drop(&mut self) {
        self.end_run();
    }
}

/// `client.log`, opened for appending, rotated at a cap.
pub(crate) struct LogFile {
    path: PathBuf,
    file: File,
    cap: u64,
}

impl LogFile {
    pub(crate) fn open(path: PathBuf, cap: u64) -> std::io::Result<LogFile> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = append(&path)?;
        Ok(LogFile { path, file, cap })
    }

    /// `$XDG_STATE_HOME/oxutrm/client.log`, else
    /// `~/.local/state/oxutrm/client.log`, capped at [`LOG_CAP`].
    ///
    /// No test calls it: it would write into the home of whoever runs the
    /// tests. It is [`state_path`] and [`LogFile::open`], which are.
    pub(crate) fn open_default() -> std::io::Result<LogFile> {
        let path = state_path(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "neither XDG_STATE_HOME nor HOME names a directory",
                )
            })?;
        LogFile::open(path, LOG_CAP)
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        // Another client sharing the file may have rotated it, leaving our
        // handle on client.log.1. Appending there would take it past the
        // cap, so the name is followed rather than the handle.
        let ours = self.file.metadata()?;
        let named = std::fs::metadata(&self.path).ok();
        if named.is_none_or(|m| m.dev() != ours.dev() || m.ino() != ours.ino()) {
            self.file = append(&self.path)?;
        }
        let bytes = format!("{line}\n");
        let len = self.file.metadata()?.len();
        if len > 0 && len + bytes.len() as u64 > self.cap {
            self.rotate()?;
        }
        // One write of the whole line: with O_APPEND, two clients appending
        // at once cannot interleave inside it.
        self.file.write_all(bytes.as_bytes())
    }
}

impl LogFile {
    /// Move client.log to client.log.1 and carry on in a fresh client.log.
    ///
    /// No client.log to move means another client sharing the file passed
    /// the cap at the same moment and renamed it first: a lost race, and the
    /// rotation it wanted has happened. The log carries on in whatever
    /// client.log is there now, or a new one.
    fn rotate(&mut self) -> std::io::Result<()> {
        match std::fs::rename(&self.path, rotated(&self.path)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            other => other?,
        }
        self.file = append(&self.path)?;
        Ok(())
    }
}

fn append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn rotated(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// Where the log lives, from the two variables that decide it. A relative
/// or empty value is ignored, as the XDG base-directory spec requires.
pub(crate) fn state_path(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let base = match absolute(xdg) {
        Some(p) => p,
        None => absolute(home)?.join(".local/state"),
    };
    Some(base.join("oxutrm").join("client.log"))
}

/// `t` as RFC 3339 in UTC, to the second.
pub(crate) fn rfc3339_utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

fn hh_mm_utc(t: SystemTime) -> String {
    let s = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) % 86_400;
    format!("{:02}:{:02}", s / 3600, s / 60 % 60)
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 2026-09-21T14:13:20Z.
    const T: u64 = 1_790_000_000;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn texts(a: &Activity) -> Vec<String> {
        a.entries().map(|e| e.text.clone()).collect()
    }

    #[test]
    fn record_stamps_the_entry_with_the_wall_clock() {
        let before = SystemTime::now();
        let mut a = Activity::new();
        a.record(Kind::Link, "silent");
        let e = a.entries().next().unwrap();
        assert!(e.at >= before && e.at <= SystemTime::now(), "{:?}", e.at);
    }

    #[test]
    fn every_kind_has_the_name_the_log_shows() {
        let names: Vec<&str> = [
            Kind::Link,
            Kind::Standby,
            Kind::Failover,
            Kind::Rebuild,
            Kind::Input,
            Kind::Log,
        ]
        .into_iter()
        .map(Kind::name)
        .collect();
        assert_eq!(
            names,
            ["link", "standby", "failover", "rebuild", "input", "log"]
        );
    }

    #[test]
    fn utc_timestamps_are_rfc_3339() {
        assert_eq!(rfc3339_utc(at(0)), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(at(951_782_400)), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(at(1_000_000_000)), "2001-09-09T01:46:40Z");
        assert_eq!(rfc3339_utc(at(4_102_444_799)), "2099-12-31T23:59:59Z");
        assert_eq!(rfc3339_utc(at(T)), "2026-09-21T14:13:20Z");
    }

    /// The same thing happening again is one entry with a count, not a
    /// screenful of copies -- and its time is the latest one.
    #[test]
    fn a_repeat_folds_into_the_entry_before_it() {
        let mut a = Activity::new();
        a.record_at(Kind::Standby, "not found: no path", at(T));
        a.record_at(Kind::Standby, "not found: no path", at(T + 30));
        a.record_at(Kind::Standby, "not found: no path", at(T + 90));

        assert_eq!(a.entries().len(), 1);
        let e = a.entries().next().unwrap();
        assert_eq!(e.repeats, 2);
        assert_eq!(e.at, at(T + 90), "the time was not updated");
        assert_eq!(e.since, at(T), "the run forgot when it began");
    }

    /// Same text, different kind: two things, not one.
    #[test]
    fn only_the_same_kind_and_text_fold() {
        let mut a = Activity::new();
        a.record_at(Kind::Link, "x", at(T));
        a.record_at(Kind::Standby, "x", at(T));
        a.record_at(Kind::Standby, "y", at(T));
        assert_eq!(texts(&a), ["x", "x", "y"]);
    }

    #[test]
    fn the_ring_keeps_the_newest_two_hundred() {
        let mut a = Activity::new();
        for i in 0..250 {
            a.record_at(Kind::Link, &format!("e-{i:03}"), at(T + i));
        }
        assert_eq!(a.entries().len(), RING);
        assert_eq!(a.entries().next().unwrap().text, "e-050");
        assert_eq!(a.entries().next_back().unwrap().text, "e-249");
    }

    /// A reason is a remote's stderr. Escaped and cut to its first line when
    /// recorded, so neither the popup nor the file can be handed a control
    /// sequence.
    #[test]
    fn a_recorded_text_is_escaped_and_cut_to_one_line() {
        let mut a = Activity::new();
        a.record_at(
            Kind::Standby,
            "not found: \u{1b}[2Jgone\u{9b}1m\nsecond line",
            at(T),
        );
        let text = &a.entries().next().unwrap().text;
        assert!(!text.chars().any(char::is_control), "{text:?}");
        assert!(
            text.contains("^[[2Jgone") && text.contains("<9B>"),
            "{text:?}"
        );
        assert!(!text.contains("second line"), "{text:?}");
    }

    #[test]
    fn the_state_path_prefers_xdg_and_falls_back_to_home() {
        let p = |x: Option<&str>, h: Option<&str>| state_path(x.map(Into::into), h.map(Into::into));
        assert_eq!(
            p(Some("/x/state"), Some("/home/u")),
            Some(PathBuf::from("/x/state/oxutrm/client.log"))
        );
        assert_eq!(
            p(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/oxutrm/client.log"))
        );
        // The XDG spec: a relative path in an XDG variable is invalid and
        // must be ignored, not resolved against wherever oxutrm was started.
        assert_eq!(
            p(Some("rel/state"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/oxutrm/client.log"))
        );
        assert_eq!(p(Some(""), Some("")), None);
        assert_eq!(p(None, None), None);
    }

    fn opened(dir: &Path, cap: u64) -> (Activity, PathBuf) {
        let path = dir.join("state/oxutrm/client.log");
        let a = Activity::with_file(LogFile::open(path.clone(), cap), "bastion", "f00dcafe0123");
        (a, path)
    }

    #[test]
    fn every_new_entry_is_appended_as_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, path) = opened(dir.path(), LOG_CAP);
        a.record_at(Kind::Link, "silent", at(T));
        a.record_at(Kind::Standby, "search started", at(T + 5));

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "2026-09-21T14:13:20Z bastion f00dcafe link silent\n\
             2026-09-21T14:13:25Z bastion f00dcafe standby search started\n"
        );
    }

    /// The first occurrence is on disk at once; the repeats are one line
    /// when the run ends.
    #[test]
    fn a_folded_run_is_written_once_when_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, path) = opened(dir.path(), LOG_CAP);
        a.record_at(Kind::Link, "silent", at(T));
        a.record_at(Kind::Link, "silent", at(T + 60));
        a.record_at(Kind::Link, "silent", at(T + 120));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            1,
            "a repeat was written as a line of its own"
        );

        a.record_at(Kind::Standby, "search started", at(T + 130));
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "2026-09-21T14:13:20Z bastion f00dcafe link silent",
                "2026-09-21T14:15:20Z bastion f00dcafe link silent (repeated 2\u{d7} since 14:13)",
                "2026-09-21T14:15:30Z bastion f00dcafe standby search started",
            ]
        );
    }

    #[test]
    fn a_run_still_folding_when_the_session_ends_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, path) = opened(dir.path(), LOG_CAP);
        a.record_at(Kind::Link, "silent", at(T));
        a.record_at(Kind::Link, "silent", at(T + 60));
        drop(a);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("(repeated 1\u{d7} since 14:13)"), "{text}");
    }

    /// The cap holds on disk: client.log at most one cap, client.log.1 at
    /// most one more, and the newest line in client.log.
    #[test]
    fn the_file_rotates_before_it_passes_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let cap = 300;
        let (mut a, path) = opened(dir.path(), cap);
        for i in 0..40 {
            a.record_at(Kind::Link, &format!("entry number {i:02}"), at(T + i));
        }
        let rotated = dir.path().join("state/oxutrm/client.log.1");
        let live = std::fs::metadata(&path).unwrap().len();
        let old = std::fs::metadata(&rotated).expect("never rotated").len();
        assert!(
            live <= cap && old <= cap,
            "{live} + {old} against a cap of {cap}"
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with("entry number 39\n")
        );
    }

    /// Another client rotated the shared file under us. Our handle now
    /// points at client.log.1; appending there would grow it past the cap.
    #[test]
    fn a_rotation_by_another_client_is_followed_and_the_cap_holds() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, path) = opened(dir.path(), LOG_CAP);
        a.record_at(Kind::Link, "first", at(T));
        let rotated = dir.path().join("state/oxutrm/client.log.1");
        std::fs::rename(&path, &rotated).unwrap();
        let before = std::fs::metadata(&rotated).unwrap().len();

        a.record_at(Kind::Link, "second", at(T + 1));

        assert_eq!(
            std::fs::metadata(&rotated).unwrap().len(),
            before,
            "wrote into the rotated file"
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .ends_with("link second\n")
        );
    }

    /// Final review, Minor 3. Two clients pass the cap at once and the other
    /// one renames client.log first: our rename finds no client.log. That
    /// is a lost race, not a broken file -- the rotation it wanted has
    /// happened -- so the log carries on in the new client.log. Reached
    /// through `rotate` because nothing single-threaded can rename the file
    /// between `write_line`'s check and its rename.
    #[test]
    fn a_rotation_that_lost_the_race_carries_on_in_the_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/oxutrm/client.log");
        let rotated = dir.path().join("state/oxutrm/client.log.1");
        let mut log = LogFile::open(path.clone(), LOG_CAP).unwrap();
        log.write_line("ours, before").unwrap();
        std::fs::rename(&path, &rotated).unwrap();
        assert!(!path.exists(), "the race was not set up");

        log.rotate()
            .expect("a lost rotation race turned logging off");
        log.write_line("ours, after").unwrap();

        assert_eq!(
            std::fs::read_to_string(&rotated).unwrap(),
            "ours, before\n",
            "the winner's rotated file was touched"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ours, after\n");
    }

    /// The race tolerance is for the missing name only: any other rename
    /// failure is still a failure.
    #[test]
    fn a_rotation_that_fails_otherwise_is_still_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/oxutrm/client.log");
        let mut log = LogFile::open(path.clone(), LOG_CAP).unwrap();
        // A non-empty directory in the rotated name's place: rename fails,
        // and not with "not found".
        std::fs::create_dir_all(dir.path().join("state/oxutrm/client.log.1/x")).unwrap();
        let e = log.rotate().expect_err("a rename into a directory worked");
        assert_ne!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
    }

    /// A read-only home, a missing one, a path component that is a file: the
    /// session goes on, and the popup says once that there is no file.
    /// A file in the way rather than a read-only directory, so this also
    /// fails as root, where permissions do not.
    #[test]
    fn a_log_file_that_cannot_be_opened_leaves_one_entry_and_the_ring_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"").unwrap();
        let mut a = Activity::with_file(
            LogFile::open(blocker.join("oxutrm/client.log"), LOG_CAP),
            "bastion",
            "f00d",
        );
        let first: Vec<(Kind, String)> = a.entries().map(|e| (e.kind, e.text.clone())).collect();
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].0, Kind::Log);
        assert!(first[0].1.starts_with("log file off: "), "{first:?}");

        a.record_at(Kind::Link, "silent", at(T));
        assert_eq!(texts(&a).last().map(String::as_str), Some("silent"));
    }

    /// The file vanished and something else took its name mid-session.
    #[test]
    fn a_log_file_that_fails_mid_session_is_dropped_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, path) = opened(dir.path(), LOG_CAP);
        a.record_at(Kind::Link, "before", at(T));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        a.record_at(Kind::Link, "during", at(T + 1));
        a.record_at(Kind::Link, "after", at(T + 2));

        let offs = a.entries().filter(|e| e.kind == Kind::Log).count();
        assert_eq!(
            offs,
            1,
            "the failure was reported {offs} times: {:?}",
            texts(&a)
        );
        assert!(
            texts(&a).contains(&"during".to_string()),
            "the entry that hit the failure was lost"
        );
        assert_eq!(texts(&a).last().map(String::as_str), Some("after"));
    }
}

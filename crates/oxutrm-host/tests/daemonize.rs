//! The descriptor that must not survive.
//!
//! If one file descriptor inherited from ssh outlives `daemonize`, closing the
//! laptop lid kills the session — precisely the failure oxutrm exists to
//! prevent, and one that no ordinary test would notice. So this file does not
//! inspect the code: it spawns a real process, lets it detach, and then reads
//! `/proc/self/fd` from inside the detached grandchild.
//!
//! Two claims are worth stating plainly, because each is easy to fake:
//!
//! * **Nothing points at the pipes we handed it.** Enumerated, not sampled.
//! * **It outlives its parent.** The probe sleeps before reporting, so the
//!   report existing at all proves the parent was already reaped.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Wait for the probe to write its report, up to `limit`.
///
/// The probe writes the whole report in one call ending in a newline, so a
/// partial read cannot be mistaken for a finished one.
fn wait_for(path: &Path, limit: Duration) -> String {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return text;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the daemonized process never wrote {}", path.display());
}

fn fd_targets(report: &str) -> Vec<(i32, String)> {
    report
        .lines()
        .filter_map(|l| l.strip_prefix("fd="))
        .filter_map(|l| l.split_once(" -> "))
        .map(|(n, t)| (n.parse::<i32>().expect("fd number"), t.to_string()))
        .collect()
}

fn field<'a>(report: &'a str, key: &str) -> &'a str {
    report
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key} in report:\n{report}"))
}

/// Spawn the probe with piped stdio, wait for its first process to exit, and
/// return that pid together with the still-open child handle.
fn spawn_probe(report_path: &Path) -> (u32, std::process::Child) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxutrm-daemon-probe"))
        .arg(report_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the probe");
    let pid = child.id();
    let status = child.wait().expect("wait for the probe's first process");
    assert!(
        status.success(),
        "the fork parent must exit 0, got {status:?}"
    );
    (pid, child)
}

#[test]
fn the_daemon_outlives_its_parent_and_keeps_no_inherited_descriptor() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let report_path = tmp.path().join("report.txt");

    let (child_pid, mut child) = spawn_probe(&report_path);

    // The probe sleeps before reporting, so the report cannot exist yet. This
    // is what makes the next wait mean "outlived its parent" rather than
    // "finished before its parent did".
    assert!(
        !report_path.exists(),
        "the probe reported before its parent died; the test proves nothing"
    );

    // daemonize must have closed the pipes we handed it, so reading them
    // returns EOF rather than blocking until the daemon eventually exits.
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut out)
        .expect("read stdout");
    assert!(
        out.is_empty(),
        "the daemon wrote to the inherited stdout: {out:?}"
    );

    let report = wait_for(&report_path, Duration::from_secs(10));

    let ppid: u32 = field(&report, "ppid").parse().expect("ppid");
    assert_ne!(
        ppid, child_pid,
        "still parented to the process ssh waited on"
    );
    assert_ne!(
        ppid,
        std::process::id(),
        "still parented to the test harness"
    );

    // The probe held a varied set open before detaching. Anything less than
    // all of them surviving-nothing would let a partial close pass.
    let held: Vec<&str> = report
        .lines()
        .filter_map(|l| l.strip_prefix("held="))
        .collect();
    assert!(
        held.len() >= 6,
        "the probe held too few descriptors to prove anything: {held:?}"
    );
    assert!(
        held.iter().any(|h| h.starts_with("high-900")),
        "without a high-numbered descriptor, a bounded close loop would pass: {held:?}"
    );

    let targets = fd_targets(&report);
    assert!(
        !targets.is_empty(),
        "the probe reported no descriptors at all, so it checked nothing"
    );

    // The enumeration opens a descriptor of its own to read /proc/self/fd.
    // Everything else that survived is a genuine leak.
    let survivors: Vec<&(i32, String)> = targets
        .iter()
        .filter(|(_, target)| !target.starts_with("/proc/"))
        .collect();

    // THE assertion. Not "no pipe survived" -- exactly three descriptors
    // survived, and each is /dev/null. A close that missed one file, one
    // socket or one high number fails here; a check for particular targets
    // would not.
    assert_eq!(
        survivors.len(),
        3,
        "exactly stdin, stdout and stderr may survive; these did:\n{survivors:#?}\n\nfull report:\n{report}"
    );

    for std_fd in [0, 1, 2] {
        let (_, target) = survivors
            .iter()
            .find(|(n, _)| *n == std_fd)
            .unwrap_or_else(|| panic!("fd {std_fd} is missing entirely:\n{report}"));
        assert_eq!(
            target, "/dev/null",
            "fd {std_fd} must be reopened on /dev/null"
        );
    }

    // Kept as named checks too, because the message they give when they fail
    // says what went wrong rather than only that a count was off.
    for (fd, target) in &targets {
        assert!(
            !target.starts_with("pipe:"),
            "fd {fd} still points at an inherited pipe ({target}); \
             closing the laptop lid would kill this session"
        );
        assert!(
            !target.contains(".marker"),
            "fd {fd} still points at something held before daemonizing ({target})"
        );
        assert!(
            !target.contains(".sock"),
            "fd {fd} still points at a socket bound before daemonizing ({target})"
        );
    }
}

#[test]
fn the_daemon_leaves_the_terminals_session_and_cannot_reacquire_one() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let report_path = tmp.path().join("report2.txt");

    // Our own session: the one ssh would own in the real thing.
    let ours = unsafe { libc::getsid(0) };
    assert!(ours > 0, "getsid failed for the test harness itself");

    let (_pid, _child) = spawn_probe(&report_path);
    let report = wait_for(&report_path, Duration::from_secs(10));

    let pid: i32 = field(&report, "pid").parse().expect("pid");
    let sid: i32 = field(&report, "sid").parse().expect("sid");

    assert_ne!(
        sid, ours,
        "setsid did not run: the daemon is still in the session that spawned \
         it, so a hangup on that terminal still reaches it"
    );
    // This is what the SECOND fork buys, and it is the assertion people get
    // backwards: a session LEADER acquires a controlling terminal the moment
    // it opens one. The final grandchild is a session member, not its leader,
    // so it cannot. `sid` here names the middle process, which is already gone.
    assert_ne!(
        sid, pid,
        "the daemon is a session leader, so opening any terminal would hand \
         it a controlling terminal again; the second fork is missing"
    );

    assert_eq!(field(&report, "cwd"), "/", "the daemon must chdir to /");
}

//! Test fixture. Holds an open file, daemonizes, then reports what survived.
//!
//! Not part of the product. It exists so `tests/daemonize.rs` can assert
//! against a real detached process rather than a mock — the failure it guards
//! against (an inherited descriptor outliving the detach) is invisible to any
//! test that does not go and look at `/proc/self/fd` from the far side.

use std::io::Write;
use std::os::fd::AsRawFd;

fn main() {
    let report = std::env::args()
        .nth(1)
        .expect("usage: oxutrm-daemon-probe <report-path>");
    let marker = format!("{report}.marker");

    // Stand in for the descriptors a real `oxutrm host --serve` inherits from
    // sshd, and deliberately VARIED. A single held descriptor would let a
    // partial close pass: an implementation that shut fd 3 and stopped, or one
    // that looped over a guessed range, would look identical to a correct one.
    //
    // Everything here is leaked on purpose, so nothing but `daemonize` can
    // close it.
    let mut held: Vec<(String, i32)> = Vec::new();

    // Several regular files, so "closed the first one" is not enough.
    for i in 0..4 {
        let f = std::fs::File::create(format!("{marker}.{i}")).expect("create a marker file");
        held.push((format!("file{i}"), f.as_raw_fd()));
        std::mem::forget(f);
    }

    // A pipe pair: what sshd actually hands a remote command.
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == 0 {
        held.push(("pipe-read".to_string(), fds[0]));
        held.push(("pipe-write".to_string(), fds[1]));
    }

    // A listening Unix socket, which is the shape of a session socket bound
    // too early -- one of the four ordering rules daemonize documents.
    let sock_path = format!("{marker}.sock");
    let _ = std::fs::remove_file(&sock_path);
    if let Ok(listener) = std::os::unix::net::UnixListener::bind(&sock_path) {
        held.push(("unix-listener".to_string(), listener.as_raw_fd()));
        std::mem::forget(listener);
    }

    // A HIGH-numbered descriptor. This is the one that catches a bounded loop
    // such as `for fd in 3..256`: enumeration finds it, a guessed range does
    // not.
    let high = unsafe { libc::dup2(held[0].1, 900) };
    if high >= 0 {
        held.push(("high-900".to_string(), high));
    }

    oxutrm_host::daemonize().expect("daemonize");

    // Outlive the parent, so writing the report at all proves independence
    // rather than a race won.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mut lines = Vec::new();
    lines.push(format!("pid={}", std::process::id()));
    lines.push(format!("ppid={}", unsafe { libc::getppid() }));
    lines.push(format!("sid={}", unsafe { libc::getsid(0) }));
    for (name, fd) in &held {
        lines.push(format!("held={name}:{fd}"));
    }
    lines.push(format!(
        "cwd={}",
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".to_string())
    ));
    for entry in std::fs::read_dir("/proc/self/fd").expect("read /proc/self/fd") {
        let entry = entry.expect("fd entry");
        let n = entry.file_name().to_string_lossy().to_string();
        let target = std::fs::read_link(entry.path())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".to_string());
        lines.push(format!("fd={n} -> {target}"));
    }

    // Written last and in one call, so the test never reads half a report. The
    // descriptor for this file is opened after the enumeration above, so it
    // does not appear in the listing and cannot be mistaken for a survivor.
    let mut f = std::fs::File::create(&report).expect("create the report");
    writeln!(f, "{}", lines.join("\n")).expect("write the report");
}

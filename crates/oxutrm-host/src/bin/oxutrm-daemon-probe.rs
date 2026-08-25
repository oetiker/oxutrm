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

    // Stand in for the descriptors ssh leaves behind: opened before
    // daemonizing and deliberately leaked, so nothing but `daemonize` can
    // close it. If it survives, the test sees it by name.
    let held = std::fs::File::create(&marker).expect("create the marker file");
    let held_fd = held.as_raw_fd();
    std::mem::forget(held);

    oxutrm_host::daemonize().expect("daemonize");

    // Outlive the parent, so writing the report at all proves independence
    // rather than a race won.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mut lines = Vec::new();
    lines.push(format!("pid={}", std::process::id()));
    lines.push(format!("ppid={}", unsafe { libc::getppid() }));
    lines.push(format!("sid={}", unsafe { libc::getsid(0) }));
    lines.push(format!("held_fd={held_fd}"));
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

//! A child that prints combining marks without end must not grow the host.
//!
//! In a file of its own because it measures the resident size of the test
//! process, and the unit tests running next to it would add their noise.
//! Run under a hard memory cap: on a regression this test is meant to fail,
//! not to take the machine's memory with it.

use std::time::{Duration, Instant};

use oxutrm_proto::TermSize;
use oxutrm_term::HostTerm;

/// Bytes of U+0301 the child writes, all onto one cell.
const FED: usize = 24 * 1024 * 1024;

/// Resident set size of this process, in bytes.
fn rss() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
    let kib: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|v| v.parse().ok())
        .expect("VmRSS");
    kib * 1024
}

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "reads /proc")]
fn endless_combining_marks_do_not_grow_the_host() {
    // `yes` repeats U+0301 (CC 81) a line at a time; the newlines go, and
    // an even byte count keeps the last mark whole.
    let script = format!(
        "printf e; yes \"$(printf '\\314\\201')\" | tr -d '\\n' | head -c {FED}; \
         printf ' after'; sleep 30"
    );
    let mut t = HostTerm::spawn(
        "/bin/sh",
        &["-c".to_owned(), script],
        &[],
        TermSize { cols: 20, rows: 3 },
        10,
    )
    .expect("spawn");

    let before = rss();
    let mut peak = before;
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut seen = false;
    while Instant::now() < deadline {
        t.poll().expect("poll");
        peak = peak.max(rss());
        let s = t.snapshot(1);
        let row: String = s.row(0).iter().map(|c| c.text.as_str()).collect();
        if row.contains("after") {
            seen = true;
            break;
        }
        if !t.more_output_waiting() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert!(seen, "the text after the marks never rendered");
    let grew = peak.saturating_sub(before);
    assert!(
        grew < 16 * 1024 * 1024,
        "fed {} MiB of combining marks, the host grew by {} MiB",
        FED >> 20,
        grew >> 20
    );
}

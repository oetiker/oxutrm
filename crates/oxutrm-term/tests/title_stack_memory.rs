//! Pushing one huge title many times must not grow the host.
//!
//! `CSI 22 t` pushes a copy of the current title onto a stack the emulator
//! keeps 4096 deep, so five bytes of output can pin a whole title again.
//! In a file of its own because it measures the resident size of the test
//! process. Run under a hard memory cap: on a regression this test is meant
//! to fail, not to take the machine's memory with it.

use std::time::{Duration, Instant};

use oxutrm_proto::TermSize;
use oxutrm_term::HostTerm;

/// The title, in bytes: under the general OSC cap.
const TITLE: usize = 400 * 1024;
/// How many copies are pushed. 512 x 400 KiB is 200 MiB when nothing stops it.
const PUSHES: usize = 512;

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
fn pushing_a_huge_title_does_not_grow_the_host() {
    let script = format!(
        "printf '\\033]2;'; head -c {TITLE} /dev/zero | tr '\\000' A; printf '\\007'; \
         i=0; while [ $i -lt {PUSHES} ]; do printf '\\033[22t'; i=$((i+1)); done; \
         printf after; sleep 30"
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
        if row.starts_with("after") {
            seen = true;
            break;
        }
        if !t.more_output_waiting() {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    assert!(seen, "the text after the pushes never rendered");
    let grew = peak.saturating_sub(before);
    assert!(
        grew < 32 * 1024 * 1024,
        "pushed a {} KiB title {PUSHES} times, the host grew by {} MiB",
        TITLE >> 10,
        grew >> 20
    );
}

//! Test fixture. Holds an open file, detaches, then reports what survived.
//!
//! Not part of the product. It exists so `tests/daemonize.rs` can assert
//! against a real detached process rather than a mock — the failure it guards
//! against (an inherited descriptor outliving the detach) is invisible to any
//! test that does not go and look at `/proc/self/fd` from the far side.
//!
//! # Two modes
//!
//! * `oxutrm-daemon-probe <report>` — call `daemonize` and report the end
//!   state. Nothing may have survived.
//! * `oxutrm-daemon-probe <report> --split` — call `detach_process`, write a
//!   marker on two descriptors inherited from the test, *then* call
//!   `sever_from_ssh`, then try the same two writes again. The end state must
//!   be identical; what differs is that the state in between is now observable
//!   from outside, and the whole wiring of `oxutrm host --serve` depends on it:
//!   the grandchild must still be able to speak to sshd until it severs.

use std::io::Write;
use std::os::fd::AsRawFd;

/// Written after the double fork, before the sever. Must arrive.
const PHASE1_STDOUT: &str = "phase1-stdout-marker";
const PHASE1_FILE: &str = "phase1-file-marker";
/// Written after the sever, on the same two descriptors. Must not arrive.
const PHASE2_STDOUT: &str = "phase2-stdout-marker";
const PHASE2_FILE: &str = "phase2-file-marker";

fn main() {
    let mut args = std::env::args().skip(1);
    let report = args
        .next()
        .expect("usage: oxutrm-daemon-probe <report-path> [--split]");
    // Collected rather than read in sequence, so the flags below are
    // order-independent and adding one cannot silently shift another.
    let flags: Vec<String> = args.collect();
    let split = flags.iter().any(|f| f == "--split");
    // With `--gate`, the probe waits for `<report>.go` to appear before it
    // severs, so a test can hold the window between the two phases open for
    // as long as it needs rather than racing a sleep.
    let gate = flags.iter().any(|f| f == "--gate");
    // With `--keep-open`, the probe opens a descriptor AFTER detaching, which
    // must survive the sever. Behind a flag because the strict "nothing but
    // /dev/null survived" assertion is about descriptors inherited from ssh,
    // and this one deliberately is not.
    let keep_open = flags.iter().any(|f| f == "--keep-open");
    let marker = format!("{report}.marker");

    let held = hold_descriptors(&marker);

    if split {
        run_split(&report, &held, gate, keep_open);
    } else {
        oxutrm_host::daemonize().expect("daemonize");

        // Outlive the parent, so writing the report at all proves independence
        // rather than a race won.
        std::thread::sleep(std::time::Duration::from_millis(300));

        write_report(&report, &held, &[]);
    }
}

/// Stand in for the descriptors a real `oxutrm host --serve` inherits from
/// sshd, and deliberately VARIED. A single held descriptor would let a partial
/// close pass: an implementation that shut fd 3 and stopped, or one that looped
/// over a guessed range, would look identical to a correct one.
///
/// Everything here is leaked on purpose, so nothing but the sever can close it.
fn hold_descriptors(marker: &str) -> Vec<(String, i32)> {
    let mut held: Vec<(String, i32)> = Vec::new();

    // Several regular files, so "closed the first one" is not enough. `file0`
    // is also the one the split mode writes its markers on, because its
    // contents are readable from outside the process afterwards — a pipe we
    // both ends of would not be.
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
    // too early -- one of the ordering rules the daemon module documents.
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

    held
}

/// The two phases with the state between them made visible.
///
/// The order here mirrors `oxutrm host --serve` exactly: fork first, talk to
/// ssh, settle the rung, sever. Only the "talk to ssh" part is a marker line
/// instead of a handshake and an ICE ladder.
fn run_split(report: &str, held: &[(String, i32)], gate: bool, keep_open: bool) {
    let file0 = fd_named(held, "file0");
    let high = fd_named(held, "high-900");

    // Phase 1. Nothing before this line has created a thread, which is the
    // whole reason `oxutrm host --serve` can put it first.
    let detached = oxutrm_host::detach_process().expect("detach_process");

    // The claim the design rests on: descriptors inherited from ssh are still
    // open here, in the grandchild, after the double fork. If either of these
    // fails, `HostHello` could not be sent and no candidate could cross, so
    // rungs 1-3 would be unreachable and only rung 4 would ever nominate.
    let phase1_file = write_line(file0, PHASE1_FILE);
    let phase1_stdout = write_line(libc::STDOUT_FILENO, PHASE1_STDOUT);

    // A descriptor opened AFTER the detach, standing in for the one the real
    // session cannot do without: the UDP socket bound at R5, punched by the
    // ladder, and adopted by QUIC. It cannot be opened after the sever -- the
    // NAT mapping belongs to that exact socket -- so if the sever closes it
    // the session dies the moment it detaches.
    let after_detach_fd = if keep_open {
        let f = std::fs::File::create(format!("{report}.after-detach"))
            .expect("a file opened after the detach");
        let fd = f.as_raw_fd();
        std::mem::forget(f);
        Some(fd)
    } else {
        None
    };

    // Stand in for the handshake and the ladder: the stretch during which the
    // session is detached but has NOT yet severed, and still needs to talk to
    // sshd. Gated on a file when the caller asks, so a test can observe the
    // whole window instead of racing a sleep.
    if gate {
        let go = format!("{report}.go");
        while !std::path::Path::new(&go).exists() {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    } else {
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    // Phase 2, gated exactly as a real session gates it: a permit that only a
    // nominated non-tunnel rung can produce. A rung-4 session gets `None` here
    // and stops, keeping every descriptor above.
    let mut meta = oxutrm_host::SessionMeta {
        session_id: "probe".to_string(),
        attach_id: 1,
        pid: std::process::id(),
        created_unix: oxutrm_host::now_unix(),
        shell: "/bin/sh".to_string(),
        size: oxutrm_proto::TermSize { cols: 80, rows: 24 },
        detachable: false,
    };
    let permit = oxutrm_host::settle_detachability(&mut meta, oxutrm_proto::Rung::StunPunch)
        .expect("a nominated non-tunnel rung must yield a permit");
    oxutrm_host::sever_from_ssh(detached, permit).expect("sever_from_ssh");

    // And the other half of the claim: the same writes, now that the sever has
    // run. The two on leaked descriptors must fail EBADF. The one on stdout
    // "succeeds" -- into /dev/null -- so it is the test's read of the pipe, not
    // this return value, that proves stdout is gone.
    //
    // Deliberately before anything opens a descriptor: once something does, a
    // closed low number would be reused and this would test the wrong file.
    let after_sever_file = write_line(file0, PHASE2_FILE);
    let after_sever_high = write_line(high, PHASE2_FILE);
    let after_sever_stdout = write_line(libc::STDOUT_FILENO, PHASE2_STDOUT);
    let opened_after_detach = match after_detach_fd {
        Some(fd) => write_line(fd, "still-open"),
        None => "not-asked-for".to_string(),
    };

    write_report(
        report,
        held,
        &[
            "mode=split".to_string(),
            format!("phase1_file={phase1_file}"),
            format!("phase1_stdout={phase1_stdout}"),
            format!("after_sever_file={after_sever_file}"),
            format!("after_sever_high={after_sever_high}"),
            format!("after_sever_stdout={after_sever_stdout}"),
            format!("opened_after_detach={opened_after_detach}"),
        ],
    );
}

fn fd_named(held: &[(String, i32)], name: &str) -> i32 {
    held.iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("the probe never held a descriptor called {name}"))
        .1
}

/// Write one line on a raw descriptor. Returns `ok`, or the name of the errno,
/// so the report says *why* a write failed rather than only that it did.
///
/// Raw `write` rather than `println!`: a buffered writer would leave "did the
/// bytes reach the pipe" a question about flushing rather than about the
/// descriptor, which is the thing under test.
fn write_line(fd: i32, line: &str) -> String {
    let text = format!("{line}\n");
    // SAFETY: `fd` is a descriptor number this process opened, inherited or
    // leaked; `text` is valid for `text.len()` bytes for the call's duration.
    // A closed `fd` is exactly the case under test and is reported, not UB.
    let n = unsafe { libc::write(fd, text.as_ptr().cast(), text.len()) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::EBADF) => "EBADF".to_string(),
            Some(code) => format!("errno-{code}"),
            None => "unknown".to_string(),
        };
    }
    if n as usize == text.len() {
        "ok".to_string()
    } else {
        format!("short-{n}")
    }
}

/// Enumerate `/proc/self/fd` and write the report.
///
/// Written last and in one call, so the test never reads half a report. The
/// descriptor for the report file is opened after the enumeration, so it does
/// not appear in the listing and cannot be mistaken for a survivor.
fn write_report(report: &str, held: &[(String, i32)], extra: &[String]) {
    let mut lines = Vec::new();
    lines.push(format!("pid={}", std::process::id()));
    lines.push(format!("ppid={}", unsafe { libc::getppid() }));
    lines.push(format!("sid={}", unsafe { libc::getsid(0) }));
    for (name, fd) in held {
        lines.push(format!("held={name}:{fd}"));
    }
    lines.extend_from_slice(extra);
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

    let mut f = std::fs::File::create(report).expect("create the report");
    writeln!(f, "{}", lines.join("\n")).expect("write the report");
}

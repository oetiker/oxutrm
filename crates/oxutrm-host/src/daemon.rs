//! Detaching a session from the ssh connection that created it (spec §4.3).
//!
//! This is the highest-risk function in the project. If a single descriptor
//! inherited from ssh survives, closing the laptop lid kills the session —
//! which is the exact failure oxutrm exists to prevent, and it will not show up
//! in any test that does not go looking. `tests/daemonize.rs` goes looking.

// `fork` has no safe wrapper and cannot have one: whether the child is sound
// depends on what the whole process was doing beforehand, which no signature
// can express. The rules that make it sound here are stated on `daemonize`.
#![allow(unsafe_code)]

use anyhow::{Context, anyhow};

/// Double fork, `setsid`, `chdir("/")`, close every inherited descriptor and
/// reopen 0/1/2 on `/dev/null`.
///
/// Returns `Ok(())` only in the final grandchild. The two intermediate
/// processes call `_exit(0)` and never return.
///
/// # The four rules, each of which is a real bug if broken
///
/// 1. **The intermediates exit with `_exit`, never `std::process::exit` and
///    never by returning.** `_exit` runs no destructor and no `atexit` handler,
///    so a [`RegistryGuard`](crate::RegistryGuard) the caller already holds does
///    not delete the session directory when the fork parent goes away.
/// 2. **Call it before any thread exists**, a tokio runtime included. `fork`
///    copies only the calling thread, so a runtime built beforehand wakes up in
///    the child with its worker threads gone and deadlocks.
/// 3. **Call it after `HostHello` is flushed**, because it closes the pipes
///    that message travels on.
/// 4. **Call it after detachability is settled**, and only when
///    [`SessionMeta::set_detachable`](crate::SessionMeta::set_detachable)
///    returned true. A rung-4 session's QUIC traffic runs inside the ssh
///    connection, so closing those descriptors destroys the link it depends on.
///
/// And one that follows from the third: bind the session's Unix socket
/// **after** this, or it is closed a moment later along with everything else.
///
/// # Why two forks
///
/// The first lets the process ssh is waiting on exit immediately, so ssh sees a
/// clean exit and the user's prompt returns. `setsid` then leaves the
/// terminal's session, so a hangup can no longer reach us. The second fork is
/// the one people omit: `setsid` made us a *session leader*, and a session
/// leader acquires a controlling terminal the moment it opens one. Forking
/// again leaves a process that is a session member and can never acquire one.
pub fn daemonize() -> anyhow::Result<()> {
    // SAFETY: `fork` is called before this process has created any thread —
    // rule 2 above — so there is no other thread whose lock the child could
    // inherit held. The child does nothing between here and `_exit`/the
    // syscalls below that allocates or takes a lock.
    match unsafe { libc::fork() } {
        -1 => return Err(std::io::Error::last_os_error()).context("first fork"),
        0 => {}
        // SAFETY: `_exit` terminates immediately without running destructors or
        // atexit handlers, which is rule 1: a RegistryGuard held by the caller
        // must not delete the session directory from this dying parent.
        _ => unsafe { libc::_exit(0) },
    }

    // New session, no controlling terminal.
    // SAFETY: `setsid` only rearranges this process's own session membership.
    if unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error()).context("setsid");
    }

    // SAFETY: as above; still single-threaded, still nothing held across it.
    match unsafe { libc::fork() } {
        -1 => return Err(std::io::Error::last_os_error()).context("second fork"),
        0 => {}
        // SAFETY: see the first `_exit`.
        _ => unsafe { libc::_exit(0) },
    }

    // Do not hold a mount busy, and do not depend on a directory that may be
    // unmounted or deleted while the session runs for a week.
    std::env::set_current_dir("/").context("chdir to /")?;
    // Anything this process creates from here on is its own business.
    rustix::process::umask(rustix::fs::Mode::RWXG | rustix::fs::Mode::RWXO);

    close_inherited_descriptors()?;
    reopen_standard_descriptors()?;
    Ok(())
}

/// Close every descriptor above 2.
///
/// This is the step that decides whether closing the laptop lid kills the
/// session. It enumerates rather than guessing a range, because guessing is how
/// one descriptor survives.
fn close_inherited_descriptors() -> anyhow::Result<()> {
    // Collect first and close afterwards: closing while the directory handle is
    // open would invalidate the iterator. The handle's own descriptor is in the
    // collected list and is already closed by the time the loop reaches it,
    // which is harmless — nothing opens a new descriptor in between, so that
    // number cannot have been reused by something we must keep.
    let fds: Vec<i32> = {
        let dir = std::fs::read_dir("/proc/self/fd").context(
            "reading /proc/self/fd; oxutrm needs /proc mounted to detach safely, \
             because closing a guessed range of descriptors is how one survives",
        )?;
        dir.filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()))
            .collect()
    };
    for fd in fds {
        if fd > 2 {
            // SAFETY: `close` on a descriptor number we just read from
            // /proc/self/fd. A double close cannot happen because each number
            // appears once, and nothing in this loop opens anything.
            unsafe { libc::close(fd) };
        }
    }
    Ok(())
}

/// Point 0, 1 and 2 at `/dev/null`.
///
/// Not tidiness: leaving them closed means the next `open` in this process gets
/// descriptor 0, and then a stray `println!` writes into whatever that turned
/// out to be — a session socket, or a file being rewritten.
fn reopen_standard_descriptors() -> anyhow::Result<()> {
    // SAFETY: `open` with a NUL-terminated literal and no user input.
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
    if null < 0 {
        return Err(std::io::Error::last_os_error()).context("opening /dev/null");
    }
    for target in 0..=2 {
        // SAFETY: both arguments are valid descriptor numbers; `dup2` closes
        // `target` first if it happens to be open, which is what we want.
        if unsafe { libc::dup2(null, target) } < 0 {
            let e = std::io::Error::last_os_error();
            // SAFETY: `null` is open and owned by us at this point.
            unsafe { libc::close(null) };
            return Err(anyhow!("pointing fd {target} at /dev/null: {e}"));
        }
    }
    if null > 2 {
        // SAFETY: as above. Skipped when `null` is itself 0, 1 or 2, because
        // `dup2` has already made it one of the descriptors we are keeping.
        unsafe { libc::close(null) };
    }
    Ok(())
}

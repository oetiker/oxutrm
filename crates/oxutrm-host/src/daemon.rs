//! Detaching a session from the ssh connection that created it (spec §4.3).
//!
//! This is the highest-risk function in the project. If a single descriptor
//! inherited from ssh survives, closing the laptop lid kills the session —
//! which is the exact failure oxutrm exists to prevent, and it will not show up
//! in any test that does not go looking. `tests/daemonize.rs` goes looking.

// `fork` has no safe wrapper and cannot have one: whether the child is sound
// depends on what the whole process was doing beforehand, which no signature
// can express. The rules that make it sound here are stated on
// `detach_process`, which is where the fork now lives.
#![allow(unsafe_code)]

use anyhow::{Context, anyhow};

/// Proof that [`detach_process`] has already run **in this process**.
///
/// It exists for one reason: [`sever_from_ssh`] is only safe on the far side of
/// the double fork, and that is an ordering no comment has ever managed to
/// enforce. Severing without having forked closes ssh's pipes while still
/// sitting in ssh's session, holding ssh's controlling terminal — so ssh exits,
/// the terminal hangs up, and `SIGHUP` reaches a process that never left. The
/// session dies exactly the way detaching exists to prevent.
///
/// There is no way to construct one except by returning from `detach_process`,
/// and `detach_process` only returns in the grandchild. So a call that severs
/// before it has forked cannot be written, in the same way that
/// [`DetachPermit`](crate::DetachPermit) makes a call that severs before the
/// rung is known impossible to write.
///
/// Deliberately **not** `Clone` or `Copy`: one fork, one sever.
#[derive(Debug)]
pub struct Detached {
    _private: (),
}

/// Phase 1 of detaching: double fork, `setsid`, `umask`. **No descriptor is
/// touched.**
///
/// Returns `Ok(Detached)` only in the final grandchild. The two intermediate
/// processes call `_exit(0)` and never return.
///
/// This is designed to be the *first statement* of `oxutrm host --serve`,
/// before a socket is bound and before a runtime exists. That placement is not
/// a style preference: it is what turns rule 2 below from a comment somebody
/// has to remember into a property of the program's shape, because there is no
/// code before it that could have created a thread.
///
/// # The rules that still apply here
///
/// 1. **The intermediates exit with `_exit`, never `std::process::exit` and
///    never by returning.** `_exit` runs no destructor and no `atexit` handler,
///    so a [`RegistryGuard`](crate::RegistryGuard) the caller already holds does
///    not delete the session directory when the fork parent goes away. (Call
///    this first and there is nothing to destroy yet, which is a second reason
///    to call it first rather than a reason to stop caring.)
/// 2. **Call it before any thread exists**, a tokio runtime included. `fork`
///    copies only the calling thread, so a runtime built beforehand wakes up in
///    the child with its worker threads gone and deadlocks.
///
/// Rules 3 and 4 — after `HostHello` is flushed, after detachability is settled
/// — belong to [`sever_from_ssh`], not here. Nothing about forking is unsafe
/// for a rung-4 session, which is why this needs no
/// [`DetachPermit`](crate::DetachPermit).
///
/// # What the grandchild keeps, and why the whole design rests on it
///
/// **The grandchild inherits descriptors 0, 1 and 2 — sshd's pipes — and holds
/// them open.** `sshd` sends `exit-status` as soon as the process it spawned
/// exits, which the fork parent does immediately, but it does **not** close the
/// channel until stdout and stderr reach EOF. Because the grandchild still
/// holds them, the local `ssh` stays alive for the whole handshake and the
/// whole ICE ladder — which is what bidirectional candidate exchange requires.
/// EOF, and with it ssh's exit, arrives only when the grandchild calls
/// [`sever_from_ssh`].
///
/// This is the classic "my daemon made ssh hang" bug, used deliberately.
/// **Consequence: do not close, redirect or drop 0/1/2 between the two phases,
/// and do not "tidy up" by moving the close into this function.** Doing so
/// severs signalling the instant the fork completes, and rungs 1–3 — every rung
/// that needs candidates to cross after the fork — stop being reachable at all.
/// The symptom is not a compile error and not a panic; it is a connection that
/// only ever falls to rung 4.
///
/// # Why two forks
///
/// The first lets the process ssh is waiting on exit immediately, so ssh sees a
/// clean exit and the user's prompt returns. `setsid` then leaves the
/// terminal's session, so a hangup can no longer reach us. The second fork is
/// the one people omit: `setsid` made us a *session leader*, and a session
/// leader acquires a controlling terminal the moment it opens one. Forking
/// again leaves a process that is a session member and can never acquire one.
pub fn detach_process() -> anyhow::Result<Detached> {
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

    // Anything this process creates from here on is its own business.
    rustix::process::umask(rustix::fs::Mode::RWXG | rustix::fs::Mode::RWXO);

    Ok(Detached { _private: () })
}

/// Phase 2 of detaching: `chdir("/")`, close every inherited descriptor and
/// reopen 0/1/2 on `/dev/null`.
///
/// This is the half a rung-4 session must never run, and the
/// [`DetachPermit`](crate::DetachPermit) is what makes that structural rather
/// than remembered: it can only come from
/// [`settle_detachability`](crate::settle_detachability), which needs the
/// nominated rung, and a rung-4 session never gets one. A rung-4 session's QUIC
/// traffic runs *inside* the ssh connection, so closing those descriptors
/// destroys the link it depends on — the permit gates precisely the operation
/// its own documentation says it exists to prevent.
///
/// The [`Detached`] is the other half of the ordering: this may only run on the
/// far side of the double fork. See [`Detached`] for what goes wrong otherwise.
///
/// # The rules that belong here
///
/// * **Call it after `HostHello` — and everything else on the ssh pipes — is
///   flushed**, because this is what closes them. In practice: after
///   `Established`.
/// * **Bind the session's Unix socket after this**, or it is closed a moment
///   later along with everything else. `close_inherited_descriptors` closes by
///   enumeration and keeps no list of exceptions, which is the whole of its
///   value.
///
/// When this returns, the local `ssh` sees EOF on stdout and stderr and exits,
/// and the user's prompt comes back. That is the intended, visible effect: it
/// is the moment the session becomes detached.
pub fn sever_from_ssh(detached: Detached, permit: crate::DetachPermit) -> anyhow::Result<()> {
    // Both taken by value rather than by reference: one fork, one permit, one
    // sever. Neither binding is read, because neither has anything to read --
    // their whole content is the fact that they exist and could only have come
    // from the call that had to happen first.
    let (Detached { .. }, _) = (detached, permit);
    sever()
}

/// The body of [`sever_from_ssh`], without the two tokens.
///
/// Private, and it must stay private: every public route to it either carries
/// the tokens or is [`daemonize`], which forks immediately beforehand and is
/// documented as the whole-hog version for a caller that has no ladder to run.
fn sever() -> anyhow::Result<()> {
    // Do not hold a mount busy, and do not depend on a directory that may be
    // unmounted or deleted while the session runs for a week.
    std::env::set_current_dir("/").context("chdir to /")?;

    close_inherited_descriptors()?;
    reopen_standard_descriptors()?;
    Ok(())
}

/// Both phases back to back: double fork, `setsid`, `chdir("/")`, close every
/// inherited descriptor and reopen 0/1/2 on `/dev/null`.
///
/// Returns `Ok(())` only in the final grandchild.
///
/// This is the *old* shape, kept because it is exactly right for a caller that
/// has nothing to say over the ssh pipes after forking — and because
/// `tests/daemonize.rs` uses it to assert the end state that both phases
/// together must produce. `oxutrm host --serve` does **not** use it: it needs
/// the ladder to run in between, so it calls [`detach_process`] first and
/// [`sever_from_ssh`] once the rung is nominated.
///
/// Every rule stated on [`detach_process`] and [`sever_from_ssh`] applies here,
/// and they apply *jointly*, which is what makes this function unusable for a
/// session: it would have to fork after the ladder (impossible — the runtime's
/// threads do not survive `fork`) and sever before it (impossible — signalling
/// still needs the pipes).
pub fn daemonize() -> anyhow::Result<()> {
    let Detached { .. } = detach_process()?;
    sever()
}

/// Close every descriptor above 2.
///
/// This is the step that decides whether closing the laptop lid kills the
/// session. It enumerates rather than guessing a range, because guessing is how
/// one descriptor survives.
///
/// **It has no keep-list and must never grow one.** "Close everything except
/// these" is precisely the seam a descriptor survives through, and the
/// indiscriminacy is the entire value of `tests/daemonize.rs`: commit
/// `6152a29` records that skipping this function was one of three injected
/// faults that test caught. Anything that must outlive the sever is opened
/// *after* it, which is why the session's Unix socket is bound afterwards.
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

/// Both phases at once, for a session that has been cleared to detach and has
/// **not** forked yet.
///
/// The [`DetachPermit`](crate::DetachPermit) is the point: it can only come
/// from [`settle_detachability`](crate::settle_detachability), which needs the
/// nominated rung, so the descriptor-closing half is gated by the type system
/// rather than by anyone remembering it. A rung-4 session never gets one, and
/// therefore cannot reach this function at all.
///
/// Note the "has not forked yet". A caller that already ran [`detach_process`]
/// — which `oxutrm host --serve` must, because the ladder that produces the
/// permit needs a runtime and a runtime cannot cross a `fork` — wants
/// [`sever_from_ssh`] instead. Calling this there would fork a second time for
/// no reason and orphan the process the registry entry names.
///
/// [`daemonize`] itself stays public because the descriptor probe in
/// `tests/daemonize.rs` needs to call it without inventing a session.
pub fn daemonize_session(_permit: crate::DetachPermit) -> anyhow::Result<()> {
    // Taken by value rather than by reference: a permit is good for one detach,
    // and the signature is what enforces the ordering. The binding is unused on
    // purpose -- there is nothing to read from it, because its whole content is
    // the fact that it exists.
    daemonize()
}

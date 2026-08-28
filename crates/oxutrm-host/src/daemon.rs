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
    /// Every descriptor this process held at the instant it finished forking,
    /// which is the last instant at which "everything open" and "everything
    /// inherited from ssh" are the same set. [`sever_from_ssh`] closes exactly
    /// these and nothing else.
    ///
    /// The snapshot lives here rather than being taken at sever time, and that
    /// is the whole correction: by the time a session severs it has bound the
    /// UDP socket the ladder punched and QUIC adopted, and an enumeration then
    /// would close it. That socket cannot be reopened afterwards -- the NAT
    /// mapping belongs to that exact socket, which is why a nomination hands
    /// back the socket and not merely an address.
    ///
    /// This is still an enumeration and still keeps no list of exceptions. It
    /// enumerates at the right moment instead of the wrong one.
    inherited: Vec<i32>,
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
    // The pipe that tells the holder below when it may go. Created BEFORE the
    // fork, so every descendant inherits the write end; nothing is ever sent
    // on it, because what carries the signal is its CLOSURE. `sever_from_ssh`
    // performs that closure for free -- it closes every inherited descriptor
    // by enumeration -- and a grandchild that dies performs it too.
    let mut fds = [0i32; 2];
    // SAFETY: `pipe` writes exactly two ints into a two-int array we own.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("sever-notification pipe");
    }
    let (read_end, write_end) = (fds[0], fds[1]);

    // Close-on-exec, so the shell started at the end of the wiring does not
    // inherit it. Without this a rung-4 session's shell would hold the pipe
    // open after the session itself had gone, and the holder would wait for a
    // process that is no longer the session.
    // SAFETY: `fcntl` on a descriptor this function owns.
    unsafe { libc::fcntl(write_end, libc::F_SETFD, libc::FD_CLOEXEC) };

    // SAFETY: `fork` is called before this process has created any thread —
    // rule 2 above — so there is no other thread whose lock the child could
    // inherit held. The child does nothing between here and `_exit`/the
    // syscalls below that allocates or takes a lock.
    match unsafe { libc::fork() } {
        -1 => {
            // SAFETY: two descriptors this function owns and is abandoning.
            unsafe {
                libc::close(read_end);
                libc::close(write_end);
            }
            return Err(std::io::Error::last_os_error()).context("first fork");
        }
        0 => {
            // SAFETY: the holder's end has no reader here.
            unsafe { libc::close(read_end) };
        }
        first_child => {
            // SAFETY: the session's end belongs to the descendants.
            unsafe { libc::close(write_end) };
            hold_ssh_open(first_child, read_end);
        }
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
        // SAFETY: see [`hold_ssh_open`] for why this one still exits at once.
        // It closes its copy of the write end as it goes, which is what leaves
        // exactly one holder of it: the grandchild.
        _ => unsafe { libc::_exit(0) },
    }

    // Anything this process creates from here on is its own business.
    rustix::process::umask(rustix::fs::Mode::RWXG | rustix::fs::Mode::RWXO);

    Ok(Detached {
        inherited: open_descriptors()?,
    })
}

/// Where a process's own open descriptors can be enumerated, in the order
/// they are tried.
///
/// `/dev/fd` first because it is the one **both** systems have: on macOS it is
/// a devfs directory and the only answer there is, and on Linux it is the
/// conventional symlink to `/proc/self/fd`. The Linux path is kept as a second
/// candidate rather than dropped, because `/dev/fd` is a userspace convention
/// there — a stripped container image that never created it would otherwise
/// lose the ability to detach, and the static musl binary this ships as is
/// exactly what gets copied into one.
///
/// Two candidates and not a `cfg`: a `cfg` would make the macOS path
/// unreachable from the Linux test suite, and being able to run it here is the
/// entire reason this port is cheap.
///
/// Public for the same reason [`daemonize`] is: the descriptor probe in
/// `tests/daemonize.rs` has to look at the same descriptors from outside, and
/// a second copy of this list in the fixture would be free to drift away from
/// the one the product uses.
pub const FD_DIRS: [&str; 2] = ["/dev/fd", "/proc/self/fd"];

/// Every descriptor this process currently holds.
///
/// An enumeration and not a guessed range: closing a guessed range is how a
/// descriptor survives a detach, and `tests/daemonize.rs` exists because that
/// failure is invisible from inside the process.
fn open_descriptors() -> anyhow::Result<Vec<i32>> {
    descriptors_in(&FD_DIRS)
}

/// The body of [`open_descriptors`], against a given list of candidates.
///
/// Taking the directories as an argument is what lets the tests drive both the
/// fall-through and the stale-number filter with real directories instead of
/// asserting on the one path this machine happens to have.
fn descriptors_in(dirs: &[&str]) -> anyhow::Result<Vec<i32>> {
    for dir in dirs {
        let Ok(listing) = std::fs::read_dir(dir) else {
            continue;
        };
        let listed: Vec<i32> = listing
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()))
            .collect();
        drop_the_listing_handle();

        // The enumeration lists ITS OWN directory handle, and that handle is
        // shut as soon as the iterator drops. Keeping the number would be a
        // bug with a long fuse: this list is closed much later, and by then
        // the kernel has handed that very number to the next thing opened --
        // in a real session, the UDP socket. So each number is checked to be
        // open still. Nothing else opens a descriptor between the two, so
        // exactly one number can fail, and filtering rather than naming it
        // keeps this free of a special case.
        //
        // This was measured, not foreseen: with the stale number left in, the
        // file the probe opens after detaching came back `EBADF` from the far
        // side of the sever.
        return Ok(listed.into_iter().filter(|&fd| is_open(fd)).collect());
    }

    Err(anyhow!(
        "none of {dirs:?} could be read; oxutrm needs one of them to detach \
         safely, because closing a guessed range of descriptors is how one \
         survives"
    ))
}

/// Nothing. The `read_dir` iterator above is dropped at the end of its own
/// statement; this exists so the ordering it relies on is written down rather
/// than inferred from where a temporary happens to die.
fn drop_the_listing_handle() {}

/// Is this descriptor still open in this process?
fn is_open(fd: i32) -> bool {
    // SAFETY: `F_GETFD` reads a flag and changes nothing. An invalid
    // descriptor is reported, not undefined.
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

/// Keep the process `ssh` is waiting on alive until the session severs, then
/// exit — and never return.
///
/// # Why this exists, and why the obvious version is wrong
///
/// The obvious version is `_exit(0)` right after the first fork: the command
/// finishes, `ssh` reports it finished, and the grandchild carries on holding
/// the inherited descriptors. That is what this function replaced, and it does
/// not work, for a reason that is a property of **sshd** rather than of this
/// code.
///
/// Measured against a real sshd on 2026-08-28, with none of oxutrm involved —
/// a plain `python3 -c` that double forks and exits:
///
/// ```text
/// [out] GRANDCHILD_ALIVE                    <- it can still WRITE
/// [err] GRANDCHILD_SAW_EOF_ON_STDIN at t+1  <- but its stdin is already closed
/// [t+2s] ssh rc=0
/// ```
///
/// **Only the write direction survives.** sshd closes the session's *stdin* as
/// soon as the process it is waiting on exits, whatever else still holds the
/// descriptor. And the entire handshake reads from it: `ClientHello`, and every
/// `CandidateUpdate` that crosses while the ladder races. Detaching at once
/// therefore makes rungs 0 to 3 unreachable, and the symptom is a broken pipe
/// on the client's first message — a failure that looks like the network.
///
/// So the fork still happens first, for the reason it always did: it must
/// happen before a thread exists. What changed is *which* process leaves. The
/// original stays, holding ssh open and doing nothing else, and goes when the
/// session no longer needs ssh — which is exactly [`sever_from_ssh`], the point
/// the module has always documented as "the moment the session becomes
/// detached". A rung-4 session never severs, so this never returns, and its ssh
/// lives as long as it does. That is the behaviour rung 4 requires.
///
/// It reads nothing from the ssh pipes. Both this process and the grandchild
/// hold descriptor 0, and two readers on one pipe would take each other's
/// bytes.
fn hold_ssh_open(first_child: libc::pid_t, read_end: i32) -> ! {
    // Reap the middle process. It exits immediately after the second fork, and
    // with this process now staying it would otherwise sit as a zombie for the
    // whole life of the session.
    let mut status: libc::c_int = 0;
    // SAFETY: waiting on this process's own child.
    unsafe { libc::waitpid(first_child, &raw mut status, 0) };

    // Block until nothing holds the write end any more.
    let mut byte = [0u8; 1];
    loop {
        // SAFETY: reading one byte into a one-byte buffer we own.
        let n = unsafe { libc::read(read_end, byte.as_mut_ptr().cast(), 1) };
        if n == 0 {
            break;
        }
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if n < 0 {
            break;
        }
        // Nothing writes on this pipe. A byte would mean somebody misused it,
        // and waiting for a closure that has already been signalled some other
        // way is not better than leaving.
        break;
    }

    // SAFETY: `_exit` runs no destructor and no atexit handler — rule 1. This
    // process may hold a `RegistryGuard`'s directory open through an inherited
    // descriptor, and unwinding here would delete the live session's directory.
    unsafe { libc::_exit(0) }
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
    let (Detached { inherited }, _) = (detached, permit);
    sever(&inherited)
}

/// The body of [`sever_from_ssh`], without the two tokens.
///
/// Private, and it must stay private: every public route to it either carries
/// the tokens or is [`daemonize`], which forks immediately beforehand and is
/// documented as the whole-hog version for a caller that has no ladder to run.
fn sever(inherited: &[i32]) -> anyhow::Result<()> {
    // Do not hold a mount busy, and do not depend on a directory that may be
    // unmounted or deleted while the session runs for a week.
    std::env::set_current_dir("/").context("chdir to /")?;

    close_inherited_descriptors(inherited);
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
    let Detached { inherited } = detach_process()?;
    sever(&inherited)
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
fn close_inherited_descriptors(inherited: &[i32]) {
    for &fd in inherited {
        if fd > 2 {
            // SAFETY: a descriptor number enumerated by `open_descriptors`
            // inside `detach_process`, before this process opened anything of
            // its own.
            // Each number appears once, so there is no double close, and
            // nothing this process opened afterwards is in the list.
            unsafe { libc::close(fd) };
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd as _;

    /// A number no descriptor can have: the default soft `RLIMIT_NOFILE` on
    /// every platform oxutrm targets is orders of magnitude below it. The
    /// tests that use it assert it is closed first, so a machine that somehow
    /// proved this wrong would fail loudly rather than pass for the wrong
    /// reason.
    const IMPOSSIBLE_FD: i32 = 1_000_000;

    #[test]
    fn the_first_candidate_is_the_one_macos_has() {
        // `/proc/self/fd` does not exist on macOS, and `/dev/fd` exists on
        // both. Order is the whole portability decision, so it is pinned here
        // rather than only described above: a reordering that put the Linux
        // path first would leave every test on this machine green and break
        // `host --serve` on a Mac at the first detach.
        assert_eq!(FD_DIRS[0], "/dev/fd");
    }

    #[test]
    fn the_portable_directory_lists_this_processes_descriptors() {
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        let fd = file.as_raw_fd();

        let listed = descriptors_in(&["/dev/fd"]).expect("/dev/fd must be readable");

        assert!(
            listed.contains(&fd),
            "a descriptor this process holds must appear in {listed:?}"
        );
        for std_fd in 0..=2 {
            assert!(
                listed.contains(&std_fd),
                "fd {std_fd} missing from {listed:?}"
            );
        }
    }

    #[test]
    fn a_directory_that_cannot_be_read_falls_through_to_the_next() {
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        let fd = file.as_raw_fd();

        let listed = descriptors_in(&["/oxutrm/no/such/directory", "/dev/fd"])
            .expect("the second candidate answers");

        assert!(listed.contains(&fd), "{listed:?}");
    }

    #[test]
    fn a_number_that_is_no_longer_open_is_not_reported() {
        // The enumeration lists its own directory handle, and that handle is
        // shut before the list is used. Keeping the number would hand the
        // sever a descriptor the kernel has since given to something else --
        // in a real session, the UDP socket ICE punched. Measured as `EBADF`
        // from the far side of the sever before the filter existed.
        assert!(
            !is_open(IMPOSSIBLE_FD),
            "the premise of this test is that fd {IMPOSSIBLE_FD} cannot be open"
        );
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::fs::write(dir.path().join("0"), "").expect("naming an open descriptor");
        std::fs::write(dir.path().join(IMPOSSIBLE_FD.to_string()), "")
            .expect("naming a closed one");

        let listed =
            descriptors_in(&[dir.path().to_str().expect("utf-8")]).expect("the directory reads");

        assert!(listed.contains(&0), "{listed:?}");
        assert!(!listed.contains(&IMPOSSIBLE_FD), "{listed:?}");
    }

    #[test]
    fn when_nothing_can_be_read_the_error_names_every_candidate() {
        let err = descriptors_in(&["/oxutrm/no/such/directory", "/oxutrm/nor/this/one"])
            .expect_err("no candidate can answer");
        let text = format!("{err:#}");
        assert!(text.contains("/oxutrm/no/such/directory"), "{text}");
        assert!(text.contains("/oxutrm/nor/this/one"), "{text}");
    }
}

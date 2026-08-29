//! A descriptor that becomes readable when the child exits.
//!
//! # Why this exists rather than watching the PTY for EOF
//!
//! The session loop wants to sleep until something happens, and one of the
//! things that can happen is the child exiting. `try_wait` cannot be waited
//! on, so the loop polls - which is where its idle CPU goes.
//!
//! The tempting free answer is to treat EOF on the PTY controller as the
//! exit. It is a proxy, and it was measured against the real thing on both
//! platforms before this module was written:
//!
//! - **A child that closes its terminal and keeps running gives EOF on
//!   Linux** (`exec 0<&- 1>&- 2>&-; sleep 3`), while the shell is alive. As
//!   an exit signal that tears down a live session, which is the one thing
//!   this project exists to avoid.
//! - **That EOF is permanent**: 200 of 200 polls reported it. An `AsyncFd`
//!   arm over it fires forever at full CPU - the same spin hazard already
//!   documented for the keyboard, so it would not even remove the busy loop
//!   it was proposed to remove.
//! - The failure that was *predicted* and did **not** happen: `sleep 30 &
//!   exit 3` was expected to leave the controller open via the grandchild and
//!   so miss the exit. Both kernels hang up the controlling terminal when the
//!   session leader exits, and EOF arrived anyway, on macOS and on Linux.
//!   The prediction was wrong; only the case above stands.
//!
//! So: an exact primitive, per child, costing no dependency.
//!
//! # The contract
//!
//! Readability is a **hint**. `HostTerm::child_exited` stays the authority in
//! every case, including when there is no descriptor at all. A caller waits
//! for readability, asks `child_exited`, and then stops watching; nothing here
//! needs draining, and nothing re-arms.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

// Both supported platforms have an exact primitive for this. Anywhere else
// there is no answer to fall back to that is not a proxy, so say so at build
// time rather than degrade silently to something that can end a live session.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "oxutrm-term needs a pollable child-exit primitive: pidfd (Linux) or kqueue (macOS)"
);

/// A descriptor that becomes readable when one specific child exits.
///
/// [`as_fd`](ExitWake::as_fd) returns `None` when the OS declined to watch
/// the child because it had already exited. That is not an error and not a
/// missed exit: the child is a zombie until we reap it, so `child_exited`
/// still has the answer, and a caller that has no descriptor to wait on
/// simply asks immediately.
pub struct ExitWake {
    fd: Option<OwnedFd>,
}

impl ExitWake {
    /// The descriptor to wait on, if the OS gave us one.
    ///
    /// Never `read` it. It is a readiness source, not a stream, and the two
    /// platforms put entirely different things behind it.
    pub fn as_fd(&self) -> Option<BorrowedFd<'_>> {
        self.fd.as_ref().map(AsFd::as_fd)
    }

    /// The wake for a child that is already gone.
    fn unarmed() -> ExitWake {
        ExitWake { fd: None }
    }
}

/// `pidfd_open` is a safe call, so the Linux half needs no `unsafe` at all.
#[cfg(target_os = "linux")]
pub(crate) fn watch(pid: u32) -> anyhow::Result<ExitWake> {
    use rustix::process::{Pid, PidfdFlags};

    let Some(pid) = Pid::from_raw(pid as i32) else {
        return Ok(ExitWake::unarmed());
    };
    // `PIDFD_NONBLOCK` because the caller may hand this to a reactor that
    // requires a non-blocking descriptor.
    match rustix::process::pidfd_open(pid, PidfdFlags::NONBLOCK) {
        Ok(fd) => Ok(ExitWake { fd: Some(fd) }),
        // The child beat us to it between `spawn` and here.
        Err(rustix::io::Errno::SRCH) => Ok(ExitWake::unarmed()),
        Err(e) => Err(anyhow::Error::new(e).context("watching the child for its exit")),
    }
}

/// A kqueue holding one `EVFILT_PROC`/`NOTE_EXIT` registration. The kqueue
/// descriptor is itself pollable, which is what lets an ordinary reactor wait
/// on it.
///
/// Unlike Linux, **this cannot be registered on a child that has already
/// exited**: `kevent` answers `ESRCH` for a zombie. Measured, and the reason
/// [`ExitWake::as_fd`] is an `Option` rather than an `OwnedFd`.
#[cfg(target_os = "macos")]
pub(crate) fn watch(pid: u32) -> anyhow::Result<ExitWake> {
    use anyhow::Context as _;
    use rustix::event::kqueue::{Event, EventFilter, EventFlags, ProcessEvents, kevent, kqueue};

    let Some(pid) = rustix::process::Pid::from_raw(pid as i32) else {
        return Ok(ExitWake::unarmed());
    };
    let kq = kqueue().context("opening a kqueue for the child's exit")?;
    let registration = Event::new(
        EventFilter::Proc {
            pid,
            flags: ProcessEvents::EXIT,
        },
        // RECEIPT makes the registration report its own outcome instead of
        // failing silently, which is how ESRCH is told apart from success.
        EventFlags::ADD | EventFlags::RECEIPT,
        std::ptr::null_mut(),
    );
    // `Buffer for &mut Vec<T>` sizes the event list from `len()`, NOT from
    // capacity: a `with_capacity` vec is a zero-length event list that can
    // never receive anything. `spare_capacity` is the one that means "fill me
    // up to capacity".
    let mut reply: Vec<Event> = Vec::with_capacity(1);

    // The second exemption in this crate. `kevent` is an `unsafe fn` because
    // the file descriptors named by an `Event` must outlive the kqueue; here
    // the event names a PID and no descriptor at all, so the clause it is
    // guarding cannot be violated by this call.
    #[allow(unsafe_code)]
    let outcome = unsafe {
        kevent(
            &kq,
            &[registration],
            rustix::buffer::spare_capacity(&mut reply),
            Some(std::time::Duration::ZERO),
        )
    };

    // ESRCH arrives one of two ways depending on whether the reply had room.
    // Fault injection says the RECEIPT reply below is the live path and this
    // arm is unreachable as written; it is kept because the alternative is a
    // hard spawn failure for a benign race, not because it is covered.
    match outcome {
        Ok(_) => {}
        Err(rustix::io::Errno::SRCH) => return Ok(ExitWake::unarmed()),
        Err(e) => return Err(anyhow::Error::new(e).context("watching the child for its exit")),
    }
    if let Some(event) = reply.first() {
        match event.data() {
            0 => {}
            e if e == rustix::io::Errno::SRCH.raw_os_error() as i64 => {
                return Ok(ExitWake::unarmed());
            }
            e => anyhow::bail!("watching the child for its exit: errno {e}"),
        }
    }
    Ok(ExitWake { fd: Some(kq) })
}

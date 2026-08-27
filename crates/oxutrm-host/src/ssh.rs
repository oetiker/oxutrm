//! Driving `ssh`, and failing usefully when it does not work.
//!
//! oxutrm never parses `~/.ssh/config`. It shells out to `ssh` and assumes the
//! user has already made `ssh <target>` work, by whatever means — jump host,
//! reverse tunnel, VPN, direct. That keeps the trust root exactly where it was.
//!
//! # The failure modes are the substance
//!
//! Four things go wrong, and each needs a different sentence, because each
//! sends the user somewhere different:
//!
//! | What happened | What the user must do |
//! |---|---|
//! | `ssh` is not installed | install OpenSSH locally |
//! | `ssh` ran and failed | read its stderr — the reason is only there |
//! | the **remote** binary is missing | install oxutrm on the far end |
//! | login worked, nothing was said | look at the remote shell's rc files |
//!
//! The third is the one that matters most. It is the likeliest first-run
//! problem, and reporting it as "connection failed" sends somebody to their
//! firewall for an hour. It gets its own variant and its own advice.
//!
//! # On detachability
//!
//! `HostHello.detachable` is the host's **intent**, written before the ladder
//! has run — the candidates travel in that same message — so at handshake time
//! nobody knows which rung will be nominated. This module therefore reads the
//! field and passes it on, and draws no conclusion from it. The outcome is
//! settled later, from the nominated rung, by
//! [`SessionMeta::set_detachable`](crate::SessionMeta::set_detachable).

use std::ffi::{OsStr, OsString};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use oxutrm_proto::{ProtoError, Signal};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::signalling::{read_signal_async, write_signal_async};

/// The command the wrapper asks the far end to run.
pub const REMOTE_SERVE: [&str; 3] = ["oxutrm", "host", "--serve"];

/// How to launch `ssh`.
///
/// This is the injection point that makes the bootstrap testable without a
/// server: tests substitute a local fixture that behaves like `ssh`, including
/// the banner and motd that a quiet developer machine never produces.
///
/// It overrides the *command* rather than hiding the process behind a trait, so
/// the tests drive a real subprocess over real pipes. The pipe handling is a
/// large part of what can go wrong here — a deadlock on an undrained stderr, a
/// message stuck in a buffer — and a trait-shaped mock would bypass precisely
/// the code that has those bugs.
#[derive(Clone, Debug)]
pub struct SshLauncher {
    program: OsString,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
}

impl SshLauncher {
    /// The real thing: `ssh` from `$PATH`.
    #[must_use]
    pub fn ssh() -> SshLauncher {
        SshLauncher::command("ssh")
    }

    /// Run `program` instead of `ssh`. For tests, and for a user who needs a
    /// wrapper script.
    pub fn command(program: impl Into<OsString>) -> SshLauncher {
        SshLauncher {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
        }
    }

    /// An extra argument, placed before the target, exactly where an `ssh`
    /// option belongs.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> SshLauncher {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> SshLauncher {
        self.envs.push((key.into(), value.into()));
        self
    }

    #[must_use]
    pub fn program(&self) -> &OsStr {
        &self.program
    }
}

impl Default for SshLauncher {
    fn default() -> Self {
        SshLauncher::ssh()
    }
}

/// Why the bootstrap did not happen.
///
/// Every variant exists because it sends the user somewhere different. Merging
/// any two of them would save a little code and cost somebody an afternoon.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    /// `ssh` itself could not be started. Not a connection failure: nothing was
    /// ever connected.
    #[error(
        "`{program}` could not be run: is OpenSSH installed and on your PATH? \
         Nothing was contacted, so this is not a network problem."
    )]
    SshNotFound { program: String },

    /// `ssh` ran and exited non-zero. Its stderr is the only place the reason
    /// exists — host key mismatch, permission denied, no route — so it is
    /// reproduced rather than summarised.
    #[error("ssh exited with status {}: {stderr}", status.map_or("(signal)".to_string(), |c| c.to_string()))]
    SshFailed { status: Option<i32>, stderr: String },

    /// The login worked and `oxutrm` was not there. The likeliest first-run
    /// problem, and the one most often misdiagnosed.
    #[error(
        "the oxutrm binary was not found on {target}. Install oxutrm there — it \
         has to be on both ends, and on the remote PATH for a non-interactive \
         ssh, which does not read ~/.bashrc. ssh said: {stderr}"
    )]
    RemoteBinaryMissing { target: String, stderr: String },

    /// Logged in, ran something, produced no handshake. Usually a remote rc
    /// file writing to stdout, which corrupts the channel.
    #[error(
        "{target} completed the login but sent no oxutrm handshake. If a shell \
         startup file on that host writes to stdout, it will have corrupted the \
         channel. ssh said: {stderr}"
    )]
    NoSignal { target: String, stderr: String },

    /// The far end said something, and it was wrong. Version skew lands here,
    /// and is deliberately fatal rather than a downgrade.
    #[error("{0}")]
    Protocol(#[from] ProtoError),

    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// A live signalling channel: a child `ssh` with its pipes.
///
/// SSH stays open for the whole of connection establishment, because NAT
/// traversal needs a bidirectional signalling channel — candidates are
/// discovered asynchronously and must be exchanged as they appear.
pub struct SshChannel {
    target: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Filled by a background task. See `open` for why that is not optional.
    stderr: Arc<Mutex<String>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for SshChannel {
    /// Hand-written because pipes and a `Child` are not usefully printable,
    /// and because a derived one would be a place for `HostHello.psk` to end
    /// up in a log the day this struct grows a field holding one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshChannel")
            .field("target", &self.target)
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

/// How much of the child's stderr is worth keeping for a diagnostic.
///
/// `ssh` is spawned before anything about the far end has been established, so
/// until it succeeds the length of its stderr is the *peer's* choice. 64 KiB is
/// two orders of magnitude more than any real failure prints - a banner, a motd
/// and "Permission denied" together are a few hundred bytes - and it turns a
/// buffer the remote sizes into one we do.
const STDERR_KEPT: usize = 64 * 1024;

/// Read `r` to the end, retaining only its first `keep` bytes.
///
/// # Why it keeps reading after it stops keeping
///
/// Because the alternative deadlocks. Stopping the read at the cap leaves the
/// pipe full and the child blocked on `write`, so the connection hangs with no
/// error at all - strictly worse than the unbounded buffer the cap replaces.
/// Draining costs nothing: the bytes are discarded as they arrive.
///
/// # Why the first bytes and not the last
///
/// A flood must not be able to evict the very thing stderr is kept for. `ssh`
/// says why it failed early, so a tail buffer would let a chatty - or hostile -
/// host push that reason out with padding of its own choosing.
async fn drain_stderr<R>(r: &mut R, keep: usize) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut kept = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => return kept,
            Ok(n) => {
                let room = keep.saturating_sub(kept.len());
                if room > 0 {
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
    }
}

impl SshChannel {
    /// Spawn `ssh <target> oxutrm host --serve` and take its pipes.
    pub async fn open(launcher: &SshLauncher, target: &str) -> Result<SshChannel, BootstrapError> {
        let mut cmd = Command::new(&launcher.program);
        cmd.args(&launcher.args);
        for (k, v) in &launcher.envs {
            cmd.env(k, v);
        }
        cmd.arg(target);
        cmd.args(REMOTE_SERVE);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Without this, a child that outlives us keeps running after the
            // wrapper is gone.
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                BootstrapError::SshNotFound {
                    program: launcher.program.to_string_lossy().into_owned(),
                }
            } else {
                BootstrapError::Io(e)
            }
        })?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr was piped");

        // Drain stderr continuously rather than reading it at the end. A chatty
        // ssh fills the pipe buffer, and then the child blocks on write while
        // we block on read: the connection hangs forever with no error at all.
        // Collecting it also means the reason for a failure is already in hand
        // by the time we need it.
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        let stderr_task = tokio::spawn(async move {
            // Errors here are not worth reporting: stderr is diagnostic, and
            // losing it must never be the reason a working link fails.
            let buf = drain_stderr(&mut stderr_pipe, STDERR_KEPT).await;
            let text = String::from_utf8_lossy(&buf).into_owned();
            if let Ok(mut guard) = sink.lock() {
                guard.push_str(&text);
            }
        });

        Ok(SshChannel {
            target: target.to_string(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr,
            stderr_task: Some(stderr_task),
        })
    }

    /// Read the next `Signal`, skipping the banner, the motd and whatever else
    /// the login printed.
    ///
    /// End of stream is not returned as an I/O error: it means the far end is
    /// finished, so the child is reaped and the reason is worked out from its
    /// exit status and its stderr.
    pub async fn recv(&mut self) -> Result<Signal, BootstrapError> {
        match read_signal_async(&mut self.stdout).await {
            Ok(s) => Ok(s),
            Err(ProtoError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                Err(self.diagnose().await)
            }
            Err(e) => Err(BootstrapError::Protocol(e)),
        }
    }

    /// Send one `Signal` and flush it. A message left in a buffer looks exactly
    /// like a peer that never answered.
    pub async fn send(&mut self, s: &Signal) -> Result<(), BootstrapError> {
        write_signal_async(&mut self.stdin, s)
            .await
            .map_err(BootstrapError::Protocol)
    }

    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Everything `ssh` has written to stderr so far.
    #[must_use]
    pub fn stderr_so_far(&self) -> String {
        self.stderr.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// The far end stopped talking. Work out why, and say the one useful thing.
    async fn diagnose(&mut self) -> BootstrapError {
        // Let the stderr drain finish, so a reason written just before exit is
        // not lost to a race.
        if let Some(task) = self.stderr_task.take() {
            let _ = task.await;
        }
        let stderr = self.stderr_so_far();
        let status = self.child.wait().await.ok();
        let code = status.and_then(|s| s.code());
        let success = status.is_some_and(|s| s.success());

        // A shell that cannot find a command exits 127 and says so. Checking
        // both, because the exit code alone is lost when ssh is wrapped in
        // something that rewrites it.
        if code == Some(127) || looks_like_missing_command(&stderr) {
            return BootstrapError::RemoteBinaryMissing {
                target: self.target.clone(),
                stderr: trimmed(&stderr),
            };
        }
        if !success {
            return BootstrapError::SshFailed {
                status: code,
                stderr: trimmed(&stderr),
            };
        }
        BootstrapError::NoSignal {
            target: self.target.clone(),
            stderr: trimmed(&stderr),
        }
    }
}

/// Did a shell tell us the command does not exist?
///
/// Matched loosely on purpose: the wording differs between `bash`, `dash`,
/// `zsh` and busybox, and getting this wrong sends the user to their firewall.
fn looks_like_missing_command(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    lower.contains("command not found")
        || lower.contains(": not found")
        || lower.contains("no such file or directory")
}

/// Keep stderr readable in a one-line error without losing the reason.
fn trimmed(stderr: &str) -> String {
    let text = stderr.trim();
    if text.is_empty() {
        return "(nothing)".to_string();
    }
    const LIMIT: usize = 2000;
    if text.len() <= LIMIT {
        return text.to_string();
    }
    // Keep the END: ssh puts the banner first and the reason last.
    let tail: String = text
        .chars()
        .rev()
        .take(LIMIT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("(earlier output elided) {tail}")
}

#[cfg(test)]
mod stderr_tests {
    use super::*;

    /// A pipe that serves `head` bytes of `b'a'`, then `tail` bytes of `b'z'`,
    /// then ends — and counts what it handed out.
    ///
    /// The two fills are what make "the first bytes" a checkable claim rather
    /// than a length assertion: a drain that kept the *tail* would return the
    /// right number of the wrong bytes.
    struct Chatty {
        head: usize,
        tail: usize,
        served: usize,
    }

    impl Chatty {
        fn new(head: usize, tail: usize) -> Chatty {
            Chatty {
                head,
                tail,
                served: 0,
            }
        }

        fn total(&self) -> usize {
            self.head + self.tail
        }
    }

    impl tokio::io::AsyncRead for Chatty {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let me = self.get_mut();
            let total = me.head + me.tail;
            let n = buf.remaining().min(total - me.served);
            let filled = buf.initialize_unfilled_to(n);
            for (i, slot) in filled.iter_mut().enumerate() {
                *slot = if me.served + i < me.head { b'a' } else { b'z' };
            }
            buf.advance(n);
            me.served += n;
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn ordinary_stderr_is_kept_whole() {
        let mut pipe = Chatty::new(100, 0);
        let kept = drain_stderr(&mut pipe, STDERR_KEPT).await;
        assert_eq!(kept, vec![b'a'; 100], "a short stderr must survive intact");
    }

    /// What a remote can make us hold is the peer's choice today. `ssh` is
    /// spawned before anything about the far end has been established, so a
    /// host that writes to stderr for ever grows this buffer for ever.
    #[tokio::test]
    async fn a_flood_of_stderr_is_capped_at_what_we_agreed_to_keep() {
        const KEEP: usize = 4096;
        let mut pipe = Chatty::new(KEEP, KEEP * 16);
        let kept = drain_stderr(&mut pipe, KEEP).await;
        assert_eq!(
            kept.len(),
            KEEP,
            "the drain held {} bytes of a stderr the peer chose the length of",
            kept.len()
        );
    }

    /// The *first* bytes, not the last. A flood must not be able to evict the
    /// diagnostic we keep stderr for: `ssh` says why it failed early, and a
    /// tail buffer would let a chatty - or hostile - host push that reason out
    /// with padding it chose itself.
    #[tokio::test]
    async fn the_bytes_kept_are_the_ones_that_arrived_first() {
        const KEEP: usize = 4096;
        let mut pipe = Chatty::new(KEEP, KEEP * 16);
        let kept = drain_stderr(&mut pipe, KEEP).await;
        assert!(
            kept.iter().all(|&b| b == b'a'),
            "the drain kept later bytes over earlier ones, so a flood can \
             evict the reason a connection failed"
        );
    }

    /// The property the whole task exists for, and the one a naive cap breaks.
    ///
    /// Stopping the read at the cap leaves the pipe full, the child blocked on
    /// write, and the connection hung with no error at all - which is worse
    /// than the unbounded buffer this cap replaces. So the drain must keep
    /// reading to EOF and merely stop *retaining*.
    #[tokio::test]
    async fn the_pipe_is_drained_to_the_end_even_after_the_cap_is_reached() {
        const KEEP: usize = 4096;
        let mut pipe = Chatty::new(KEEP, KEEP * 16);
        let total = pipe.total();
        let _ = drain_stderr(&mut pipe, KEEP).await;
        assert_eq!(
            pipe.served, total,
            "the drain stopped reading at the cap, so a chatty ssh fills the \
             pipe, blocks on write, and the connection hangs with no error"
        );
    }
}

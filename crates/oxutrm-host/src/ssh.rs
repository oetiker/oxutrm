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
            let mut buf = Vec::new();
            // Errors here are not worth reporting: stderr is diagnostic, and
            // losing it must never be the reason a working link fails.
            if stderr_pipe.read_to_end(&mut buf).await.is_ok() {
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Ok(mut guard) = sink.lock() {
                    guard.push_str(&text);
                }
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

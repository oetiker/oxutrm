//! The one dispatcher behind both doors of a session process (switcher spec
//! §2.2, §4.1).
//!
//! A session process -- or a lobby -- has two doors: the QUIC control stream
//! from its own client ([`Via::Client`]) and its Unix socket, from a sibling
//! session or from `oxutrm host --attach` over ssh ([`Via::Socket`]). Both
//! hand every connection to [`serve`], on a task of its own, which reads the
//! first line -- an [`Open`] -- under [`OPEN_TIMEOUT`] and answers it here.
//!
//! Only `Attach` goes on to the session's serial attach loop
//! (`listener::serve_attaches`, one exchange at a time). Everything else is
//! answered from state the door holds itself -- the session's record and
//! whether a client is attached -- so `Myself` and `Sessions` answer within
//! milliseconds even while the session is mid-attach, and a `Sessions` fetch
//! never disturbs a sibling's standby search.
//!
//! A request is always served by the process that owns what it touches: a
//! session only ever writes its own `meta.json` and signals its own shell.

// This runs in the host daemon, whose stderr is nobody's screen, under the
// same rule as the client so nothing here starts printing by accident.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxutrm_host::registry::{Registry, RegistryGuard, SessionMeta};
use oxutrm_host::signalling::{read_answer_async, read_line_async, write_line_async};
use oxutrm_net::NetConfig;
use oxutrm_proto::{
    Answer, Attached, Name, Open, PROTO_VERSION, ProtoError, Reply, Request, Role, SessionEntry,
    SessionId,
};
use tokio::io::{AsyncBufRead, AsyncWrite};

use crate::control::DoorRequest;
use crate::host_session::{LoopCmd, Presence};

/// How long a connection may take to say what it wants. Generous for a
/// peer on the same machine or a live link, short enough that a silent one
/// costs a task for seconds, not for ever (the QUIC idle timeout is off).
pub(crate) const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a sibling has to answer `Myself` before it is listed from its
/// `meta.json` as `Unknown` (shown `?`).
pub(crate) const SIBLING_TIMEOUT: Duration = Duration::from_secs(2);

/// Which door a connection came through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Via {
    /// The control stream of a link: this session's own client.
    Client,
    /// The Unix socket: a sibling, or `oxutrm host --attach`.
    Socket,
}

/// Where `Attach` requests go: the serial attach loop's two inboxes. Two,
/// so the loop can let a primary pre-empt a standby's exchange (listener).
#[derive(Clone)]
pub(crate) struct AttachQueue {
    pub(crate) primary: tokio::sync::mpsc::Sender<DoorRequest>,
    pub(crate) standby: tokio::sync::mpsc::Sender<DoorRequest>,
}

/// The receiving half of an [`AttachQueue`], for the serial attach loop.
pub(crate) struct AttachInbox {
    pub(crate) primary: tokio::sync::mpsc::Receiver<DoorRequest>,
    pub(crate) standby: tokio::sync::mpsc::Receiver<DoorRequest>,
}

/// A queue and its inbox. One deep each: the loop is serial, and a request
/// that finds the queue full waits for the attempt ahead, which is bounded.
pub(crate) fn attach_queue() -> (AttachQueue, AttachInbox) {
    let (primary, primary_rx) = tokio::sync::mpsc::channel(1);
    let (standby, standby_rx) = tokio::sync::mpsc::channel(1);
    (
        AttachQueue { primary, standby },
        AttachInbox {
            primary: primary_rx,
            standby: standby_rx,
        },
    )
}

/// The session's own record, as the door answers from it.
struct Own {
    /// What `meta.json` says, kept current by the attach loop and by
    /// renames. Not the loop's working copy: that one is held across an
    /// exchange, and this must answer while one runs.
    meta: SessionMeta,
    /// The registry entry, while there is one: a lobby has none. Dropping it
    /// removes the session's directory, its socket with it.
    guard: Option<RegistryGuard>,
    /// Where `Attach` goes, while the session is registered and severed:
    /// `None` in a lobby, and on rung 4, which has no socket.
    attaches: Option<AttachQueue>,
    /// The socket's accept loop and the serial attach loop, while they run.
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// No shell: a connect-time lobby, or a session killed by its client.
    lobby: bool,
}

/// How the door reaches the session's loop: what only the loop can do, and
/// where completed attaches go.
pub(crate) struct LoopLink {
    pub(crate) cmds: tokio::sync::mpsc::Sender<LoopCmd>,
    pub(crate) attached: tokio::sync::mpsc::Sender<crate::attach_exchange::Attached>,
}

/// What one session process's doors share.
pub(crate) struct Door {
    /// The registry directory: where siblings are found and this session
    /// registers.
    registry: PathBuf,
    /// What the attach loop runs exchanges with.
    cfg: NetConfig,
    own: std::sync::Mutex<Own>,
    presence: Presence,
    /// `None` for a door with no loop behind it: the tests' doors.
    to_loop: Option<LoopLink>,
    /// The binary a sibling runs: this session's own (switcher spec §3.4),
    /// resolved once at startup. `None` where none could be.
    exe: Option<PathBuf>,
}

impl Door {
    /// The door of a session process: a lobby until [`Door::register`].
    pub(crate) fn process(
        registry: PathBuf,
        meta: SessionMeta,
        presence: Presence,
        cfg: NetConfig,
        to_loop: LoopLink,
        exe: Option<PathBuf>,
    ) -> Arc<Door> {
        Arc::new(Door {
            registry,
            cfg,
            own: std::sync::Mutex::new(Own {
                meta,
                guard: None,
                attaches: None,
                tasks: Vec::new(),
                lobby: true,
            }),
            presence,
            to_loop: Some(to_loop),
            exe,
        })
    }

    /// A door assembled from its parts, with no loop behind it: a session
    /// registered as `guard`, whose `Attach` requests go to `attaches`.
    #[cfg(test)]
    pub(crate) fn assembled(
        registry: PathBuf,
        meta: SessionMeta,
        guard: Option<RegistryGuard>,
        presence: Presence,
        attaches: Option<AttachQueue>,
    ) -> Arc<Door> {
        Arc::new(Door {
            registry,
            cfg: NetConfig::default(),
            own: std::sync::Mutex::new(Own {
                meta,
                guard,
                attaches,
                tasks: Vec::new(),
                lobby: false,
            }),
            presence,
            to_loop: None,
            exe: None,
        })
    }

    fn own(&self) -> std::sync::MutexGuard<'_, Own> {
        // A poisoned lock means a panic elsewhere while holding a copy; the
        // record inside is still the last whole one written.
        self.own
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The session's record as it stands.
    pub(crate) fn meta(&self) -> SessionMeta {
        self.own().meta.clone()
    }

    /// The session's id.
    fn id(&self) -> String {
        self.own().meta.session_id.clone()
    }

    /// R13 and the doors after it: record the session in the registry --
    /// under its name, refused if a live session has it -- and, where it
    /// severed from ssh, bind its socket and start the accept and attach
    /// loops. The one way a session process becomes a registered session,
    /// on a first connect and when a lobby is asked for `New` alike.
    pub(crate) fn register(self: &Arc<Self>) -> anyhow::Result<()> {
        use anyhow::Context as _;
        let mut own = self.own();
        let guard = RegistryGuard::register_in(&self.registry, &own.meta)?;
        if !own.meta.detachable {
            // Rung 4: its QUIC runs inside ssh, so a socket bound anyway
            // would offer an attach that cannot outlive it.
            own.guard = Some(guard);
            own.lobby = false;
            return Ok(());
        }
        // A failure from here drops `guard`, and the entry with it: the door
        // is still the lobby it was.
        let path = guard.socket_path();
        oxutrm_host::check_socket_path_length(&path)?;
        let listener = tokio::net::UnixListener::bind(&path)
            .with_context(|| format!("binding the session socket at {}", path.display()))?;
        own.guard = Some(guard);
        own.lobby = false;
        if let Some(to_loop) = &self.to_loop {
            let (queue, inbox) = attach_queue();
            own.attaches = Some(queue);
            own.tasks.push(tokio::spawn(crate::listener::serve_attaches(
                Arc::clone(self),
                self.cfg.clone(),
                crate::listener::ATTACH_TIMEOUT,
                inbox,
                to_loop.attached.clone(),
            )));
        }
        own.tasks.push(tokio::spawn(crate::listener::accept_doors(
            listener,
            Arc::clone(self),
        )));
        Ok(())
    }

    /// The other way round: the registry entry and the socket go, and the
    /// loops behind them stop. A lobby from here.
    ///
    /// Not awaited: this runs on a door task, and the loops it stops hold
    /// clones of the door. [`Door::close`] is the awaited end of a process.
    fn unregister(&self) {
        let (guard, tasks) = {
            let mut own = self.own();
            own.lobby = true;
            own.attaches = None;
            (own.guard.take(), std::mem::take(&mut own.tasks))
        };
        for t in &tasks {
            t.abort();
        }
        drop(guard);
    }

    /// The end of the process: stop the loops, wait until they are gone --
    /// they hold clones of this door, and through it nothing else -- and
    /// remove the registry entry.
    ///
    /// Abort AND await, and why both, is `listener::close_the_door`'s own
    /// note. The guard is dropped here explicitly, not by the last clone of
    /// the door, because a control server can hold one for as long as its
    /// link stays open.
    pub(crate) async fn close(&self) {
        let tasks = std::mem::take(&mut self.own().tasks);
        for t in tasks {
            crate::listener::close_the_door(t).await;
        }
        let guard = {
            let mut own = self.own();
            own.lobby = true;
            own.attaches = None;
            own.guard.take()
        };
        drop(guard);
    }

    /// An attach completed with `m` as its working record: what it moved --
    /// the generation, the size, detachability -- becomes the session's, and
    /// is written to `meta.json`. The name is the door's and stays.
    pub(crate) fn record_attach(&self, m: &SessionMeta) {
        let mut own = self.own();
        own.meta.attach_id = m.attach_id;
        own.meta.size = m.size;
        own.meta.detachable = m.detachable;
        if let Some(guard) = &own.guard {
            // `update`'s own doc: "after every attach, because attach_id
            // moves". A failed write costs `--list` a stale generation.
            let _ = guard.update(&own.meta);
        }
    }

    /// This session as an entry, seen through `via`: its own client sees
    /// itself as `this`, `Here`; anybody else sees whether it has a client.
    fn own_entry(&self, via: Via) -> Option<SessionEntry> {
        let offer = self.own().meta.offer()?;
        Some(match via {
            Via::Client => offer.entry(Attached::Here, true),
            Via::Socket => {
                let attached = if self.presence.attached(Instant::now()) {
                    Attached::Elsewhere
                } else {
                    Attached::No
                };
                offer.entry(attached, false)
            }
        })
    }

    /// `New` in a lobby: register under `name`, then have the loop start the
    /// shell. The lobby becomes the session, with the id it already has.
    async fn start(self: &Arc<Self>, name: Option<Name>) -> Reply {
        let Some(to_loop) = &self.to_loop else {
            return Reply::Refused("this session cannot start a shell".to_string());
        };
        self.own().meta.name = name.map(String::from);
        if let Err(e) = self.register() {
            self.own().meta.name = None;
            return Reply::Refused(format!("{e:#}"));
        }
        let (reply, started) = tokio::sync::oneshot::channel();
        let asked = to_loop.cmds.send(LoopCmd::StartShell { reply }).await;
        match (asked, started.await) {
            (Ok(()), Ok(Ok(()))) => match self.own_entry(Via::Client) {
                Some(e) => Reply::Entry(e),
                None => Reply::Refused("the new session has no entry".to_string()),
            },
            (_, Ok(Err(why))) => {
                self.unregister();
                Reply::Refused(why)
            }
            _ => {
                self.unregister();
                Reply::Refused("the session ended".to_string())
            }
        }
    }

    /// `Kill` of this session: the loop hangs the shell up, then the entry
    /// goes, then -- and only then -- `Done` (switcher spec §3.4). From its
    /// own client the session stays as a lobby; from a sibling it ends.
    async fn kill_self<W: AsyncWrite + Unpin>(&self, via: Via, writer: &mut W) {
        let refused = |why: &str| Reply::Refused(why.to_string());
        let reply = if self.own().lobby {
            refused("this session has no shell left to kill")
        } else if let Some(to_loop) = &self.to_loop {
            let (reply, code) = tokio::sync::oneshot::channel();
            let (written, written_rx) = tokio::sync::oneshot::channel();
            let asked = to_loop
                .cmds
                .send(LoopCmd::Kill {
                    end: via == Via::Socket,
                    reply,
                    written: written_rx,
                })
                .await;
            match (asked, code.await) {
                (Ok(()), Ok(_code)) => {
                    self.unregister();
                    let _ = write_line_async(writer, &Reply::Done).await;
                    let _ = written.send(());
                    return;
                }
                _ => refused("the session ended before its shell could be killed"),
            }
        } else {
            refused("this session cannot kill its shell")
        };
        let _ = write_line_async(writer, &reply).await;
    }

    /// `Rename` of this session, under the registry's name lock.
    fn rename_self(&self, via: Via, name: Option<Name>) -> Reply {
        let renamed = {
            let mut own = self.own();
            let own = &mut *own;
            match &own.guard {
                None => Err("this session has no entry to name".to_string()),
                Some(guard) => guard
                    .rename(&mut own.meta, name.map(String::from))
                    .map_err(|e| format!("{e:#}")),
            }
        };
        match renamed.and_then(|()| {
            self.own_entry(via)
                .ok_or_else(|| "this session has no entry".to_string())
        }) {
            Ok(e) => Reply::Entry(e),
            Err(why) => Reply::Refused(why),
        }
    }
}

/// `Switch` (switcher spec §3.3): open the target's socket, ask it for a
/// primary attach, and relay. The target runs its ordinary attach exchange
/// with this session's client, which keeps its link here until the new one
/// is up; nothing here changes this session.
async fn switch<R, W>(door: &Door, to: SessionId, reader: R, mut writer: W)
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let refuse = |why: String| Reply::Refused(why);
    let reply = if to.to_string() == door.id() {
        refuse("this is the session you are in".to_string())
    } else {
        let live = Registry::list_in(&door.registry).unwrap_or_default();
        match live.iter().find(|m| m.session_id == to.to_string()) {
            None => refuse(format!("no session {} on this host", to.short())),
            Some(m) if !m.detachable => refuse(format!(
                "session {} is not detachable: it dies with its ssh",
                to.short()
            )),
            Some(m) => {
                let path = Registry::socket_path_in(&door.registry, &m.session_id);
                match tokio::net::UnixStream::connect(&path).await {
                    Err(e) => refuse(format!("session {} did not answer: {e}", to.short())),
                    Ok(stream) => {
                        let (r, mut w) = stream.into_split();
                        let asked = write_line_async(
                            &mut w,
                            &Open::new(Request::Attach {
                                role: Role::Primary,
                            }),
                        )
                        .await;
                        if let Err(e) = asked {
                            refuse(format!("session {} did not answer: {e}", to.short()))
                        } else {
                            let what = format!("session {}", to.short());
                            relay_attach(&what, reader, writer, tokio::io::BufReader::new(r), w)
                                .await;
                            return;
                        }
                    }
                }
            }
        }
    };
    let _ = write_line_async(&mut writer, &reply).await;
}

/// `New` in a session (switcher spec §3.4): start a sibling as the ordinary
/// `oxutrm host --serve`, its stdin and stdout on a socket pair held here,
/// and relay the client's stream into that pair exactly as ssh carries a
/// first connect. One startup path makes every session process.
async fn new_sibling<R, W>(door: &Door, name: Option<Name>, reader: R, mut writer: W)
where
    R: AsyncBufRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let refused = |why: String| Reply::Refused(why);
    let reply = match &door.exe {
        None => refused("this session does not know its own binary".to_string()),
        Some(exe) => match name_taken(&door.registry, name.as_ref()) {
            Some(why) => refused(why),
            None => match spawn_sibling(exe, &door.registry, name.as_ref()) {
                Err(e) => refused(format!("starting a new session: {e}")),
                Ok(pair) => {
                    let (r, w) = pair.into_split();
                    relay_attach(
                        "the new session",
                        reader,
                        writer,
                        tokio::io::BufReader::new(r),
                        w,
                    )
                    .await;
                    return;
                }
            },
        },
    };
    let _ = write_line_async(&mut writer, &reply).await;
}

/// Why `name` cannot be a new session's, checked under the name lock: a
/// refusal the client can read now, rather than a sibling that fails to
/// register after its exchange. The sibling checks again as it registers.
fn name_taken(registry: &std::path::Path, name: Option<&Name>) -> Option<String> {
    let name = name?;
    let checked = oxutrm_host::NamesLock::take(registry)
        .and_then(|_lock| oxutrm_host::name_refusal(registry, name.as_str(), ""));
    match checked {
        Ok(refusal) => refusal,
        Err(e) => Some(format!("checking the name: {e:#}")),
    }
}

/// `exe host --serve [--name <name>]` with its stdin and stdout on one end
/// of a socket pair, the other end returned. Its registry is this one,
/// passed as `OXUTRM_STATE_DIR` -- the directory `registry` is in -- so the
/// sibling registers beside its parent whatever its environment says.
///
/// The child is `host --serve`'s first process, which forks the session and
/// exits at once (`detach_process`); it is waited for on a task of its own.
fn spawn_sibling(
    exe: &std::path::Path,
    registry: &std::path::Path,
    name: Option<&Name>,
) -> std::io::Result<tokio::net::UnixStream> {
    let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
    let theirs = std::os::fd::OwnedFd::from(theirs);
    let base = registry.parent().unwrap_or(registry);
    let mut command = tokio::process::Command::new(exe);
    command.args(["host", "--serve"]);
    if let Some(n) = name {
        command.args(["--name", n.as_str()]);
    }
    command
        .env("OXUTRM_STATE_DIR", base)
        .stdin(std::process::Stdio::from(theirs.try_clone()?))
        .stdout(std::process::Stdio::from(theirs))
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn()?;
    // `command` held the child's ends; they go with it, so only the child
    // has them now.
    drop(command);
    // Only a reaper: the child exits at once, after forking the session, and
    // waiting for it keeps it from lingering as a zombie. The session itself
    // is the grandchild, which nothing here waits for.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    ours.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(ours)
}

/// Relay an attach exchange between this session's client and another
/// session process (`what`, for the reasons): its first line is read here,
/// so a refusal or another protocol version reaches the client as a reason,
/// then everything is relayed both ways -- decoded and re-encoded, so
/// garbage cannot pass -- until either side ends. Bounded by
/// [`crate::listener::ATTACH_TIMEOUT`], as the exchange itself is.
async fn relay_attach<CR, CW, TR, TW>(
    what: &str,
    client_r: CR,
    client_w: CW,
    target_r: TR,
    target_w: TW,
) where
    CR: AsyncBufRead + Unpin + Send,
    CW: AsyncWrite + Unpin + Send,
    TR: AsyncBufRead + Unpin + Send,
    TW: AsyncWrite + Unpin + Send,
{
    let (mut client_r, mut client_w, mut target_r, mut target_w) =
        (client_r, client_w, target_r, target_w);
    let first = tokio::time::timeout(OPEN_TIMEOUT, read_answer_async(&mut target_r)).await;
    let refusal = match first {
        Ok(Ok(Answer::Signal(hello @ oxutrm_proto::Signal::HostHello { .. }))) => {
            if oxutrm_host::signalling::write_signal_async(&mut client_w, &hello)
                .await
                .is_err()
            {
                return;
            }
            let both = async {
                tokio::select! {
                    _ = oxutrm_host::attach::relay_signals(&mut client_r, &mut target_w) => {}
                    _ = oxutrm_host::attach::relay_signals(&mut target_r, &mut client_w) => {}
                }
            };
            let _ = tokio::time::timeout(crate::listener::ATTACH_TIMEOUT, both).await;
            return;
        }
        Ok(Ok(Answer::Signal(oxutrm_proto::Signal::Failed { reason }))) => reason,
        Ok(Ok(Answer::Reply(Reply::Refused(why)))) => why,
        Ok(Ok(other)) => format!("{what} answered with {other:?}"),
        Ok(Err(ProtoError::Malformed(_) | ProtoError::VersionMismatch { .. })) => {
            format!("{what} runs another version of oxutrm")
        }
        Ok(Err(e)) => format!("{what} did not start: {e}"),
        Err(_) => format!("{what} did not answer in time"),
    };
    let _ = write_line_async(&mut client_w, &Reply::Refused(refusal)).await;
}

/// How long a request forwarded to a sibling may take: a kill waits out
/// the shell's grace and its reaping.
pub(crate) const FORWARD_TIMEOUT: Duration = Duration::from_secs(10);

/// Ask sibling `id` to do `req` -- a `Kill` or a `Rename` -- and bring back
/// its one reply. Every failure is a refusal with a reason for the user.
async fn forward(registry: &std::path::Path, id: SessionId, req: Request) -> Reply {
    let live = Registry::list_in(registry).unwrap_or_default();
    let Some(m) = live.iter().find(|m| m.session_id == id.to_string()) else {
        return Reply::Refused(format!("no session {} on this host", id.short()));
    };
    if !m.detachable {
        return Reply::Refused(format!(
            "session {} has no socket to ask: it dies with its ssh",
            id.short()
        ));
    }
    let path = Registry::socket_path_in(registry, &m.session_id);
    let asked = async {
        let stream = tokio::net::UnixStream::connect(&path).await?;
        let (r, mut w) = stream.into_split();
        write_line_async(&mut w, &Open::new(req)).await?;
        read_line_async::<_, Reply>(&mut tokio::io::BufReader::new(r)).await
    };
    match tokio::time::timeout(FORWARD_TIMEOUT, asked).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(ProtoError::Malformed(_) | ProtoError::VersionMismatch { .. })) => {
            Reply::Refused(format!(
                "session {} runs another version of oxutrm; end its shell to end it",
                id.short()
            ))
        }
        Ok(Err(e)) => Reply::Refused(format!("session {} did not answer: {e}", id.short())),
        Err(_) => Reply::Refused(format!("session {} did not answer in time", id.short())),
    }
}

/// Serve one connection on one of the doors: read its `Open`, answer it.
pub(crate) async fn serve<R, W>(door: Arc<Door>, via: Via, reader: R, writer: W)
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = reader;
    let mut writer = writer;
    let open: Open = match tokio::time::timeout(OPEN_TIMEOUT, read_line_async(&mut reader)).await {
        Ok(Ok(open)) => open,
        // Silence, a hang-up, or a line that is no `Open`: nothing was
        // asked, so nothing is answered.
        _ => return,
    };
    if open.proto != PROTO_VERSION {
        let _ = write_line_async(
            &mut writer,
            &Reply::Refused(format!(
                "this session runs protocol version {PROTO_VERSION}, the request was \
                 version {}",
                open.proto
            )),
        )
        .await;
        return;
    }
    match open.req {
        Request::Attach { role } => attach(&door, role, reader, writer).await,
        Request::Probe { nonce } => probes(nonce, reader, writer).await,
        Request::Myself => {
            let reply = match door.own_entry(Via::Socket) {
                Some(e) => Reply::Entry(e),
                None => Reply::Refused("this session has no entry".to_string()),
            };
            let _ = write_line_async(&mut writer, &reply).await;
        }
        Request::Sessions => {
            let reply = Reply::Sessions(sessions(&door, via).await);
            let _ = write_line_async(&mut writer, &reply).await;
        }
        Request::New { name } if door.own().lobby => {
            let reply = door.start(name).await;
            let _ = write_line_async(&mut writer, &reply).await;
        }
        Request::Kill { id } if id.to_string() == door.id() => {
            door.kill_self(via, &mut writer).await;
        }
        Request::Kill { id } => {
            let reply = forward(&door.registry, id, Request::Kill { id }).await;
            let _ = write_line_async(&mut writer, &reply).await;
        }
        Request::Rename { id, name } if id.to_string() == door.id() => {
            let reply = door.rename_self(via, name);
            let _ = write_line_async(&mut writer, &reply).await;
        }
        Request::Rename { id, name } => {
            let reply = forward(&door.registry, id, Request::Rename { id, name }).await;
            let _ = write_line_async(&mut writer, &reply).await;
        }
        // Only a session's own client switches or starts a sibling: a
        // socket is a sibling's or `--attach`'s, and neither does.
        Request::Switch { .. } | Request::New { .. } if via == Via::Socket => {
            let _ = write_line_async(
                &mut writer,
                &Reply::Refused("asked through the wrong door".to_string()),
            )
            .await;
        }
        Request::Switch { to } => switch(&door, to, reader, writer).await,
        Request::New { name } => new_sibling(&door, name, reader, writer).await,
    }
}

/// Hand the stream to the serial attach loop, whose exchange's `HostHello`
/// is the reply. A door with no loop behind it, or a loop that has gone,
/// drops the stream: the asker sees it end rather than wait on silence.
async fn attach<R, W>(door: &Door, role: Role, reader: R, writer: W)
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let Some(queue) = door.own().attaches.clone() else {
        return;
    };
    let to = match role {
        Role::Primary => &queue.primary,
        Role::Standby => &queue.standby,
    };
    // A busy loop is waited for: its attempt ahead is bounded by
    // `ATTACH_TIMEOUT`.
    let _ = to
        .send(DoorRequest {
            reader: Box::new(reader),
            writer: Box::new(writer),
            role,
        })
        .await;
}

/// Answer a probe, and every further probe on the same stream, until it
/// ends: one stream can serve a whole outage's probing.
async fn probes<R, W>(nonce: u64, mut reader: R, mut writer: W)
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut nonce = nonce;
    loop {
        if write_line_async(&mut writer, &Reply::ProbeAck { nonce })
            .await
            .is_err()
        {
            return;
        }
        match read_line_async::<_, Open>(&mut reader).await {
            Ok(Open {
                req: Request::Probe { nonce: next },
                ..
            }) => nonce = next,
            _ => return,
        }
    }
}

/// The host's sessions, oldest first: this one from the door's own record,
/// every other live one as it answers `Myself` over its socket -- all asked
/// at once, each within [`SIBLING_TIMEOUT`].
async fn sessions(door: &Door, via: Via) -> Vec<SessionEntry> {
    let own_id = door.own().meta.session_id.clone();
    let registry = door.registry.clone();
    // A blocking read of a directory of small files.
    let live = Registry::list_in(&registry).unwrap_or_default();
    let mut found: Vec<Option<SessionEntry>> = vec![None; live.len()];
    let mut asked = tokio::task::JoinSet::new();
    for (i, m) in live.into_iter().enumerate() {
        if m.session_id == own_id {
            found[i] = door.own_entry(via);
        } else {
            let registry = registry.clone();
            asked.spawn(async move { (i, ask_myself(&registry, &m).await) });
        }
    }
    while let Some(Ok((i, entry))) = asked.join_next().await {
        found[i] = entry;
    }
    found.into_iter().flatten().collect()
}

/// A sibling, as it answers `Myself`: `Unknown` from its `meta.json` when it
/// does not answer in time or cannot be reached; `OtherVersion` when what
/// comes back is not this version's answer -- a refusal, an attach
/// exchange's hello, a line of another protocol.
async fn ask_myself(registry: &std::path::Path, m: &SessionMeta) -> Option<SessionEntry> {
    let offer = m.offer()?;
    let path = Registry::socket_path_in(registry, &m.session_id);
    let asked = async {
        let stream = tokio::net::UnixStream::connect(&path).await?;
        let (r, mut w) = stream.into_split();
        write_line_async(&mut w, &Open::new(Request::Myself)).await?;
        let mut r = tokio::io::BufReader::new(r);
        read_answer_async(&mut r).await
    };
    let attached = match tokio::time::timeout(SIBLING_TIMEOUT, asked).await {
        Ok(Ok(Answer::Reply(Reply::Entry(e)))) if e.id == offer.id => {
            return Some(SessionEntry { this: false, ..e });
        }
        Ok(Ok(_)) => Attached::OtherVersion,
        Ok(Err(ProtoError::Malformed(_) | ProtoError::VersionMismatch { .. })) => {
            Attached::OtherVersion
        }
        Ok(Err(_)) | Err(_) => Attached::Unknown,
    };
    Some(offer.entry(attached, false))
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// A door for a session registered in `registry` as `meta`, with an
    /// attach queue whose inbox comes back for the test to read.
    pub(crate) fn registered(
        registry: &std::path::Path,
        meta: SessionMeta,
    ) -> (Arc<Door>, AttachInbox, Presence) {
        let guard = RegistryGuard::register_in(registry, &meta).expect("register");
        let (queue, inbox) = attach_queue();
        let presence = Presence::default();
        let door = Door::assembled(
            registry.to_path_buf(),
            meta,
            Some(guard),
            presence.clone(),
            Some(queue),
        );
        (door, inbox, presence)
    }

    /// A session record as a real session writes it, with a realistic id,
    /// shell and size, and a live pid so `list_in` keeps it.
    pub(crate) fn meta(id: &str, name: Option<&str>) -> SessionMeta {
        SessionMeta {
            session_id: id.to_string(),
            attach_id: 1,
            pid: std::process::id(),
            created_unix: oxutrm_host::now_unix(),
            shell: "/bin/bash".to_string(),
            size: oxutrm_proto::TermSize {
                cols: 120,
                rows: 40,
            },
            detachable: true,
            boot: None,
            name: name.map(str::to_string),
        }
    }

    /// Accept on `listener` and serve every connection through `door`, as
    /// `listener::accept_doors` does.
    pub(crate) fn socket_door(
        door: Arc<Door>,
        listener: tokio::net::UnixListener,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(crate::listener::accept_doors(listener, door))
    }

    /// Send `req` through the socket at `path` and read the one answer.
    pub(crate) async fn ask(path: &std::path::Path, req: Request) -> Answer {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .expect("connect");
        let (r, mut w) = stream.into_split();
        write_line_async(&mut w, &Open::new(req))
            .await
            .expect("ask");
        let mut r = tokio::io::BufReader::new(r);
        tokio::time::timeout(Duration::from_secs(10), read_answer_async(&mut r))
            .await
            .expect("an answer in time")
            .expect("a readable answer")
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::link::fixtures::link_pair;

    const BUILD: &str = "3ff1218f5e0c4b7d9a1c2e3f40516273";
    const LOGS: &str = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60";
    const GONE: &str = "5d2e8a7c1b9f40e3a6c8d1f2b3e4a5c6";
    const OLD: &str = "77c0ffee77c0ffee77c0ffee77c0ffee";

    /// A sibling registered in `registry`, serving its socket.
    fn sibling(registry: &std::path::Path, id: &str, name: Option<&str>) -> Arc<Door> {
        let (door, _inbox, _presence) = registered(registry, meta(id, name));
        let path = Registry::socket_path_in(registry, id);
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");
        socket_door(Arc::clone(&door), listener);
        door
    }

    #[tokio::test]
    async fn myself_is_this_sessions_entry_as_a_sibling_sees_it() {
        let dir = tempfile::tempdir().unwrap();
        sibling(dir.path(), BUILD, Some("build"));
        let path = Registry::socket_path_in(dir.path(), BUILD);
        match ask(&path, Request::Myself).await {
            Answer::Reply(Reply::Entry(e)) => {
                assert_eq!(e.id.to_string(), BUILD);
                assert_eq!(e.name.unwrap().as_str(), "build");
                assert_eq!(e.shell, "/bin/bash");
                assert_eq!(e.attached, Attached::No, "nobody is attached");
                assert!(!e.this);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn another_version_is_refused_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        sibling(dir.path(), BUILD, None);
        let path = Registry::socket_path_in(dir.path(), BUILD);
        let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (r, mut w) = stream.into_split();
        write_line_async(
            &mut w,
            &Open {
                proto: PROTO_VERSION + 1,
                req: Request::Myself,
            },
        )
        .await
        .unwrap();
        let reply: Reply = read_line_async(&mut tokio::io::BufReader::new(r))
            .await
            .unwrap();
        assert!(
            matches!(&reply, Reply::Refused(why) if why.contains("version")),
            "{reply:?}"
        );
    }

    /// The whole listing: this session (from the client's door), a named
    /// sibling, a sibling that never answers, and one that speaks another
    /// version.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sessions_lists_this_one_its_siblings_and_says_which_did_not_answer() {
        let dir = tempfile::tempdir().unwrap();
        let (me, _inbox, _presence) = registered(dir.path(), meta(BUILD, Some("build")));
        sibling(dir.path(), LOGS, Some("logs"));

        // Registered, its socket bound -- and nobody accepting: connecting
        // works off the backlog, and no answer ever comes.
        let (_gone, _gone_inbox, _) = registered(dir.path(), meta(GONE, None));
        let _deaf =
            tokio::net::UnixListener::bind(Registry::socket_path_in(dir.path(), GONE)).unwrap();

        // An older binary answers every connection with its hello, as the
        // previous protocol did.
        let (_old, _old_inbox, _) = registered(dir.path(), meta(OLD, None));
        let old =
            tokio::net::UnixListener::bind(Registry::socket_path_in(dir.path(), OLD)).unwrap();
        tokio::spawn(async move {
            while let Ok((s, _)) = old.accept().await {
                use tokio::io::AsyncWriteExt as _;
                let mut s = s;
                let hello = format!(
                    concat!(
                        r#"{{"t":"HostHello","proto":2,"session_id":"{}","attach_id":1,"#,
                        r#""cert_spki_sha256":"AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=","#,
                        r#""psk":"AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=","#,
                        r#""candidates":[],"nat_type":"Unknown","bound_port":443,"#,
                        r#""detachable":true}}"#,
                        "\n"
                    ),
                    OLD
                );
                let _ = s.write_all(hello.as_bytes()).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });

        let begun = Instant::now();
        let list = sessions(&me, Via::Client).await;
        assert!(
            begun.elapsed() < SIBLING_TIMEOUT + Duration::from_secs(1),
            "the siblings were asked one after another: {:?}",
            begun.elapsed()
        );
        let by_id = |id: &str| {
            list.iter()
                .find(|e| e.id.to_string() == id)
                .unwrap_or_else(|| panic!("{id} is missing from {list:?}"))
        };
        assert_eq!(list.len(), 4, "{list:?}");
        assert!(by_id(BUILD).this);
        assert_eq!(by_id(BUILD).attached, Attached::Here);
        assert!(!by_id(LOGS).this);
        assert_eq!(by_id(LOGS).attached, Attached::No);
        assert_eq!(by_id(LOGS).name.as_ref().unwrap().as_str(), "logs");
        assert_eq!(by_id(GONE).attached, Attached::Unknown);
        assert_eq!(by_id(OLD).attached, Attached::OtherVersion);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_sibling_with_a_client_is_listed_as_in_use_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let (me, _inbox, _) = registered(dir.path(), meta(BUILD, None));
        let (logs, _logs_inbox, presence) = registered(dir.path(), meta(LOGS, None));
        let listener =
            tokio::net::UnixListener::bind(Registry::socket_path_in(dir.path(), LOGS)).unwrap();
        socket_door(logs, listener);
        // The way the session's loop says it: a link, heard from now.
        let (host_link, _client) = link_pair().await;
        let _session = crate::host_session::HostSession::spawn(
            "/bin/sh",
            &oxutrm_term::Start::default(),
            oxutrm_proto::TermSize {
                cols: 120,
                rows: 40,
            },
            200,
            host_link,
        )
        .unwrap()
        .with_presence(presence);

        let list = sessions(&me, Via::Client).await;
        let logs = list.iter().find(|e| e.id.to_string() == LOGS).unwrap();
        assert_eq!(logs.attached, Attached::Elsewhere);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_attach_goes_to_the_loop_with_its_stream_and_its_role() {
        let dir = tempfile::tempdir().unwrap();
        let (door, mut inbox, _) = registered(dir.path(), meta(BUILD, None));
        let (host, client) = link_pair().await;
        crate::control::serve_control(host.sink.connection().clone(), Arc::clone(&door));

        let (mut send, _recv) = client.sink.connection().open_bi().await.unwrap();
        write_line_async(
            &mut send,
            &Open::new(Request::Attach {
                role: Role::Standby,
            }),
        )
        .await
        .unwrap();
        // What follows the request reaches whoever the stream is handed to:
        // the exchange reads the client's hello from it.
        write_line_async(&mut send, &Open::new(Request::Probe { nonce: 1 }))
            .await
            .unwrap();
        let mut req = tokio::time::timeout(Duration::from_secs(5), inbox.standby.recv())
            .await
            .expect("the loop was asked")
            .expect("the queue is open");
        assert_eq!(req.role, Role::Standby);
        let next: Open = read_line_async(&mut req.reader).await.unwrap();
        assert_eq!(next.req, Request::Probe { nonce: 1 });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_first_line_that_is_no_open_reaches_nothing_and_the_door_stays_open() {
        let dir = tempfile::tempdir().unwrap();
        let (door, mut inbox, _) = registered(dir.path(), meta(BUILD, None));
        let (host, client) = link_pair().await;
        crate::control::serve_control(host.sink.connection().clone(), Arc::clone(&door));

        let (mut send, _recv) = client.sink.connection().open_bi().await.unwrap();
        oxutrm_host::signalling::write_signal_async(
            &mut send,
            &oxutrm_proto::Signal::Failed {
                reason: "stray".into(),
            },
        )
        .await
        .unwrap();
        send.finish().unwrap();
        let knocked = tokio::time::timeout(Duration::from_millis(500), async {
            tokio::select! {
                r = inbox.primary.recv() => r,
                r = inbox.standby.recv() => r,
            }
        })
        .await;
        assert!(knocked.is_err(), "a stray line reached the attach loop");

        assert!(
            crate::control::probe(client.sink.connection().clone(), 9).await,
            "the door stopped serving after a stray stream"
        );
    }
}

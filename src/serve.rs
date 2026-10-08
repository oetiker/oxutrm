//! `oxutrm host --serve`: the remote half, from the fork to the shell.

use oxutrm_proto::TermSize;

use anyhow::Context as _;
use oxutrm_host::registry::{RegistryRoot, SessionMeta};
use oxutrm_net::NetConfig;

use crate::host_session::HostSession;

/// How much scrollback the host keeps. The same number `loopback` uses.
const SCROLLBACK: usize = 10_000;

/// `oxutrm host --serve`: R1 to R3, and then everything else.
///
/// The order of the first three statements is the design, not a style. See
/// [`oxutrm_host::detach_process`]: it must run before a socket, a runtime or
/// a thread exists, because `fork` copies only the calling thread and a
/// runtime built beforehand wakes up in the child with its workers gone.
pub fn run_host_serve() -> anyhow::Result<()> {
    // R1. Nothing has run before this. The parent `_exit(0)`s, so ssh reports
    // the command finished; the channel stays open because the grandchild
    // still holds 0, 1 and 2.
    let detached = oxutrm_host::detach_process().context("detaching from ssh")?;

    // R2. This reaches the user, because stderr is still the ssh pipe — and a
    // user wondering why a session vanished at logout needs this sentence
    // before they need anything else.
    let root = oxutrm_host::resolve_registry_root()
        .context("deciding where oxutrm records its sessions")?;
    if let Some(warning) = &root.warning {
        eprintln!("{warning}");
    }

    // R3. Threads are allowed from here: there will be no further fork.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the runtime")?;
    let outcome = runtime.block_on(serve(detached, &root));

    // Do not WAIT for the read that is parked on ssh's pipe.
    //
    // The signalling channel is descriptor 0, and `tokio::io::stdin` serves it
    // from the blocking pool. A blocking read cannot be cancelled -- aborting
    // the task that owns it does not touch the thread sitting in `read(2)` --
    // and dropping a runtime waits for that pool. So the ordinary end of this
    // function would park the process in `futex_do_wait` while a worker stayed
    // in `anon_pipe_read`, for as long as the client left its end of the pipe
    // open. From the user's side that is an `ssh` that never returns.
    //
    // Measured, not theorised: `tests/serve_exits.rs` reproduces it, and
    // deliberately does NOT close its end of the pipe, because closing it is
    // what hides the failure.
    //
    // Detaching is safe precisely because there is nothing behind that read we
    // still want. The session is over; the registry entry has already been
    // removed by its guard; the only thing outstanding is a message from a
    // client we have stopped listening to.
    //
    // The middle clause is the one this depends on, and it is a claim about
    // code rather than a hope: `serve()` awaits the aborted listener task
    // before it drops the guard, so the last `Arc` really is gone and
    // `RegistryGuard::drop` really has run by the time `serve()` returns. When
    // that await was missing, the guard was still held by a task the runtime
    // had not got round to dropping, `remove_dir_all` did not run, and this
    // paragraph was licensing the process to walk away from a cleanup that had
    // not happened.
    runtime.shutdown_background();
    outcome
}

/// R4 to R16, with the ssh pipes as the signalling channel.
///
/// Everything here runs in the grandchild. Descriptors 0, 1 and 2 are still
/// ssh's until R12, which is why the whole handshake happens before it.
async fn serve(detached: oxutrm_host::Detached, root: &RegistryRoot) -> anyhow::Result<()> {
    let cfg = NetConfig::default();
    let mut meta = SessionMeta {
        session_id: oxutrm_host::new_session_id().context("naming the session")?,
        attach_id: 0,
        pid: std::process::id(),
        created_unix: oxutrm_host::now_unix(),
        shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned()),
        // Replaced at R7 by what the client actually has. Until then the
        // record has to say something, and the terminal default is the least
        // surprising thing for it to say.
        size: TermSize { cols: 80, rows: 24 },
        // R11 settles this from the nominated rung. `false` until then is the
        // safe direction: a record that over-promises reattachment is worse
        // than one that under-promises it for a few hundred milliseconds.
        detachable: false,
        boot: oxutrm_host::boot_token(),
        name: None,
    };

    let attached = crate::attach_exchange::run_attach_exchange(
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        &mut meta,
        &cfg,
    )
    .await?;

    // R11. The outcome, at last, from the rung that was actually nominated.
    let permit = oxutrm_host::settle_detachability(&mut meta, attached.path.rung);

    // R12. `None` is rung 4: its QUIC traffic runs inside the ssh connection,
    // so it keeps its pipes and its ssh for life. Anything else severs, ssh
    // sees EOF, and the user's prompt comes back.
    //
    // The socket, below, follows the same arm: `permit` is consumed here, so
    // it is `meta.detachable` -- the very fact `settle_detachability` just
    // wrote -- that the listener setup reads to make the identical choice.
    let sock = match permit {
        Some(permit) => {
            oxutrm_host::sever_from_ssh(detached, permit).context("severing from ssh")?;
            true
        }
        // Rung 4. The fork already happened and is harmless; what must not
        // happen is the sever. The token simply goes unused, which is the
        // shape the type system gives this case: there is no permit to pass,
        // so there is no call to write.
        None => {
            let oxutrm_host::Detached { .. } = detached;
            false
        }
    };

    // R13. AFTER R12, or the socket path is closed a moment later:
    // `close_inherited_descriptors` closes by enumeration and keeps no list of
    // exceptions, which is the whole of its value.
    let guard =
        oxutrm_host::RegistryGuard::register_in(&oxutrm_host::Registry::dir_at(&root.base), &meta)
            .context("recording the session in the registry")?;

    // `RegistryGuard` becomes an `Arc` here because the listener task and this
    // function both need it, and its `Drop` removes the session directory: the
    // directory must outlive both, which is exactly what an `Arc` says.
    let guard = std::sync::Arc::new(guard);
    let shell = meta.shell.clone();
    let meta = std::sync::Arc::new(tokio::sync::Mutex::new(meta));

    // The socket path has always been computed and registered. Nothing has
    // ever bound it until now -- and only when this session severed from ssh:
    // rung 4 keeps `sock` false, because its QUIC traffic runs inside the ssh
    // connection and a socket bound anyway would offer an attach that cannot
    // outlive it.
    let listening = if sock {
        let path = guard.socket_path();
        oxutrm_host::check_socket_path_length(&path)?;
        let listener = tokio::net::UnixListener::bind(&path)
            .with_context(|| format!("binding the session socket at {}", path.display()))?;
        Some(open_doors(
            listener,
            std::sync::Arc::clone(&guard),
            std::sync::Arc::clone(&meta),
            cfg,
            attached.link.sink.connection().clone(),
        ))
    } else {
        // No socket, so no listener to run a standby's exchange. Such a
        // session (rung 4, whose QUIC runs inside ssh) has nothing a standby
        // could outlive it through, but its hello advertised a control stream
        // all the same, so the link still serves one: probes are answered and
        // a standby request is refused at once rather than left waiting.
        crate::control::serve_control_without_door(attached.link.sink.connection().clone());
        None
    };

    // R14. `negotiate_term` takes no arguments: the child's TERM comes from
    // the emulator, never from the client, or a client narrower than the
    // emulator would bake degraded output into the authoritative screen for
    // the life of the session.
    let mut session = HostSession::spawn(&shell, attached.client_size, SCROLLBACK, attached.link)
        .context("starting the shell")?;

    // R15, then R16: dropping the guard takes the session directory with it,
    // so a session that exits cleanly leaves nothing for `--list` to prune.
    let code = match listening {
        Some((task, mut attach_rx)) => {
            let code = session.run_with_attaches(&mut attach_rx).await;
            // Abort AND await, and why both, is `close_the_door`'s own note.
            crate::listener::close_the_door(task).await;
            code
        }
        None => session.run().await,
    };
    drop(guard);
    code.map(|_| ())
}

/// The two ways into a severed session after its first attach: the Unix
/// socket, and the door that the first link's control stream knocks on.
///
/// Returns the listener task and where completed attaches arrive. A function
/// of its own so the first link's control server, which nothing else starts,
/// can be reached from a test: every later link's is started by the listener
/// as it hands the link over.
pub(crate) fn open_doors(
    listener: tokio::net::UnixListener,
    guard: std::sync::Arc<oxutrm_host::RegistryGuard>,
    meta: std::sync::Arc<tokio::sync::Mutex<SessionMeta>>,
    cfg: NetConfig,
    first: quinn::Connection,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::Receiver<crate::attach_exchange::Attached>,
) {
    let (attach_tx, attach_rx) = tokio::sync::mpsc::channel(1);
    // The door: standby requests from the control streams, served by the
    // same loop as the socket.
    let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
    crate::control::serve_control(first, door_tx.clone());
    let task = tokio::spawn(crate::listener::serve_attaches(
        listener,
        guard,
        meta,
        cfg,
        crate::listener::ATTACH_TIMEOUT,
        door_rx,
        door_tx,
        attach_tx,
    ));
    (task, attach_rx)
}

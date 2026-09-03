//! `oxutrm host --serve`: the remote half, from the fork to the shell.

use oxutrm_proto::TermSize;

use anyhow::Context as _;
use oxutrm_host::registry::{RegistryRoot, SessionMeta};
use oxutrm_net::NetConfig;

use crate::session::HostSession;

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

        let (attach_tx, attach_rx) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(crate::listener::serve_attaches(
            listener,
            std::sync::Arc::clone(&guard),
            std::sync::Arc::clone(&meta),
            cfg,
            attach_tx,
        ));
        Some((task, attach_rx))
    } else {
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
            task.abort();
            code
        }
        None => session.run().await,
    };
    drop(guard);
    code.map(|_| ())
}

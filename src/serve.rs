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
pub fn run_host_serve(begin: Begin) -> anyhow::Result<()> {
    // R1. Nothing has run before this. The parent `_exit(0)`s, so ssh reports
    // the command finished; the channel stays open because the grandchild
    // still holds 0, 1 and 2.
    let detached = oxutrm_host::detach_process().context("detaching from ssh")?;
    // The binary a sibling runs (switcher spec §3.4), resolved once here and
    // passed in: never looked up inside the code that spawns one.
    let exe = own_binary();

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
    let outcome = runtime.block_on(serve(detached, &root, begin, exe));

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
    // code rather than a hope: `run_process` ends on `Door::close`, which
    // awaits the aborted accept and attach loops and then drops the guard
    // itself, so `RegistryGuard::drop` really has run by the time `serve()`
    // returns. When an await like that was missing, the guard was still held
    // by a task the runtime had not got round to dropping, `remove_dir_all`
    // did not run, and this paragraph was licensing the process to walk away
    // from a cleanup that had not happened.
    runtime.shutdown_background();
    outcome
}

/// R4 to R16, with the ssh pipes as the signalling channel.
///
/// Everything here runs in the grandchild. Descriptors 0, 1 and 2 are still
/// ssh's until R12, which is why the whole handshake happens before it.
async fn serve(
    detached: oxutrm_host::Detached,
    root: &RegistryRoot,
    begin: Begin,
    exe: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    let cfg = host_net_config();
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
    // The socket, later, follows the same arm: `permit` is consumed here, so
    // it is `meta.detachable` -- the very fact `settle_detachability` just
    // wrote -- that `Door::register` reads to make the identical choice.
    match permit {
        Some(permit) => {
            oxutrm_host::sever_from_ssh(detached, permit).context("severing from ssh")?;
        }
        // Rung 4. The fork already happened and is harmless; what must not
        // happen is the sever. The token simply goes unused, which is the
        // shape the type system gives this case: there is no permit to pass,
        // so there is no call to write.
        None => {
            let oxutrm_host::Detached { .. } = detached;
        }
    }

    // R13 onwards, AFTER R12, or the socket path is closed a moment later:
    // `close_inherited_descriptors` closes by enumeration and keeps no list
    // of exceptions, which is the whole of its value.
    //
    // A login shell in `$HOME`, as ssh would give you (switcher spec §3.4).
    let shell = Shell {
        program: meta.shell.clone(),
        start: oxutrm_term::Start::login_in(home_dir(std::env::var_os("HOME"))),
        sibling: exe,
    };
    run_process(
        attached.link,
        attached.client_size,
        meta,
        oxutrm_host::Registry::dir_at(&root.base),
        cfg,
        begin,
        shell,
    )
    .await
    .map(|_| ())
}

/// Set in a debug build's environment, it keeps `host --serve` off the
/// network beyond the exchange itself: no STUN servers, no port mapping, no
/// birthday blast. The tests' siblings carry it (`door::spawn_sibling`), so
/// `make test` reaches neither the internet nor the router. A release
/// build ignores it.
pub(crate) const HERMETIC_NET_ENV: &str = "OXUTRM_TEST_HERMETIC_NET";

/// The host's network configuration: the default, unless a debug build is
/// told [`HERMETIC_NET_ENV`].
fn host_net_config() -> NetConfig {
    if cfg!(debug_assertions) && std::env::var_os(HERMETIC_NET_ENV).is_some() {
        return NetConfig {
            stun_servers: vec![],
            enable_port_mapping: false,
            enable_birthday: false,
            ..NetConfig::default()
        };
    }
    NetConfig::default()
}

/// This session's own binary, for a sibling to run: on Linux
/// `/proc/self/exe`, which still opens after the file was replaced by a
/// rebuild; elsewhere the path this process was started from.
fn own_binary() -> Option<std::path::PathBuf> {
    if cfg!(target_os = "linux") {
        Some(std::path::PathBuf::from("/proc/self/exe"))
    } else {
        std::env::current_exe().ok()
    }
}

/// What a session process starts as: a session with its shell, or a lobby
/// (switcher spec §2.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Begin {
    /// Registered as `name`, if given, with its shell started.
    Session {
        name: Option<oxutrm_proto::Name>,
    },
    Lobby,
}

/// What a session process runs: its shell, how the shell starts, and the
/// binary a sibling runs (switcher spec §3.4) -- this process's own,
/// resolved once at startup; `None` where none could be.
pub(crate) struct Shell {
    pub(crate) program: String,
    pub(crate) start: oxutrm_term::Start,
    pub(crate) sibling: Option<std::path::PathBuf>,
}

/// R13 to R16: one session process, from its first link to its end.
///
/// Everything a session process does after its first attach exchange, and
/// the same for every way one starts -- a first connect over ssh, a sibling
/// over a socket pair, a lobby. A function of its own, apart from the fork
/// and the sever, so the whole of it runs in a test.
///
/// `Begin::Session` registers (under its name) and starts the shell;
/// `Begin::Lobby` does neither until it is asked for `New`. Returns the
/// shell's exit status, or `0` for a lobby that ended.
pub(crate) async fn run_process(
    link: crate::link::Link,
    client_size: TermSize,
    mut meta: SessionMeta,
    registry: std::path::PathBuf,
    cfg: NetConfig,
    begin: Begin,
    shell: Shell,
) -> anyhow::Result<i32> {
    let starts_shell = match begin {
        Begin::Session { name } => {
            meta.name = name.map(String::from);
            true
        }
        Begin::Lobby => false,
    };
    let presence = crate::host_session::Presence::default();
    let (cmds_tx, mut cmds) = tokio::sync::mpsc::channel(1);
    let (attached_tx, mut attaches) = tokio::sync::mpsc::channel(1);
    let first = link.sink.connection().clone();
    let door = crate::door::Door::process(
        registry,
        meta,
        presence.clone(),
        cfg,
        crate::door::LoopLink {
            cmds: cmds_tx,
            attached: attached_tx,
        },
        shell.sibling.clone(),
    );
    let mut session =
        HostSession::lobby(&shell.program, &shell.start, client_size, SCROLLBACK, link)?
            .with_presence(presence);
    if starts_shell {
        // R13: registered, and its socket bound where the rung allows one.
        door.register()
            .context("recording the session in the registry")?;
        // R14. `negotiate_term` takes no arguments: the child's TERM comes
        // from the emulator, never from the client, or a client narrower
        // than the emulator would bake degraded output into the
        // authoritative screen for the life of the session.
        if let Err(e) = session.start_shell() {
            door.close().await;
            return Err(e.context("starting the shell"));
        }
    }
    // The first link's control stream: every later link's is started by
    // the attach loop as it hands the link over.
    crate::control::serve_control(first, std::sync::Arc::clone(&door));

    // R15, then R16: the door's close takes the registry entry with it, so a
    // session that exits cleanly leaves nothing for `--list` to prune.
    let code = session.run_with_doors(&mut attaches, &mut cmds).await;
    door.close().await;
    code
}

/// The directory a new shell starts in: `$HOME` when it names a directory,
/// else none -- the inherited one -- rather than a shell that fails to start.
pub(crate) fn home_dir(home: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    home.map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use crate::link::Link;
    use oxutrm_host::signalling::{read_answer_async, write_line_async};
    use oxutrm_proto::{Answer, Open, Request};

    /// The size every test client has.
    pub(crate) const SIZE: TermSize = TermSize {
        cols: 120,
        rows: 40,
    };

    /// A session process run in-process, as `serve` runs one after its
    /// sever: its client's end of the first link, and the process's task.
    pub(crate) struct Process {
        pub(crate) client: Link,
        pub(crate) task: tokio::task::JoinHandle<anyhow::Result<i32>>,
    }

    /// Run a session process with id `id` in `registry`: `Begin::Session`
    /// registers it (as `name`) and starts `program` (a plain `/bin/sh`
    /// unless a test needs a particular shell).
    pub(crate) async fn process(
        registry: &std::path::Path,
        id: &str,
        name: Option<&str>,
        begin: Begin,
        shell: Shell,
    ) -> Process {
        let (host, client) = crate::link::fixtures::link_pair().await;
        // A session's name travels in its `Begin`, as `--new --name` sends it.
        let begin = match begin {
            Begin::Session { .. } => Begin::Session {
                name: name.map(|n| oxutrm_proto::Name::parse(n).expect("a name")),
            },
            Begin::Lobby => Begin::Lobby,
        };
        let mut meta = crate::door::fixtures::meta(id, None);
        meta.size = SIZE;
        let task = tokio::spawn(run_process(
            host,
            SIZE,
            meta,
            registry.to_path_buf(),
            crate::attach_exchange::fixtures::stun_free(),
            begin,
            Shell {
                sibling: Some(built_binary()),
                ..shell
            },
        ));
        Process { client, task }
    }

    /// The `oxutrm` binary `cargo test` built, for a sibling to run.
    ///
    /// `CARGO_BIN_EXE_oxutrm` is set for integration tests only, so it is
    /// found where cargo puts it: `$CARGO_TARGET_DIR`, else `target/` in
    /// the workspace, then `debug/`. `make test` builds it (the integration
    /// tests need it); a lone `cargo test --bin oxutrm` may not.
    pub(crate) fn built_binary() -> std::path::PathBuf {
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
        target.join("debug").join("oxutrm")
    }

    pub(crate) fn sh() -> Shell {
        Shell {
            program: "/bin/sh".to_string(),
            start: oxutrm_term::Start::default(),
            sibling: None,
        }
    }

    /// Ask `req` over a fresh control stream of `conn`, as the client does,
    /// and read the one answer.
    pub(crate) async fn ask_over(conn: &quinn::Connection, req: Request) -> Answer {
        let (mut send, recv) = conn.open_bi().await.expect("a control stream");
        write_line_async(&mut send, &Open::new(req))
            .await
            .expect("asking");
        let mut recv = tokio::io::BufReader::new(recv);
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            read_answer_async(&mut recv),
        )
        .await
        .expect("an answer in time")
        .expect("a readable answer")
    }

    /// Wait until `registry` lists exactly `ids`, in any order.
    pub(crate) async fn registry_holds(registry: &std::path::Path, ids: &[&str]) {
        let want: std::collections::BTreeSet<String> = ids.iter().map(|s| s.to_string()).collect();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let live: std::collections::BTreeSet<String> = oxutrm_host::Registry::list_in(registry)
                .unwrap()
                .into_iter()
                .map(|m| m.session_id)
                .collect();
            if live == want {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the registry holds {live:?}, not {want:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_used_only_when_it_is_an_absolute_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            home_dir(Some(dir.path().as_os_str().to_owned())),
            Some(dir.path().to_path_buf())
        );
        assert_eq!(home_dir(None), None);
        assert_eq!(home_dir(Some("relative/home".into())), None);
        assert_eq!(
            home_dir(Some(dir.path().join("missing").into_os_string())),
            None
        );
    }

    use super::fixtures::*;
    use oxutrm_proto::{Answer, Attached, Name, Reply, Request, SessionId};

    const BUILD: &str = "3ff1218f5e0c4b7d9a1c2e3f40516273";
    const LOGS: &str = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60";

    fn id(s: &str) -> SessionId {
        s.parse().unwrap()
    }

    fn name(s: &str) -> Option<Name> {
        Some(Name::parse(s).unwrap())
    }

    fn entry(a: Answer) -> oxutrm_proto::SessionEntry {
        match a {
            Answer::Reply(Reply::Entry(e)) => e,
            other => panic!("not an entry: {other:?}"),
        }
    }

    fn refused(a: Answer) -> String {
        match a {
            Answer::Reply(Reply::Refused(why)) => why,
            other => panic!("not a refusal: {other:?}"),
        }
    }

    fn done(a: Answer) {
        assert!(matches!(a, Answer::Reply(Reply::Done)), "{a:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn new_in_a_lobby_registers_it_by_name_and_it_becomes_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let p = process(dir.path(), BUILD, None, Begin::Lobby, sh()).await;
        registry_holds(dir.path(), &[]).await;

        let e = entry(
            ask_over(
                p.client.sink.connection(),
                Request::New {
                    name: name("build"),
                },
            )
            .await,
        );
        assert_eq!(e.id, id(BUILD), "the lobby keeps the id it already had");
        assert_eq!(e.name, name("build"));
        assert!(e.this);
        assert_eq!(e.attached, Attached::Here);
        registry_holds(dir.path(), &[BUILD]).await;

        // A session now: its socket answers, and it has a shell to kill.
        let sock = oxutrm_host::Registry::socket_path_in(dir.path(), BUILD);
        let mine = entry(crate::door::fixtures::ask(&sock, Request::Myself).await);
        assert_eq!(mine.name, name("build"));
        done(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn new_in_a_lobby_with_a_taken_name_is_refused_and_it_stays_a_lobby() {
        let dir = tempfile::tempdir().unwrap();
        let _logs = oxutrm_host::RegistryGuard::register_in(
            dir.path(),
            &crate::door::fixtures::meta(LOGS, Some("build")),
        )
        .unwrap();
        let p = process(dir.path(), BUILD, None, Begin::Lobby, sh()).await;
        let why = refused(
            ask_over(
                p.client.sink.connection(),
                Request::New {
                    name: name("build"),
                },
            )
            .await,
        );
        assert!(why.contains("taken"), "{why}");
        registry_holds(dir.path(), &[LOGS]).await;
        // Still a lobby, and still answering: an unnamed New works.
        let e = entry(ask_over(p.client.sink.connection(), Request::New { name: None }).await);
        assert_eq!(e.id, id(BUILD));
        assert_eq!(e.name, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn killing_this_session_leaves_a_lobby_that_can_start_again() {
        let dir = tempfile::tempdir().unwrap();
        let p = process(
            dir.path(),
            BUILD,
            Some("build"),
            Begin::Session { name: None },
            sh(),
        )
        .await;
        registry_holds(dir.path(), &[BUILD]).await;

        done(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
        // Done only after the entry went: no waiting here.
        assert!(
            oxutrm_host::Registry::list_in(dir.path())
                .unwrap()
                .is_empty(),
            "Done came before the registry entry was removed"
        );
        assert!(
            !p.task.is_finished(),
            "a kill from its own client ended the process"
        );
        assert!(p.client.sink.connection().close_reason().is_none());

        // The last session is gone, and the lobby lists nothing.
        match ask_over(p.client.sink.connection(), Request::Sessions).await {
            Answer::Reply(Reply::Sessions(list)) => assert!(list.is_empty(), "{list:?}"),
            other => panic!("{other:?}"),
        }
        let why =
            refused(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
        assert!(why.contains("no shell"), "{why}");

        // And it starts again, as the same id.
        let e = entry(
            ask_over(
                p.client.sink.connection(),
                Request::New {
                    name: name("again"),
                },
            )
            .await,
        );
        assert_eq!(e.id, id(BUILD));
        registry_holds(dir.path(), &[BUILD]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn killing_a_sibling_ends_it_as_a_shell_that_exited() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        // The sibling's shell is a script that says when it runs and then
        // is a plain `sleep`: an interactive `/bin/sh` hung up while it is
        // still starting can outlive the grace (bash 3.2's SIGHUP handler is
        // not async-signal-safe), and then the status is SIGKILL's, not
        // SIGHUP's. In a directory of its own: a socket path under it would
        // add to the registry's.
        let scripts = tempfile::tempdir().unwrap();
        let ready = scripts.path().join("ready");
        let script = scripts.path().join("sleeper");
        std::fs::write(
            &script,
            format!("#!/bin/sh\n: > '{}'\nexec sleep 1000\n", ready.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let me = process(
            dir.path(),
            BUILD,
            Some("build"),
            Begin::Session { name: None },
            sh(),
        )
        .await;
        let sibling = process(
            dir.path(),
            LOGS,
            Some("logs"),
            Begin::Session { name: None },
            Shell {
                program: script.to_str().unwrap().to_string(),
                start: oxutrm_term::Start::default(),
                sibling: None,
            },
        )
        .await;
        registry_holds(dir.path(), &[BUILD, LOGS]).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready.exists() {
            assert!(std::time::Instant::now() < deadline, "the shell never ran");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        done(ask_over(me.client.sink.connection(), Request::Kill { id: id(LOGS) }).await);
        let live = oxutrm_host::Registry::list_in(dir.path()).unwrap();
        assert_eq!(
            live.len(),
            1,
            "Done came before the sibling's entry went: {live:?}"
        );

        // The sibling's own client sees its shell exit, by SIGHUP.
        let code = tokio::time::timeout(std::time::Duration::from_secs(10), sibling.task)
            .await
            .expect("the killed sibling's process did not end")
            .expect("its task")
            .expect("its loop");
        assert_eq!(code, 128 + 1);
        let reason = sibling.client.sink.connection().closed().await;
        assert!(
            matches!(&reason, quinn::ConnectionError::ApplicationClosed(c)
                if c.reason.as_ref() == crate::session::SHELL_EXITED),
            "{reason:?}"
        );
        assert!(!me.task.is_finished(), "the asker was killed too");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_shell_that_ignores_the_hang_up_is_killed_and_then_done() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let script = dir.path().join("stubborn");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntrap '' HUP\n: > '{}'\nwhile :; do sleep 1; done\n",
                ready.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Its own directory: a socket path under the script's would be too
        // long for macOS's 104 bytes.
        let registry_dir = tempfile::tempdir().unwrap();
        let registry = registry_dir.path().to_path_buf();
        let p = process(
            &registry,
            BUILD,
            None,
            Begin::Session { name: None },
            Shell {
                program: script.to_str().unwrap().to_string(),
                start: oxutrm_term::Start::default(),
                sibling: None,
            },
        )
        .await;
        registry_holds(&registry, &[BUILD]).await;
        // The trap is set once the script says so.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready.exists() {
            assert!(std::time::Instant::now() < deadline, "the shell never ran");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let begun = std::time::Instant::now();
        done(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
        assert!(
            begun.elapsed() >= crate::host_session::KILL_GRACE,
            "Done before the grace was out: {:?}",
            begun.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rename_here_and_on_a_sibling_and_a_taken_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let me = process(dir.path(), BUILD, None, Begin::Session { name: None }, sh()).await;
        let _sibling = process(
            dir.path(),
            LOGS,
            Some("logs"),
            Begin::Session { name: None },
            sh(),
        )
        .await;
        registry_holds(dir.path(), &[BUILD, LOGS]).await;
        let conn = me.client.sink.connection();

        let e = entry(
            ask_over(
                conn,
                Request::Rename {
                    id: id(BUILD),
                    name: name("build"),
                },
            )
            .await,
        );
        assert_eq!(e.name, name("build"));
        assert!(e.this);

        let why = refused(
            ask_over(
                conn,
                Request::Rename {
                    id: id(BUILD),
                    name: name("logs"),
                },
            )
            .await,
        );
        assert!(why.contains("taken"), "{why}");

        let e = entry(
            ask_over(
                conn,
                Request::Rename {
                    id: id(LOGS),
                    name: name("tail"),
                },
            )
            .await,
        );
        assert_eq!(e.name, name("tail"));
        assert!(!e.this, "the sibling's entry is the sibling's");

        let e = entry(
            ask_over(
                conn,
                Request::Rename {
                    id: id(BUILD),
                    name: None,
                },
            )
            .await,
        );
        assert_eq!(e.name, None);
        let on_disk = oxutrm_host::Registry::list_in(dir.path()).unwrap();
        let names: Vec<_> = on_disk.iter().map(|m| m.name.clone()).collect();
        assert!(names.contains(&Some("tail".to_string())), "{names:?}");
        assert!(names.contains(&None), "{names:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_request_for_a_session_that_is_not_there_is_refused_with_its_short_id() {
        let dir = tempfile::tempdir().unwrap();
        let me = process(dir.path(), BUILD, None, Begin::Session { name: None }, sh()).await;
        registry_holds(dir.path(), &[BUILD]).await;
        let why =
            refused(ask_over(me.client.sink.connection(), Request::Kill { id: id(LOGS) }).await);
        assert!(why.contains("a3f9c01e"), "{why}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_name_that_is_all_hex_never_reaches_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let me = process(dir.path(), BUILD, None, Begin::Session { name: None }, sh()).await;
        registry_holds(dir.path(), &[BUILD]).await;
        // Written by hand: `Name` cannot hold it, so the line is malformed and
        // the door answers nothing.
        let (mut send, recv) = me.client.sink.connection().open_bi().await.unwrap();
        let line = format!(
            "{{\"proto\":{},\"req\":{{\"q\":\"Rename\",\"id\":\"{BUILD}\",\"name\":\"cafe\"}}}}\n",
            oxutrm_proto::PROTO_VERSION
        );
        send.write_all(line.as_bytes()).await.unwrap();
        let mut recv = tokio::io::BufReader::new(recv);
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            oxutrm_host::signalling::read_answer_async(&mut recv),
        )
        .await
        .expect("the stream must end, not hang");
        assert!(got.is_err(), "an all-hex name was answered: {got:?}");
        let on_disk = oxutrm_host::Registry::list_in(dir.path()).unwrap();
        assert_eq!(on_disk[0].name, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_new_sessions_shell_starts_in_home() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap().join("home");
        std::fs::create_dir(&home).unwrap();
        let out = dir.path().join("out");
        let script = dir.path().join("myshell");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s:%s' \"$0\" \"$(pwd -P)\" > '{}'\nexec sleep 30\n",
                out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let registry_dir = tempfile::tempdir().unwrap();
        let p = process(
            registry_dir.path(),
            BUILD,
            None,
            Begin::Session { name: None },
            Shell {
                program: script.to_str().unwrap().to_string(),
                start: oxutrm_term::Start::login_in(home_dir(Some(home.clone().into_os_string()))),
                sibling: None,
            },
        )
        .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let text = loop {
            if let Ok(t) = std::fs::read_to_string(&out)
                && !t.is_empty()
            {
                break t;
            }
            assert!(std::time::Instant::now() < deadline, "the shell never ran");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        // The directory. The dash in `argv[0]` is proven on the pty itself
        // (`oxutrm-term`'s `a_login_shell_starts_in_its_directory_with_a_dash_name`):
        // a `#!` script's `$0` is its path, whatever `argv[0]` was.
        assert!(text.ends_with(&format!(":{}", home.display())), "{text}");
        done(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
    }

    /// Open a control stream on `conn`, send `req`, and run the client's side
    /// of the attach exchange that follows, as a switch or a new sibling
    /// does.
    async fn attach_by(
        conn: &quinn::Connection,
        req: Request,
    ) -> anyhow::Result<crate::connect::Established> {
        use crate::switcher::{Answered, Ask};
        let ask = match req {
            Request::Switch { to } => Ask::Switch { to },
            Request::New { name } => Ask::New { name },
            other => panic!("{other:?} runs no attach"),
        };
        match crate::switcher::ask(
            conn.clone(),
            ask,
            SIZE,
            &crate::attach_exchange::fixtures::stun_free(),
        )
        .await
        {
            Answered::Landed(e) => Ok(*e),
            Answered::Failed(why) => Err(anyhow::anyhow!(why)),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_switch_runs_the_targets_ordinary_attach_and_leaves_this_session_be() {
        let dir = tempfile::tempdir().unwrap();
        let me = process(
            dir.path(),
            BUILD,
            Some("build"),
            Begin::Session { name: None },
            sh(),
        )
        .await;
        let logs = process(
            dir.path(),
            LOGS,
            Some("logs"),
            Begin::Session { name: None },
            sh(),
        )
        .await;
        registry_holds(dir.path(), &[BUILD, LOGS]).await;

        let landed = attach_by(
            me.client.sink.connection(),
            Request::Switch { to: id(LOGS) },
        )
        .await
        .expect("the switch lands");
        assert_eq!(landed.session_id, LOGS);

        // The target's other client was displaced, as by any primary attach.
        let reason = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            logs.client.sink.connection().closed(),
        )
        .await
        .expect("the target's old client was not displaced");
        assert!(
            matches!(&reason, quinn::ConnectionError::ApplicationClosed(c)
                if c.reason.as_ref() == crate::session::TAKEN_OVER),
            "{reason:?}"
        );
        // This session is untouched: its link is up and it is still listed.
        assert!(me.client.sink.connection().close_reason().is_none());
        assert!(!me.task.is_finished());
        registry_holds(dir.path(), &[BUILD, LOGS]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_switch_to_a_session_that_is_gone_is_refused_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let me = process(dir.path(), BUILD, None, Begin::Session { name: None }, sh()).await;
        registry_holds(dir.path(), &[BUILD]).await;
        let err = attach_by(
            me.client.sink.connection(),
            Request::Switch { to: id(LOGS) },
        )
        .await
        .err()
        .expect("there is nothing to switch to");
        assert!(format!("{err:#}").contains("a3f9c01e"), "{err:#}");
        let err = attach_by(
            me.client.sink.connection(),
            Request::Switch { to: id(BUILD) },
        )
        .await
        .err()
        .expect("a switch to here");
        assert!(format!("{err:#}").contains("you are in"), "{err:#}");
        assert!(me.client.sink.connection().close_reason().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_switch_to_another_version_is_refused_with_that_reason() {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        let dir = tempfile::tempdir().unwrap();
        let me = process(dir.path(), BUILD, None, Begin::Session { name: None }, sh()).await;
        let _old = oxutrm_host::RegistryGuard::register_in(
            dir.path(),
            &crate::door::fixtures::meta(LOGS, None),
        )
        .unwrap();
        let old =
            tokio::net::UnixListener::bind(oxutrm_host::Registry::socket_path_in(dir.path(), LOGS))
                .unwrap();
        tokio::spawn(async move {
            while let Ok((s, _)) = old.accept().await {
                // It reads what it was asked first, as any peer does: one
                // that hung up before the request was written would fail
                // the write, not answer it.
                let mut s = tokio::io::BufReader::new(s);
                let _ = s.read_line(&mut String::new()).await;
                let hello = format!(
                    concat!(
                        r#"{{"t":"HostHello","proto":2,"session_id":"{}","attach_id":1,"#,
                        r#""cert_spki_sha256":"AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=","#,
                        r#""psk":"AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=","#,
                        r#""candidates":[],"nat_type":"Unknown","bound_port":443,"#,
                        r#""detachable":true}}"#,
                        "\n"
                    ),
                    LOGS
                );
                let _ = s.write_all(hello.as_bytes()).await;
            }
        });
        registry_holds(dir.path(), &[BUILD, LOGS]).await;
        let err = attach_by(
            me.client.sink.connection(),
            Request::Switch { to: id(LOGS) },
        )
        .await
        .err()
        .expect("an old session cannot be switched to");
        assert!(format!("{err:#}").contains("another version"), "{err:#}");
    }

    /// Kills whatever sessions are left in `registry` when a test ends, by
    /// pid: a sibling is a real detached process, and one a failed test
    /// leaves behind would run for ever.
    struct Reaper(std::path::PathBuf);

    impl Drop for Reaper {
        fn drop(&mut self) {
            for m in oxutrm_host::Registry::list_in(&self.0).unwrap_or_default() {
                if m.pid != std::process::id()
                    && let Some(pid) = rustix::process::Pid::from_raw(m.pid as i32)
                {
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
                }
            }
        }
    }

    /// The whole of `New` from a session: a real sibling, started as
    /// `oxutrm host --serve --name logs` from the binary cargo built, runs
    /// the ordinary attach exchange over the relayed stream and registers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn new_in_a_session_starts_a_named_sibling_through_the_one_startup_path() {
        // Under /tmp: a socket path under macOS's per-user temp directory
        // is too long once `oxutrm/<id>/sock` is added.
        let base = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        let registry = base.path().join("oxutrm");
        let _reaper = Reaper(registry.clone());
        assert!(
            built_binary().exists(),
            "{} is not built; run the tests through `make test`",
            built_binary().display()
        );
        let me = process(
            &registry,
            BUILD,
            Some("build"),
            Begin::Session { name: None },
            sh(),
        )
        .await;
        registry_holds(&registry, &[BUILD]).await;

        let landed = attach_by(
            me.client.sink.connection(),
            Request::New { name: name("logs") },
        )
        .await
        .expect("the sibling's exchange completes");
        assert_ne!(
            landed.session_id, BUILD,
            "the client landed in a new session"
        );

        let sibling = landed.session_id.clone();
        registry_holds(&registry, &[BUILD, &sibling]).await;
        let live = oxutrm_host::Registry::list_in(&registry).unwrap();
        let entry = live.iter().find(|m| m.session_id == sibling).unwrap();
        assert_eq!(entry.name.as_deref(), Some("logs"));
        assert_ne!(entry.pid, std::process::id(), "a process of its own");

        // A taken name is refused before anything starts.
        let err = attach_by(
            me.client.sink.connection(),
            Request::New { name: name("logs") },
        )
        .await
        .err()
        .expect("logs is taken");
        assert!(format!("{err:#}").contains("taken"), "{err:#}");

        // And it is a session like any other: killed through this one.
        done(
            ask_over(
                me.client.sink.connection(),
                Request::Kill { id: id(&sibling) },
            )
            .await,
        );
        registry_holds(&registry, &[BUILD]).await;
        drop(landed);
    }

    /// The binary a sibling would run is gone -- an upgrade removed it, a
    /// path that no longer resolves: `New` is refused with the reason, and
    /// the session it was asked of carries on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn new_with_no_binary_to_run_is_refused_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (host, client) = crate::link::fixtures::link_pair().await;
        let mut meta = crate::door::fixtures::meta(BUILD, None);
        meta.size = SIZE;
        let task = tokio::spawn(run_process(
            host,
            SIZE,
            meta,
            dir.path().to_path_buf(),
            crate::attach_exchange::fixtures::stun_free(),
            Begin::Session { name: None },
            Shell {
                sibling: Some(std::path::PathBuf::from("/nonexistent/oxutrm")),
                ..sh()
            },
        ));
        registry_holds(dir.path(), &[BUILD]).await;
        let err = attach_by(client.sink.connection(), Request::New { name: None })
            .await
            .err()
            .expect("there is no binary to start");
        assert!(
            format!("{err:#}").contains("starting a new session"),
            "{err:#}"
        );
        assert!(client.sink.connection().close_reason().is_none());
        assert!(!task.is_finished());
        registry_holds(dir.path(), &[BUILD]).await;
    }
}

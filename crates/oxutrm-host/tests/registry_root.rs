//! Where the registry lives, and why it must not be `/run/user/<uid>` by
//! default.
//!
//! systemd destroys the runtime directory when the user's last login session
//! ends. A session that daemonized into it is still running afterwards and
//! completely unreachable: no socket, no `--list` entry, no way back. That is
//! the failure oxutrm exists to prevent, arriving through the back door.

use std::path::PathBuf;

use oxutrm_host::registry::{
    RegistryRootKind, RootEnv, check_socket_path_length, choose_registry_root,
};
use oxutrm_host::{Registry, RegistryGuard, SessionMeta, now_unix};
use oxutrm_proto::TermSize;

fn env(xdg: Option<&str>, home: Option<&str>, linger: Option<bool>) -> RootEnv {
    RootEnv {
        xdg_runtime_dir: xdg.map(PathBuf::from),
        home: home.map(PathBuf::from),
        override_dir: None,
        linger,
        runtime_dirs_exist: true,
    }
}

/// The whole decision table above describes a system that HAS runtime
/// directories. macOS has none, and the difference is not cosmetic: every
/// branch that falls back also explains itself, and on a Mac that explanation
/// would name `XDG_RUNTIME_DIR` and tell the user to run `loginctl`, neither
/// of which exists there. It would be printed on every session.
#[test]
fn a_system_without_runtime_directories_falls_back_silently() {
    let root = choose_registry_root(&RootEnv {
        xdg_runtime_dir: None,
        home: Some(PathBuf::from("/Users/u")),
        override_dir: None,
        linger: None,
        runtime_dirs_exist: false,
    })
    .expect("choose");

    assert_eq!(root.kind, RegistryRootKind::StateDir);
    assert_eq!(root.base, PathBuf::from("/Users/u/.local/state"));
    assert!(
        root.warning.is_none(),
        "there is nothing to warn about and nothing to advise: {:?}",
        root.warning
    );
}

/// And it stays silent even if something set the variable anyway: without a
/// way to ask whether that directory outlives the login, oxutrm cannot trust
/// it — and on a system with no such concept there is no question to answer,
/// so there is nothing to report either.
#[test]
fn a_runtime_directory_set_by_hand_is_ignored_where_the_concept_does_not_exist() {
    let root = choose_registry_root(&RootEnv {
        xdg_runtime_dir: Some(PathBuf::from("/tmp/runtime")),
        home: Some(PathBuf::from("/Users/u")),
        override_dir: None,
        linger: None,
        runtime_dirs_exist: false,
    })
    .expect("choose");

    assert_eq!(root.base, PathBuf::from("/Users/u/.local/state"));
    assert!(root.warning.is_none(), "{:?}", root.warning);
}

// ---------------------------------------------------------------------------
// The decision table
// ---------------------------------------------------------------------------

#[test]
fn the_runtime_directory_is_used_when_lingering_keeps_it_alive() {
    let root = choose_registry_root(&env(Some("/run/user/1000"), Some("/home/u"), Some(true)))
        .expect("choose");
    assert_eq!(root.base, PathBuf::from("/run/user/1000"));
    assert_eq!(root.kind, RegistryRootKind::RuntimeDir);
    assert!(
        root.warning.is_none(),
        "nothing to warn about: {:?}",
        root.warning
    );
}

#[test]
fn without_lingering_the_state_directory_wins_and_says_why() {
    let root = choose_registry_root(&env(Some("/run/user/1000"), Some("/home/u"), Some(false)))
        .expect("choose");
    assert_eq!(root.base, PathBuf::from("/home/u/.local/state"));
    assert_eq!(root.kind, RegistryRootKind::StateDir);
    let warning = root.warning.expect("this case must warn");
    assert!(
        warning.contains("loginctl enable-linger"),
        "the warning must name the fix: {warning}"
    );
    assert!(
        warning.contains("logout") || warning.contains("log out"),
        "the warning must name the danger: {warning}"
    );
    assert!(
        warning.contains("/home/u/.local/state/oxutrm"),
        "the warning must name where things actually went: {warning}"
    );
}

#[test]
fn an_unverifiable_runtime_directory_is_not_trusted() {
    let root =
        choose_registry_root(&env(Some("/run/user/1000"), Some("/home/u"), None)).expect("choose");
    assert_eq!(
        root.kind,
        RegistryRootKind::StateDir,
        "when in doubt, persist"
    );
    assert!(root.warning.is_some());
}

#[test]
fn a_missing_runtime_directory_falls_back_quietly_but_visibly() {
    let root = choose_registry_root(&env(None, Some("/home/u"), None)).expect("choose");
    assert_eq!(root.base, PathBuf::from("/home/u/.local/state"));
    assert!(root.warning.is_some());
}

#[test]
fn an_explicit_override_beats_everything_and_never_warns() {
    let mut e = env(Some("/run/user/1000"), Some("/home/u"), Some(false));
    e.override_dir = Some(PathBuf::from("/srv/oxutrm-state"));
    let root = choose_registry_root(&e).expect("choose");
    assert_eq!(root.base, PathBuf::from("/srv/oxutrm-state"));
    assert!(
        root.warning.is_none(),
        "the user asked for this explicitly: {:?}",
        root.warning
    );
}

#[test]
fn with_neither_a_runtime_directory_nor_a_home_it_fails_with_advice() {
    let err = choose_registry_root(&env(None, None, None)).expect_err("nowhere to put it");
    let text = format!("{err:#}");
    assert!(
        text.contains("OXUTRM_STATE_DIR"),
        "must offer the override: {text}"
    );
}

#[test]
fn a_socket_path_too_long_for_sun_path_is_refused_with_advice() {
    let long = PathBuf::from(format!(
        "/home/{}/.local/state/oxutrm/abc/sock",
        "x".repeat(120)
    ));
    let err = check_socket_path_length(&long).expect_err("108 bytes is the limit");
    let text = format!("{err:#}");
    assert!(
        text.contains("OXUTRM_STATE_DIR"),
        "must offer the override: {text}"
    );
    check_socket_path_length(std::path::Path::new("/run/user/1000/oxutrm/abc/sock"))
        .expect("a normal path is fine");
}

// ---------------------------------------------------------------------------
// The point of all of it
// ---------------------------------------------------------------------------

/// The runtime directory disappearing at logout must not take the session
/// with it.
#[tokio::test]
async fn a_session_stays_discoverable_after_the_runtime_directory_is_destroyed() {
    let tmp = short_tempdir();
    let fake_runtime = tmp.path().join("run-user-1000");
    let fake_home = tmp.path().join("home");
    std::fs::create_dir_all(&fake_runtime).expect("runtime dir");
    std::fs::create_dir_all(&fake_home).expect("home");

    // Lingering is off, so the resolver must choose the state directory.
    let chosen = choose_registry_root(&RootEnv {
        xdg_runtime_dir: Some(fake_runtime.clone()),
        home: Some(fake_home.clone()),
        override_dir: None,
        linger: Some(false),
        runtime_dirs_exist: true,
    })
    .expect("choose");
    assert_eq!(chosen.kind, RegistryRootKind::StateDir);
    assert!(
        !chosen.base.starts_with(&fake_runtime),
        "the registry must not be inside the directory that is about to vanish"
    );

    let root = Registry::dir_at(&chosen.base);
    let meta = SessionMeta {
        session_id: "1234abcd1234abcd1234abcd1234abcd".to_string(),
        attach_id: 1,
        pid: std::process::id(),
        // Must be recent: an entry older than the process holding its pid is
        // stale by the pid-reuse rule.
        created_unix: now_unix(),
        shell: "/bin/bash".to_string(),
        size: TermSize { cols: 80, rows: 24 },
        detachable: true,
    };
    let guard = RegistryGuard::register_in(&root, &meta).expect("register");
    let sock = guard.socket_path();
    check_socket_path_length(&sock).expect("short enough");
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind");

    // Logout: systemd tears the runtime directory down.
    std::fs::remove_dir_all(&fake_runtime).expect("simulate logout");
    assert!(!fake_runtime.exists());

    let listed = Registry::list_in(&root).expect("list");
    assert_eq!(listed.len(), 1, "the session must still be discoverable");
    assert_eq!(listed[0].session_id, meta.session_id);

    let connected = tokio::net::UnixStream::connect(&sock).await;
    assert!(
        connected.is_ok(),
        "the socket must still be reachable: {connected:?}"
    );
    drop(listener);
}

/// A temporary directory short enough to hold a Unix socket path.
///
/// `tempfile::tempdir()` honours `TMPDIR`, which on macOS is
/// `/var/folders/<hash>/T/` - about 50 bytes before the registry appends a
/// 32-character session id and `/sock`, which puts the result past the
/// 100-byte `sun_path` limit. The product already reports that clearly and
/// says to set `OXUTRM_STATE_DIR`; these tests simply need a base that does
/// not provoke it, so they go on testing what they are named after.
fn short_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("oxu-")
        .tempdir_in("/tmp")
        .expect("tempdir under /tmp")
}

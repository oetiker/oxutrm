//! Real directories, real symlinks, real modes. `prepare_root` is about the
//! filesystem, so it cannot be tested against a table.

use oxutrm_host::registry::{
    PrepareError, RegistryRoot, RegistryRootKind, RootEnv, check_socket_path_length, prepare_root,
    registry_root_candidates, walk_candidates,
};
use oxutrm_host::{Registry, RegistryGuard, SessionMeta, now_unix};
use oxutrm_proto::TermSize;

/// A short private directory under `/tmp` rather than `std::env::temp_dir()`:
/// on macOS the latter is `/var/folders/<hash>/T/`, which eats into the
/// 108-byte `sun_path` budget these paths are subject to.
fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("oxu-roots-")
        .tempdir_in("/tmp")
        .unwrap()
}

#[test]
fn an_absent_directory_is_created_private() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = scratch();
    let base = scratch.path().join("oxutrm-1234");
    prepare_root(&base).expect("absent is the ordinary case");
    let md = std::fs::symlink_metadata(&base).unwrap();
    assert!(md.is_dir());
    assert_eq!(md.permissions().mode() & 0o777, 0o700);
}

#[test]
fn a_directory_we_own_is_reused() {
    let scratch = scratch();
    let base = scratch.path().join("oxutrm-1234");
    prepare_root(&base).expect("first call creates");
    prepare_root(&base).expect("second call reuses");
}

#[test]
fn a_directory_we_own_with_loose_bits_is_tightened_rather_than_refused() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = scratch();
    let base = scratch.path().join("oxutrm-1234");
    std::fs::create_dir(&base).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();

    prepare_root(&base).expect("ours, so recoverable");
    let md = std::fs::symlink_metadata(&base).unwrap();
    assert_eq!(
        md.permissions().mode() & 0o777,
        0o700,
        "must be tightened, not left loose"
    );
}

#[test]
fn a_squatting_file_is_occupied_and_says_so() {
    let scratch = scratch();
    let base = scratch.path().join("oxutrm-1234");
    std::fs::write(&base, b"squat").unwrap();

    match prepare_root(&base) {
        Err(PrepareError::Occupied(why)) => {
            assert!(
                why.contains("not a directory"),
                "the message must name the cause: {why}"
            );
            assert!(
                why.contains(&base.display().to_string()),
                "and the path, or the user cannot act on it: {why}"
            );
        }
        other => panic!("a squatted path must be Occupied, got {other:?}"),
    }
}

#[test]
fn a_symlink_is_occupied_even_pointing_at_a_directory_we_own() {
    let scratch = scratch();
    let real = scratch.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let base = scratch.path().join("oxutrm-1234");
    std::os::unix::fs::symlink(&real, &base).unwrap();

    assert!(
        matches!(prepare_root(&base), Err(PrepareError::Occupied(_))),
        "following the link would pass and hand the path to whoever can rewrite it"
    );
}

#[test]
fn a_world_writable_parent_without_the_sticky_bit_is_unavailable() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = scratch();
    let parent = scratch.path().join("loose");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();
    let base = parent.join("oxutrm-1234");

    // Not Occupied: nothing of ours is there. The location itself is unsafe,
    // because without the sticky bit another user could replace our directory
    // between sessions -- so moving to the next candidate is correct.
    assert!(matches!(
        prepare_root(&base),
        Err(PrepareError::Unavailable(_))
    ));
}

#[test]
fn a_missing_parent_is_unavailable_not_occupied() {
    let scratch = scratch();
    let base = scratch.path().join("no-such-parent").join("oxutrm-1234");
    assert!(matches!(
        prepare_root(&base),
        Err(PrepareError::Unavailable(_))
    ));
}

#[test]
fn a_sticky_world_writable_parent_we_own_is_accepted() {
    // The production shape: `/dev/shm` and `/var/tmp` are `1777` and owned by
    // root, but a test can only own what it creates, so this parent is ours
    // instead -- the owner rule accepts either. Without this positive test,
    // every negative parent-permission test above could pass by accident: an
    // inverted sticky-bit condition would make this exact shape `Unavailable`
    // too, and nothing here would catch it.
    use std::os::unix::fs::PermissionsExt;
    let scratch = scratch();
    let parent = scratch.path().join("sticky");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o1777)).unwrap();
    let base = parent.join("oxutrm-1234");

    prepare_root(&base).expect("a sticky, world-writable parent we own is safe");
}

#[test]
fn a_group_writable_parent_without_the_sticky_bit_is_unavailable() {
    // 0770, not 0777: group-writable alone is enough to let a fellow group
    // member rename or delete our directory, the same capability the sticky
    // rule exists to deny. A check that only looked at the world-writable bit
    // would miss this shape entirely.
    use std::os::unix::fs::PermissionsExt;
    let scratch = scratch();
    let parent = scratch.path().join("group-writable");
    std::fs::create_dir(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o770)).unwrap();
    let base = parent.join("oxutrm-1234");

    assert!(matches!(
        prepare_root(&base),
        Err(PrepareError::Unavailable(_))
    ));
}

// ---------------------------------------------------------------------------
// Walking the candidate list
// ---------------------------------------------------------------------------

fn root(base: &std::path::Path, kind: RegistryRootKind) -> RegistryRoot {
    RegistryRoot {
        base: base.to_path_buf(),
        kind,
        warning: None,
    }
}

#[test]
fn an_unavailable_candidate_falls_through_to_the_next() {
    let scratch = scratch();
    let missing = scratch.path().join("no-such-parent").join("oxutrm-1234");
    let good = scratch.path().join("oxutrm-1234");

    let chosen = walk_candidates(vec![
        root(&missing, RegistryRootKind::SharedMemory),
        root(&good, RegistryRootKind::VarTmp),
    ])
    .expect("an absent location is a reason to look elsewhere");
    assert_eq!(chosen.kind, RegistryRootKind::VarTmp);
}

#[test]
fn a_squatted_candidate_stops_the_walk_instead_of_downgrading() {
    // THE test for this change. If Occupied fell through like Unavailable,
    // a local user could squat both local candidates with two mkdirs and
    // silently force every session back onto the networked home directory.
    let scratch = scratch();
    let squatted = scratch.path().join("oxutrm-1234");
    std::fs::write(&squatted, b"squat").unwrap();
    let home = scratch.path().join("home-state");

    let err = walk_candidates(vec![
        root(&squatted, RegistryRootKind::SharedMemory),
        root(&home, RegistryRootKind::HomeState),
    ])
    .expect_err("a squatted candidate must stop the walk");

    let msg = err.to_string();
    assert!(
        msg.contains("not a directory"),
        "must say what is wrong: {msg}"
    );
    assert!(
        msg.contains("OXUTRM_STATE_DIR"),
        "must say how to recover: {msg}"
    );
    assert!(
        !home.exists(),
        "the walk must not have reached, let alone created, the next candidate"
    );
}

#[test]
fn an_explicit_choice_never_falls_through() {
    let scratch = scratch();
    let squatted = scratch.path().join("chosen");
    std::fs::write(&squatted, b"squat").unwrap();

    assert!(
        walk_candidates(vec![root(&squatted, RegistryRootKind::Explicit)]).is_err(),
        "the user's own choice is never second-guessed"
    );
}

#[test]
fn a_walk_with_nothing_usable_says_so() {
    let scratch = scratch();
    let missing = scratch.path().join("no-such-parent").join("oxutrm-1234");
    let err =
        walk_candidates(vec![root(&missing, RegistryRootKind::VarTmp)]).expect_err("none usable");
    assert!(
        err.to_string().contains("nowhere to record sessions"),
        "the empty case needs its own message: {err}"
    );
}

// ---------------------------------------------------------------------------
// Moved from the now-deleted tests/registry_root.rs: everything else there
// tested `choose_registry_root`'s two-way decision table, which is gone.
// These two survive because they test what is still here: the socket-length
// guard, and the regression this whole subsystem exists to prevent.
// ---------------------------------------------------------------------------

#[test]
fn a_socket_path_too_long_for_sun_path_is_refused_with_advice() {
    let long = std::path::PathBuf::from(format!(
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

/// The runtime directory disappearing at logout must not take the session
/// with it. This is the regression guard for the bug this whole subsystem
/// exists to prevent.
#[tokio::test]
async fn a_session_stays_discoverable_after_the_runtime_directory_is_destroyed() {
    let tmp = scratch();
    let fake_runtime = tmp.path().join("run-user-1000");
    let fake_home = tmp.path().join("home");
    std::fs::create_dir_all(&fake_runtime).expect("runtime dir");
    std::fs::create_dir_all(&fake_home).expect("home");

    // Lingering is off, so no candidate the walk would try may lie under the
    // runtime directory that is about to vanish.
    let env = RootEnv {
        xdg_runtime_dir: Some(fake_runtime.clone()),
        home: Some(fake_home.clone()),
        override_dir: None,
        linger: Some(false),
        runtime_dirs_exist: true,
        // Not the production shape under test here; `false` is honest.
        shm_exists: false,
    };
    let candidates = registry_root_candidates(&env);
    assert!(
        candidates
            .iter()
            .all(|c| !c.base.starts_with(&fake_runtime)),
        "no candidate may lie under the directory that is about to vanish: {candidates:?}"
    );

    // Exercise the register/bind/list half against a scratch directory, never
    // the real /var/tmp or /dev/shm.
    let root = Registry::dir_at(&tmp.path().join("state"));
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
        boot: None,
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

//! The registry: what a session records about itself, and how a stale record
//! is told from a live one.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use oxutrm_host::{
    META_FILE, REGISTRY_SUBDIR, Registry, RegistryGuard, SessionMeta, entry_is_stale, now_unix,
    pid_alive, process_start_unix,
};
use oxutrm_proto::TermSize;

fn meta(id: &str, pid: u32) -> SessionMeta {
    SessionMeta {
        session_id: id.to_string(),
        attach_id: 1,
        pid,
        created_unix: now_unix(),
        shell: "/bin/bash".to_string(),
        size: TermSize { cols: 80, rows: 24 },
        detachable: true,
    }
}

fn mode_of(path: &Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o7777
}

/// A pid that is certainly gone: spawned, waited on, reaped.
///
/// `/bin/sh -c :` and not `/bin/true`, which is `/usr/bin/true` on macOS.
/// POSIX places the shell at `/bin/sh` on every system we build for, and the
/// test only needs something that exits at once.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", ":"])
        .spawn()
        .expect("spawn /bin/sh");
    let pid = child.id();
    child.wait().expect("wait");
    pid
}

/// Plant an entry without going through `RegistryGuard`, so a test can choose
/// the pid and the creation time and skip `Drop`.
fn plant(root: &Path, id: &str, pid: u32, created: u64) -> std::path::PathBuf {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).expect("create entry dir");
    let m = SessionMeta {
        session_id: id.to_string(),
        attach_id: 1,
        pid,
        created_unix: created,
        shell: "/bin/bash".to_string(),
        size: TermSize { cols: 80, rows: 24 },
        detachable: true,
    };
    std::fs::write(dir.join(META_FILE), serde_json::to_vec(&m).unwrap()).expect("write meta");
    std::fs::write(dir.join("sock"), b"").expect("write sock");
    dir
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

#[test]
fn dir_at_appends_the_oxutrm_subdirectory() {
    let base = Path::new("/run/user/1000");
    assert_eq!(
        Registry::dir_at(base),
        Path::new("/run/user/1000").join(REGISTRY_SUBDIR)
    );
}

#[test]
fn socket_path_in_is_dir_id_sock() {
    let dir = Path::new("/run/user/1000/oxutrm");
    assert_eq!(
        Registry::socket_path_in(dir, "deadbeef"),
        Path::new("/run/user/1000/oxutrm/deadbeef/sock")
    );
}

// ---------------------------------------------------------------------------
// Liveness
// ---------------------------------------------------------------------------

#[test]
fn our_own_pid_is_alive_and_pid_zero_is_not() {
    assert!(pid_alive(std::process::id()));
    assert!(
        !pid_alive(0),
        "pid 0 means the whole process group to kill(2)"
    );
}

#[test]
fn a_reaped_child_is_not_alive() {
    let pid = dead_pid();
    assert!(
        !pid_alive(pid),
        "pid {pid} was reaped and must read as dead"
    );
}

#[test]
fn process_start_unix_answers_for_a_living_process() {
    let start = process_start_unix(std::process::id()).expect("/proc must answer for us");
    let now = now_unix();
    assert!(
        start <= now + 1,
        "we cannot have started in the future: {start} > {now}"
    );
    assert!(
        !entry_is_stale(&meta(
            "aaaabbbbaaaabbbbaaaabbbbaaaabbbb",
            std::process::id()
        )),
        "an entry created now by this very process is not stale"
    );
}

#[test]
fn session_meta_round_trips_through_json() {
    let m = meta("00112233445566778899aabbccddeeff", 4242);
    let text = serde_json::to_string(&m).expect("encode");
    let back: SessionMeta = serde_json::from_str(&text).expect("decode");
    assert_eq!(back.session_id, m.session_id);
    assert_eq!(back.attach_id, m.attach_id);
    assert_eq!(back.pid, m.pid);
    assert_eq!(back.shell, m.shell);
    assert_eq!(back.size, m.size);
    assert_eq!(back.detachable, m.detachable);
}

// ---------------------------------------------------------------------------
// Registration, permissions, removal
// ---------------------------------------------------------------------------

#[test]
fn registering_creates_a_private_directory_and_a_private_meta_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let m = meta("aaaabbbbccccddddeeeeffff00001111", std::process::id());

    let guard = RegistryGuard::register_in(&root, &m).expect("register");

    assert_eq!(mode_of(&root), 0o700, "registry root must be 0700");
    assert_eq!(
        mode_of(guard.dir()),
        0o700,
        "session directory must be 0700"
    );
    assert_eq!(mode_of(&guard.meta_path()), 0o600, "meta.json must be 0600");
    assert_eq!(guard.dir(), root.join(&m.session_id));
    assert_eq!(guard.socket_path(), root.join(&m.session_id).join("sock"));

    let text = std::fs::read_to_string(guard.meta_path()).expect("read meta");
    let back: SessionMeta = serde_json::from_str(&text).expect("decode meta");
    assert_eq!(back.session_id, m.session_id);
    assert_eq!(back.pid, m.pid);
}

#[test]
fn a_loose_umask_does_not_loosen_the_bits() {
    // The bits are set explicitly after creation, so they hold whatever the
    // process umask happens to be. 0o022 would otherwise leave 0o755/0o644.
    let previous = unsafe { libc::umask(0o022) };
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let m = meta("11112222333344445555666677778888", std::process::id());
    let guard = RegistryGuard::register_in(&root, &m).expect("register");

    let dir_mode = mode_of(guard.dir());
    let meta_mode = mode_of(&guard.meta_path());
    let root_mode = mode_of(&root);
    unsafe { libc::umask(previous) };

    assert_eq!(root_mode & 0o077, 0, "no group or other bits on the root");
    assert_eq!(
        dir_mode & 0o077,
        0,
        "no group or other bits on the session dir"
    );
    assert_eq!(meta_mode & 0o077, 0, "no group or other bits on meta.json");
}

#[test]
fn dropping_the_guard_removes_the_session_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let m = meta("99998888777766665555444433332222", std::process::id());

    let dir = {
        let guard = RegistryGuard::register_in(&root, &m).expect("register");
        let dir = guard.dir().to_path_buf();
        // A socket left behind by a live session must not stop removal.
        std::fs::write(guard.socket_path(), b"").expect("touch sock");
        assert!(dir.exists());
        dir
    };
    assert!(!dir.exists(), "Drop must remove {}", dir.display());
    assert!(root.exists(), "the registry root itself stays");
}

#[test]
fn registering_the_same_id_twice_fails_rather_than_stealing_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let m = meta("0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f", std::process::id());
    let _first = RegistryGuard::register_in(&root, &m).expect("first register");
    assert!(
        RegistryGuard::register_in(&root, &m).is_err(),
        "a second live registration of one id must fail; taking it over would \
         delete the first session's socket on drop"
    );
}

#[test]
fn update_rewrites_the_pid_and_keeps_the_bits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let mut m = meta("abcdabcdabcdabcdabcdabcdabcdabcd", 1);
    let guard = RegistryGuard::register_in(&root, &m).expect("register");
    m.pid = std::process::id();
    guard.update(&m).expect("update");

    let text = std::fs::read_to_string(guard.meta_path()).expect("read");
    let back: SessionMeta = serde_json::from_str(&text).expect("decode");
    assert_eq!(back.pid, std::process::id());
    assert_eq!(
        mode_of(&guard.meta_path()),
        0o600,
        "still 0600 after rewrite"
    );
}

// ---------------------------------------------------------------------------
// Listing and pruning
// ---------------------------------------------------------------------------

#[test]
fn list_returns_live_sessions_oldest_first() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    std::fs::create_dir_all(&root).expect("root");
    plant(
        &root,
        "22222222222222222222222222222222",
        std::process::id(),
        now_unix() - 1,
    );
    plant(
        &root,
        "11111111111111111111111111111111",
        std::process::id(),
        now_unix() - 2,
    );

    let listed = Registry::list_in(&root).expect("list");
    let ids: Vec<&str> = listed.iter().map(|m| m.session_id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "11111111111111111111111111111111",
            "22222222222222222222222222222222"
        ],
        "oldest first"
    );
}

#[test]
fn list_prunes_entries_whose_process_is_gone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    std::fs::create_dir_all(&root).expect("root");
    let live = plant(
        &root,
        "aaaa1111aaaa1111aaaa1111aaaa1111",
        std::process::id(),
        now_unix(),
    );
    let dead = plant(
        &root,
        "bbbb2222bbbb2222bbbb2222bbbb2222",
        dead_pid(),
        now_unix(),
    );

    let listed = Registry::list_in(&root).expect("list");

    assert_eq!(
        listed.len(),
        1,
        "only the live session survives: {listed:?}"
    );
    assert_eq!(listed[0].session_id, "aaaa1111aaaa1111aaaa1111aaaa1111");
    assert!(live.exists(), "the live entry stays on disk");
    assert!(!dead.exists(), "the dead entry is removed from disk");
}

/// The reboot case. The `$HOME` fallback is a real filesystem, so the pid in a
/// long-dead entry is very likely to have been handed to something unrelated.
#[test]
fn an_entry_whose_pid_now_belongs_to_another_process_is_stale() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    std::fs::create_dir_all(&root).expect("root");
    // Our own pid, recorded as created in 1970: whatever wrote this entry, it
    // was not this process.
    let reused = plant(
        &root,
        "eeee5555eeee5555eeee5555eeee5555",
        std::process::id(),
        1,
    );
    let live = plant(
        &root,
        "ffff6666ffff6666ffff6666ffff6666",
        std::process::id(),
        now_unix(),
    );

    let listed = Registry::list_in(&root).expect("list");

    assert_eq!(
        listed.len(),
        1,
        "a recycled pid is not a live session: {listed:?}"
    );
    assert_eq!(listed[0].session_id, "ffff6666ffff6666ffff6666ffff6666");
    assert!(
        !reused.exists(),
        "the stale entry and its socket must be removed"
    );
    assert!(!reused.join("sock").exists());
    assert!(live.exists());
}

#[test]
fn list_of_a_missing_registry_is_empty_not_an_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path()).join("never-created");
    assert!(Registry::list_in(&root).expect("list").is_empty());
}

#[test]
fn a_directory_without_meta_is_ignored_and_kept() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let half = root.join("cccc3333cccc3333cccc3333cccc3333");
    std::fs::create_dir_all(&half).expect("create");
    assert!(Registry::list_in(&root).expect("list").is_empty());
    assert!(
        half.exists(),
        "a half-built entry belongs to a session that is still registering"
    );
}

#[test]
fn a_corrupt_meta_is_ignored_and_kept() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let bad = root.join("dddd4444dddd4444dddd4444dddd4444");
    std::fs::create_dir_all(&bad).expect("create");
    std::fs::write(bad.join(META_FILE), b"not json at all").expect("write");
    assert!(Registry::list_in(&root).expect("list").is_empty());
    assert!(bad.exists());
}

// ---- session identifiers ---------------------------------------------------
//
// The identifier is a DIRECTORY NAME under the registry root, chosen by this
// process and joined onto a path. That is what makes its alphabet a safety
// property rather than a formatting preference: anything outside `[0-9a-f]`
// would put path syntax into a filename, and `..` is spelled entirely in
// characters a laxer alphabet would allow.

#[test]
fn a_session_identifier_is_thirty_two_lowercase_hex_characters() {
    let id = oxutrm_host::registry::new_session_id().expect("the system CSPRNG");
    assert_eq!(id.len(), 32, "wrong length: {id:?}");
    assert!(
        id.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "a session id must not be able to spell a path component: {id:?}"
    );
}

/// Two sessions started in the same second must not collide, and an
/// identifier anyone can guess is a socket path anyone can guess.
#[test]
fn two_session_identifiers_are_not_the_same() {
    let a = oxutrm_host::registry::new_session_id().expect("the system CSPRNG");
    let b = oxutrm_host::registry::new_session_id().expect("the system CSPRNG");
    assert_ne!(a, b, "session identifiers are not being drawn at random");
}

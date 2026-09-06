# Registry root and boot identity — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `oxutrm host --attach` work without `loginctl enable-linger`, by moving the registry (and therefore its socket) off the networked home directory onto local, boot-cleared storage, and by giving each entry a boot identity so stale and foreign entries are recognised definitively.

**Architecture:** `choose_registry_root`'s two-way choice becomes an ordered candidate list (`registry_root_candidates`, pure) plus a per-candidate safety check (`prepare_root`, impure), walked by `resolve_registry_root`. `/dev/shm/oxutrm-<uid>` becomes the normal Linux location; `/var/tmp/oxutrm-<uid>` is the portable fallback; `$HOME/.local/state` survives only as a last resort. A squatted directory is a terminal error, never a silent fall-through. `SessionMeta` gains `boot: Option<String>`, and `entry_is_stale` treats a token mismatch as definitive without consulting the pid.

**Tech Stack:** Rust, edition 2024, MSRV 1.96. `rustix` for process checks, `libc` for the two scoped FFI calls, `serde` for `meta.json`, `tempfile` (dev) for filesystem tests.

**Spec:** `docs/superpowers/specs/2026-09-06-registry-root-and-boot-identity-design.md` — read it before Task 1. The plan argues from the spec; where they disagree, the spec wins.

## Global Constraints

Copied verbatim from the standing record. Every task's requirements include these.

- **`oxutrm-host` MUST NOT depend on `oxutrm-net`.**
- **`oxutrm-host/src/lib.rs` is `#![deny(unsafe_code)]`.** Exceptions are `daemon.rs` and **scoped** `#[allow(unsafe_code)]` on individual FFI functions in `registry.rs`. This plan takes that list from one such function to two, which the user approved on 2026-09-06. Each one carries a `SAFETY:` comment and the standing preamble; a module-wide allowance is forbidden.
- **`src/main.rs` is `#![forbid(unsafe_code)]`** — `forbid` cannot be locally overridden, so nothing in this plan may put `unsafe` there.
- **`meta.json` never holds key material.** `crates/oxutrm-host/tests/no_keys_on_disk.rs` enforces it by grepping every byte under the registry. A boot token is not key material, but the new field must not break that test.
- **Never a bare `cargo test`** — the repo root is both a package and a workspace, so it silently runs almost nothing. `make check` is the gate: `cargo fmt --all -- --check`, then `cargo clippy --workspace --all-targets -- -D warnings`, then the tests. It caps parallelism at 4; use it.
- **A changelog entry is part of the work** (`CHANGES.md`, under `## Unreleased`).
- **Do not add `#[allow(dead_code)]` for something not wired up yet.** If a helper is test-only, use `#[cfg(test)]`.
- English for all comments, identifiers and documentation.
- **Do not assert platform rules in tests.** A test must not hardcode "on Linux this is `/dev/shm`" as a *rule*; test the decision table through `RootEnv`, which is why that struct exists.

## Test discipline — read before writing any test

This project's two signature defects, both of which have recurred:

1. **A guard that cannot fail.** **Every task below ends with an injection step: break the thing deliberately, watch the new test fail, put it back.** A test that has not been seen to fail is not a guard.
2. **A comment that outlives its truth.**

Two specific shapes that have bitten this codebase:

- **When a test asserts a return value, ask what else produces that value.** Assert the side effect.
- **When a test asserts on a string, check what the string said BEFORE the change.** Three tests on the previous branch passed against the pre-existing message.

## File Structure

| File | Responsibility |
|---|---|
| `crates/oxutrm-host/src/registry.rs` | Modified throughout. Boot token, `SessionMeta.boot`, staleness, the candidate list, `prepare_root`, the walk. This file already owns all of it; no split is warranted. |
| `crates/oxutrm-host/tests/registry_roots.rs` | **New.** Real-filesystem tests for `prepare_root` and the walk — they need actual directories, symlinks and modes, which do not belong in a unit-test module. |
| `CHANGES.md` | The user-facing entry. |

---

### Task 1: Boot identity, and one correction

**Files:**
- Modify: `crates/oxutrm-host/src/registry.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub fn boot_token() -> Option<String>` — the token for the *current* boot. `None` when the platform cannot answer. Task 2 consumes this.

- [ ] **Step 1: Write the failing tests**

Add to `registry.rs`'s existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn a_boot_token_is_available_on_this_platform() {
    // Both supported platforms can answer. A `None` here means the reader
    // below is broken, not that the platform is exotic -- oxutrm-host is
    // cfg'd for linux and macos only.
    let token = boot_token().expect("this platform must supply a boot token");
    assert!(!token.is_empty(), "an empty token would compare equal to itself forever");
    assert!(!token.contains(char::is_whitespace), "token must be trimmed: {token:?}");
}

#[test]
fn the_boot_token_is_stable_within_one_boot() {
    // The whole design rests on this: two reads during one boot must agree,
    // or every entry would look foreign to the next `--list`.
    assert_eq!(boot_token(), boot_token());
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --lib -- boot_token --test-threads 4`
Expected: FAIL to compile — `cannot find function 'boot_token' in this scope`.

- [ ] **Step 3: Implement the Linux reader**

Add to `registry.rs`, near `process_start_unix`:

```rust
/// A token identifying the boot this session belongs to.
///
/// The only contract is that it differs across boots and, where the platform
/// allows, across machines. `None` when the platform cannot answer, which
/// costs the cross-boot staleness check in [`entry_is_stale`] and nothing
/// else.
///
/// There is no portable way to ask this, so there is one implementation per
/// system and they agree only on the return type.
///
/// # Linux
///
/// `/proc/sys/kernel/random/boot_id` is a UUID the kernel regenerates at every
/// boot, and it is random rather than derived, so it also differs between two
/// machines that booted at the same instant. That is what lets a registry on
/// shared storage tell another host's entries from its own.
#[cfg(target_os = "linux")]
#[must_use]
pub fn boot_token() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_owned())
}
```

- [ ] **Step 4: Implement the macOS reader**

The second scoped FFI exception. Follow the existing preamble style exactly.

```rust
/// # macOS
///
/// `kern.boottime` is the wall-clock time the kernel started, to the
/// microsecond, and `libc` types it as a `timeval` — so unlike the
/// `proc_pidinfo` call above there is no kernel struct to declare here.
///
/// **It carries no machine identity.** Two Macs that booted in the same
/// microsecond and shared a registry directory would collide. Accepted
/// deliberately: shared home directories are rare on macOS, and Linux — where
/// they are not — has a random per-boot UUID that is exact. Recorded as a
/// known limit in the design note rather than engineered around.
///
/// Asking for pid 1's start time would have reused the FFI exception above
/// instead of adding one, and does not work: `proc_pidinfo` refuses for
/// root-owned `launchd`, verified by experiment on 2026-09-06.
#[cfg(target_os = "macos")]
// The rest of this crate is `deny(unsafe_code)` and stays that way. This is
// one FFI call with a scalar return, kept to the smallest scope that can hold
// it rather than a module-wide allowance.
#[allow(unsafe_code)]
#[must_use]
pub fn boot_token() -> Option<String> {
    let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let want = std::mem::size_of::<libc::timeval>();
    let mut len = want;

    // SAFETY: `tv` is an owned local of exactly the type this sysctl reports
    // and of exactly the size passed in `len`, and it is not aliased. The
    // name is a NUL-terminated C string literal. Passing a null new-value
    // pointer with a zero length is the documented way to read without
    // writing.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            std::ptr::from_mut(&mut tv).cast::<libc::c_void>(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };

    if rc != 0 || len != want {
        return None;
    }
    // Zero-padded microseconds so the string orders the same way the instant
    // does, and so two tokens never differ only by formatting.
    Some(format!("{}.{:06}", tv.tv_sec, tv.tv_usec))
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --lib -- boot_token --test-threads 4`
Expected: PASS, 2 tests.

- [ ] **Step 6: Injection — prove the stability test is a guard**

Temporarily make the reader return a fresh value each call. On Linux, replace the body's last line with `Some(format!("{trimmed}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()))`; on macOS, append the same suffix.

Run the same command. Expected: `the_boot_token_is_stable_within_one_boot` FAILS. Restore the real body and re-run: PASS. Paste both outputs into the report.

- [ ] **Step 7: Correct the comment that outlived its truth**

`process_start_unix`'s macOS doc currently claims:

> **Compile-verified against `aarch64-apple-darwin`, never run** — nobody on the project has a Mac yet.

Both halves are false: the suite was run on a Mac on 2026-09-04, and this task's own tests exercise the same file there. Replace that paragraph with what is now true — that it runs on macOS in CI and locally, and that its `None` path still costs only the pid-reuse guard. Do not overstate: say it is exercised by the suite, not that every branch is covered.

- [ ] **Step 8: Run the full gate**

Run: `make check`
Expected: exit 0, zero warnings. Record the passed/failed/ignored counts.

- [ ] **Step 9: Commit**

```bash
git add crates/oxutrm-host/src/registry.rs
git commit -m "feat(registry): a boot token, and one comment that had outlived its truth"
```

---

### Task 2: `SessionMeta.boot`, and staleness that uses it

**Files:**
- Modify: `crates/oxutrm-host/src/registry.rs`

**Interfaces:**
- Consumes: `boot_token()` from Task 1.
- Produces: `SessionMeta.boot: Option<String>`, and an `entry_is_stale` that short-circuits on a token mismatch. Nothing later in this plan depends on it.

- [ ] **Step 1: Write the failing tests**

The first test is the one that matters, and its shape is deliberate: it uses **this process's own live pid**, so if the boot check were removed the entry would look *alive* and the test would fail. That is what proves the pid was never consulted.

```rust
fn meta_for_staleness(pid: u32, boot: Option<&str>) -> SessionMeta {
    SessionMeta {
        session_id: "0123456789abcdef0123456789abcdef".to_owned(),
        attach_id: 1,
        pid,
        created_unix: now_unix(),
        shell: "/bin/sh".to_owned(),
        size: TermSize { cols: 80, rows: 24 },
        detachable: true,
        boot: boot.map(str::to_owned),
    }
}

#[test]
fn an_entry_from_another_boot_is_stale_even_though_its_pid_is_alive() {
    // Our own pid, so every pre-existing liveness check says "alive". Only
    // the boot comparison can make this stale -- delete that check and this
    // test fails rather than passing for the wrong reason.
    let meta = meta_for_staleness(std::process::id(), Some("not-this-boot"));
    assert!(entry_is_stale(&meta));
}

#[test]
fn an_entry_from_this_boot_still_follows_the_pid_rules() {
    let live = meta_for_staleness(std::process::id(), boot_token().as_deref());
    assert!(!entry_is_stale(&live), "our own live pid, this boot: not stale");
}

#[test]
fn an_entry_with_no_boot_token_follows_the_old_rules_unchanged() {
    // Written by a version before this field existed. It must not become
    // stale merely for lacking a token, or upgrading would strand sessions.
    let live = meta_for_staleness(std::process::id(), None);
    assert!(!entry_is_stale(&live));
}

#[test]
fn meta_json_written_before_this_field_existed_still_parses() {
    let old = r#"{"session_id":"0123456789abcdef0123456789abcdef","attach_id":1,
        "pid":1,"created_unix":1,"shell":"/bin/sh",
        "size":{"cols":80,"rows":24},"detachable":true}"#;
    let meta: SessionMeta = serde_json::from_str(old).expect("old meta.json must still parse");
    assert_eq!(meta.boot, None);
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --lib -- staleness boot_token meta_json --test-threads 4`
Expected: FAIL to compile — `SessionMeta` has no field `boot`.

- [ ] **Step 3: Add the field**

In `SessionMeta`, after `detachable`:

```rust
    /// Which boot this session belongs to — see [`boot_token`].
    ///
    /// `Option` for two reasons that both matter: a platform may not be able
    /// to answer, and an entry written before this field existed must keep
    /// parsing. `serde(default)` is what makes the second true; without it an
    /// upgrade would strand every running session behind a parse error.
    #[serde(default)]
    pub boot: Option<String>,
```

Fix every construction site the compiler now rejects, including the one in `Registry`'s own tests and any fixture helper. Real sites set `boot: boot_token()`.

- [ ] **Step 4: Teach `entry_is_stale` the rule**

Insert **before** the existing `pid_alive` check, so a foreign entry never reaches a pid comparison that would be meaningless for it:

```rust
pub fn entry_is_stale(meta: &SessionMeta) -> bool {
    // A different boot means a different process table: this pid refers to
    // another boot, or another machine sharing the registry directory. Decide
    // here and do not look at the pid at all -- on a shared registry it names
    // an unrelated live process on this host, which would read as "alive".
    //
    // Only when BOTH sides have a token: a `None` on either side means "cannot
    // tell", and the rules below are the answer for that case.
    if let (Some(entry), Some(current)) = (meta.boot.as_deref(), boot_token().as_deref())
        && entry != current
    {
        return true;
    }

    if !pid_alive(meta.pid) {
        return true;
    }
    match process_start_unix(meta.pid) {
        // Started well after the entry was written: the pid was recycled.
        Some(start) => start > meta.created_unix.saturating_add(PID_REUSE_SLACK_SECS),
        // The pid exists but the system will not say when it started. Keep
        // it: deleting a live session's socket is much worse than listing a
        // dead one.
        None => false,
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --lib -- staleness boot_token meta_json --test-threads 4`
Expected: PASS.

- [ ] **Step 6: Injection — prove the guard**

Delete the whole `if let (Some(entry), Some(current)) = …` block. Re-run.
Expected: `an_entry_from_another_boot_is_stale_even_though_its_pid_is_alive` FAILS, because the entry's pid is our own and reads as alive. Restore and re-run: PASS. Paste both.

- [ ] **Step 7: Confirm the no-keys test still holds**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --test no_keys_on_disk -- --test-threads 4`
Expected: PASS. The boot token is not key material, but this test greps every byte under the registry and a new field is exactly the kind of change that trips it.

- [ ] **Step 8: Run the full gate**

Run: `make check`
Expected: exit 0, zero warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/oxutrm-host/src/registry.rs
git commit -m "feat(registry): an entry from another boot is stale, whatever its pid says"
```

---

### Task 3: `prepare_root` — the safety check

This is the security-relevant task. A mistake here is a vulnerability, not a wrong path.

**Files:**
- Modify: `crates/oxutrm-host/src/registry.rs`
- Create: `crates/oxutrm-host/tests/registry_roots.rs`

**Interfaces:**
- Consumes: `create_private_dir` / `set_private_mode`, already in the file.
- Produces:
  - `pub enum DirVerdict { Absent, Ours, OursWrongMode, NotOurs(String) }`
  - `pub fn dir_verdict(md: &std::fs::Metadata, our_uid: u32) -> DirVerdict` — **pure**, takes metadata already obtained *without* following symlinks.
  - `pub enum PrepareError { Unavailable(String), Occupied(String), Io(std::io::Error) }`
  - `pub fn prepare_root(base: &std::path::Path) -> Result<(), PrepareError>`
  - Task 5 consumes `prepare_root` and matches on `PrepareError`.

- [ ] **Step 1: Write the failing pure-predicate tests**

The ownership rules are a pure function of metadata, so they get exhaustive unit tests. A directory owned by *another* uid cannot be created without privileges, which is exactly why this predicate takes `our_uid` as a parameter — pass a uid that is not ours and the foreign case becomes testable everywhere.

In `registry.rs`'s test module:

```rust
#[test]
fn a_directory_we_own_at_0700_is_ours() {
    let dir = tempfile::Builder::new().prefix("oxu-").tempdir_in("/tmp").unwrap();
    set_private_mode(dir.path(), 0o700).unwrap();
    let md = std::fs::symlink_metadata(dir.path()).unwrap();
    let uid = rustix::process::getuid().as_raw();
    assert!(matches!(dir_verdict(&md, uid), DirVerdict::Ours));
}

#[test]
fn a_directory_we_own_with_extra_bits_is_recoverable_not_refused() {
    // Ours, so a wrong mode is a crash or an older version, not a trust
    // decision about somebody else's directory. Recovering beats refusing.
    let dir = tempfile::Builder::new().prefix("oxu-").tempdir_in("/tmp").unwrap();
    set_private_mode(dir.path(), 0o755).unwrap();
    let md = std::fs::symlink_metadata(dir.path()).unwrap();
    let uid = rustix::process::getuid().as_raw();
    assert!(matches!(dir_verdict(&md, uid), DirVerdict::OursWrongMode));
}

#[test]
fn a_directory_owned_by_someone_else_is_not_ours() {
    let dir = tempfile::Builder::new().prefix("oxu-").tempdir_in("/tmp").unwrap();
    set_private_mode(dir.path(), 0o700).unwrap();
    let md = std::fs::symlink_metadata(dir.path()).unwrap();
    let not_us = rustix::process::getuid().as_raw().wrapping_add(1);
    assert!(matches!(dir_verdict(&md, not_us), DirVerdict::NotOurs(_)));
}

#[test]
fn a_plain_file_is_not_ours_however_it_is_owned() {
    let dir = tempfile::Builder::new().prefix("oxu-").tempdir_in("/tmp").unwrap();
    let file = dir.path().join("squat");
    std::fs::write(&file, b"").unwrap();
    let md = std::fs::symlink_metadata(&file).unwrap();
    let uid = rustix::process::getuid().as_raw();
    assert!(matches!(dir_verdict(&md, uid), DirVerdict::NotOurs(_)));
}

#[test]
fn a_symlink_is_not_ours_even_when_it_points_at_a_directory_we_own() {
    // The case that catches an lstat-versus-stat mistake. Following the link
    // would report a directory we own and pass -- while handing control of the
    // path to whoever can rewrite the link.
    let dir = tempfile::Builder::new().prefix("oxu-").tempdir_in("/tmp").unwrap();
    let target = dir.path().join("real");
    std::fs::create_dir(&target).unwrap();
    set_private_mode(&target, 0o700).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let md = std::fs::symlink_metadata(&link).unwrap();
    let uid = rustix::process::getuid().as_raw();
    assert!(matches!(dir_verdict(&md, uid), DirVerdict::NotOurs(_)));
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --lib -- dir_verdict directory symlink plain_file --test-threads 4`
Expected: FAIL to compile — `dir_verdict` and `DirVerdict` are not defined.

- [ ] **Step 3: Implement the pure predicate**

```rust
/// What an existing path at a candidate registry root turns out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum DirVerdict {
    /// Nothing there. Create it.
    Absent,
    /// A directory, ours, mode `0700`. Reuse it.
    Ours,
    /// A directory, ours, wrong mode. `chmod` and reuse: we own it, so this is
    /// recovery from a crash or an older version.
    OursWrongMode,
    /// Anything else. Never used, never repaired. The string says why, for a
    /// message a user has to act on.
    NotOurs(String),
}

/// Judge a path from metadata obtained **without following symlinks**.
///
/// Pure, and takes `our_uid` rather than calling `getuid` itself, so the
/// foreign-owner case is testable without privileges — a directory owned by
/// another user cannot be created by a test.
#[must_use]
pub fn dir_verdict(md: &std::fs::Metadata, our_uid: u32) -> DirVerdict {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    if md.file_type().is_symlink() {
        return DirVerdict::NotOurs("it is a symbolic link".to_owned());
    }
    if !md.is_dir() {
        return DirVerdict::NotOurs("it is not a directory".to_owned());
    }
    if md.uid() != our_uid {
        return DirVerdict::NotOurs(format!("it is owned by uid {}, not {our_uid}", md.uid()));
    }
    // Exactly 0700, not merely "no group or other bits". A directory at 0600
    // has no bits we object to and still cannot be traversed, so testing only
    // `& 0o077` would accept a root nothing could be created inside.
    if md.permissions().mode() & 0o777 != 0o700 {
        return DirVerdict::OursWrongMode;
    }
    DirVerdict::Ours
}
```

- [ ] **Step 4: Run the predicate tests to verify they pass**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --lib -- dir_verdict directory symlink plain_file --test-threads 4`
Expected: PASS, 5 tests.

- [ ] **Step 5: Write the failing filesystem tests for `prepare_root`**

Create `crates/oxutrm-host/tests/registry_roots.rs`:

```rust
//! Real directories, real symlinks, real modes. `prepare_root` is about the
//! filesystem, so it cannot be tested against a table.

use oxutrm_host::registry::{PrepareError, prepare_root};

/// A short private directory under `/tmp` rather than `std::env::temp_dir()`:
/// on macOS the latter is `/var/folders/<hash>/T/`, which eats into the
/// 108-byte `sun_path` budget these paths are subject to.
fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("oxu-roots-").tempdir_in("/tmp").unwrap()
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
    assert_eq!(md.permissions().mode() & 0o777, 0o700, "must be tightened, not left loose");
}

#[test]
fn a_squatting_file_is_occupied_and_says_so() {
    let scratch = scratch();
    let base = scratch.path().join("oxutrm-1234");
    std::fs::write(&base, b"squat").unwrap();

    match prepare_root(&base) {
        Err(PrepareError::Occupied(why)) => {
            assert!(why.contains("not a directory"), "the message must name the cause: {why}");
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
    assert!(matches!(prepare_root(&base), Err(PrepareError::Unavailable(_))));
}

#[test]
fn a_missing_parent_is_unavailable_not_occupied() {
    let scratch = scratch();
    let base = scratch.path().join("no-such-parent").join("oxutrm-1234");
    assert!(matches!(prepare_root(&base), Err(PrepareError::Unavailable(_))));
}
```

- [ ] **Step 6: Run them to verify they fail**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --test registry_roots -- --test-threads 4`
Expected: FAIL to compile — `prepare_root` and `PrepareError` are not defined.

- [ ] **Step 7: Implement `prepare_root`**

```rust
/// Why a candidate registry root could not be used.
#[derive(Debug)]
pub enum PrepareError {
    /// This location does not exist on this system, or is not a safe place to
    /// put anything. Nothing of ours is there. **Try the next candidate.**
    Unavailable(String),
    /// The path exists and is not safely ours. **Do not try the next
    /// candidate** — falling through is a downgrade a squatter can force.
    Occupied(String),
    /// An ordinary I/O failure: a full or read-only filesystem. Try the next.
    Io(std::io::Error),
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(why) | Self::Occupied(why) => f.write_str(why),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

/// Create a registry root, or prove an existing one is safe to reuse.
///
/// The parent of a candidate is often world-writable (`/dev/shm` and
/// `/var/tmp` are both `1777`), so this function is the whole of the
/// protection. Two rules carry it:
///
/// **The parent's sticky bit is load-bearing.** `1777` lets only an entry's
/// owner delete or rename it, which is what stops another user replacing our
/// directory once it exists. A world-writable parent *without* `S_ISVTX`
/// offers no such protection, so that location is refused outright.
///
/// **An occupied path is terminal, not a reason to look elsewhere.** See
/// `resolve_registry_root`.
pub fn prepare_root(base: &Path) -> Result<(), PrepareError> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let parent = base
        .parent()
        .ok_or_else(|| PrepareError::Unavailable(format!("{} has no parent", base.display())))?;

    let parent_md = std::fs::metadata(parent).map_err(|e| {
        PrepareError::Unavailable(format!("{}: {e}", parent.display()))
    })?;
    if !parent_md.is_dir() {
        return Err(PrepareError::Unavailable(format!(
            "{} is not a directory",
            parent.display()
        )));
    }
    let parent_mode = parent_md.permissions().mode();
    // World-writable without the sticky bit: anyone could replace our
    // directory between sessions, so nothing we put here can be trusted later.
    if parent_mode & 0o002 != 0 && parent_mode & 0o1000 == 0 {
        return Err(PrepareError::Unavailable(format!(
            "{} is world-writable without the sticky bit, so a directory created \
             there could be replaced by another user",
            parent.display()
        )));
    }

    let our_uid = rustix::process::getuid().as_raw();
    let verdict = match std::fs::symlink_metadata(base) {
        Ok(md) => dir_verdict(&md, our_uid),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DirVerdict::Absent,
        Err(e) => return Err(PrepareError::Io(e)),
    };

    match verdict {
        DirVerdict::Absent => {
            std::fs::create_dir(base).map_err(PrepareError::Io)?;
            set_private_mode(base, 0o700).map_err(|e| {
                PrepareError::Io(std::io::Error::other(e.to_string()))
            })
        }
        DirVerdict::Ours => Ok(()),
        DirVerdict::OursWrongMode => set_private_mode(base, 0o700).map_err(|e| {
            PrepareError::Io(std::io::Error::other(e.to_string()))
        }),
        DirVerdict::NotOurs(why) => Err(PrepareError::Occupied(format!(
            "{} cannot be used because {why}. Remove or rename it, or set \
             OXUTRM_STATE_DIR to a directory you control.",
            base.display()
        ))),
    }
}
```

- [ ] **Step 8: Run the filesystem tests to verify they pass**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --test registry_roots -- --test-threads 4`
Expected: PASS, 7 tests.

- [ ] **Step 9: Injection — three faults, three failures**

Run the same command after each, restoring between:

1. Change `symlink_metadata` to `metadata` (i.e. follow symlinks).
   Expected: `a_symlink_is_occupied_even_pointing_at_a_directory_we_own` FAILS.
2. Delete the sticky-bit block.
   Expected: `a_world_writable_parent_without_the_sticky_bit_is_unavailable` FAILS.
3. Change the `NotOurs` arm to `Ok(())`.
   Expected: `a_squatting_file_is_occupied_and_says_so` FAILS.

Paste each RED and the restored GREEN. **A check that has not been seen to fail is not protecting anything**, and this is the task where that matters most.

- [ ] **Step 10: Run the full gate**

Run: `make check`
Expected: exit 0, zero warnings.

- [ ] **Step 11: Commit**

```bash
git add crates/oxutrm-host/src/registry.rs crates/oxutrm-host/tests/registry_roots.rs
git commit -m "feat(registry): prove a registry root is ours before using it"
```

---

### Task 4: The candidate list

**Files:**
- Modify: `crates/oxutrm-host/src/registry.rs`

**Interfaces:**
- Consumes: `RootEnv` (existing), extended here.
- Produces:
  - `RootEnv.shm_exists: bool`
  - `RegistryRootKind` variants `Explicit`, `RuntimeDir`, `SharedMemory`, `VarTmp`, `HomeState`
  - `pub fn registry_root_candidates(env: &RootEnv) -> Vec<RegistryRoot>` — **pure**, ordered best-first. Task 5 walks it.

- [ ] **Step 1: Write the failing tests**

Test through `RootEnv`, never through `cfg` — that is what the struct exists for, and "do not assert platform rules in tests" is a standing constraint.

```rust
fn env_linux() -> RootEnv {
    RootEnv {
        xdg_runtime_dir: Some(PathBuf::from("/run/user/1000")),
        home: Some(PathBuf::from("/home/u")),
        override_dir: None,
        linger: Some(false),
        runtime_dirs_exist: true,
        shm_exists: true,
    }
}

fn kinds(env: &RootEnv) -> Vec<RegistryRootKind> {
    registry_root_candidates(env).iter().map(|r| r.kind).collect()
}

#[test]
fn an_explicit_state_dir_is_the_only_candidate() {
    // Never second-guessed, and nothing to fall through to: if the user's own
    // choice is unusable they must be told, not quietly overridden.
    let mut env = env_linux();
    env.override_dir = Some(PathBuf::from("/somewhere/chosen"));
    let got = registry_root_candidates(&env);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].kind, RegistryRootKind::Explicit);
    assert_eq!(got[0].base, PathBuf::from("/somewhere/chosen"));
}

#[test]
fn without_lingering_shared_memory_comes_first() {
    // The whole point of the change: no `loginctl enable-linger` required.
    assert_eq!(
        kinds(&env_linux()),
        vec![
            RegistryRootKind::SharedMemory,
            RegistryRootKind::VarTmp,
            RegistryRootKind::HomeState
        ]
    );
}

#[test]
fn with_lingering_the_runtime_dir_still_wins() {
    // It is tmpfs, boot-cleared AND 0700 by construction rather than 1777.
    let mut env = env_linux();
    env.linger = Some(true);
    assert_eq!(kinds(&env)[0], RegistryRootKind::RuntimeDir);
}

#[test]
fn an_undeterminable_linger_does_not_win_the_runtime_dir() {
    // `None` means "could not tell", and an unverifiable runtime directory is
    // exactly what the old code refused everywhere else.
    let mut env = env_linux();
    env.linger = None;
    assert_eq!(kinds(&env)[0], RegistryRootKind::SharedMemory);
}

#[test]
fn without_shared_memory_the_portable_fallback_leads() {
    // The macOS shape: no runtime directories, no /dev/shm.
    let mut env = env_linux();
    env.shm_exists = false;
    env.runtime_dirs_exist = false;
    env.xdg_runtime_dir = None;
    env.linger = None;
    assert_eq!(kinds(&env), vec![RegistryRootKind::VarTmp, RegistryRootKind::HomeState]);
}

#[test]
fn home_state_is_last_and_carries_the_warning_no_other_candidate_does() {
    let got = registry_root_candidates(&env_linux());
    let home = got.last().unwrap();
    assert_eq!(home.kind, RegistryRootKind::HomeState);
    let warning = home.warning.as_deref().expect("the last resort must explain itself");
    assert!(warning.contains("networked"), "it must name the actual risk: {warning}");
    assert!(
        got[..got.len() - 1].iter().all(|r| r.warning.is_none()),
        "a local candidate has nothing to warn about"
    );
}

#[test]
fn with_no_home_the_list_simply_ends_early() {
    let mut env = env_linux();
    env.home = None;
    assert_eq!(kinds(&env), vec![RegistryRootKind::SharedMemory, RegistryRootKind::VarTmp]);
}

#[test]
fn the_per_uid_directories_are_named_for_this_user() {
    let got = registry_root_candidates(&env_linux());
    let uid = rustix::process::getuid().as_raw();
    assert_eq!(got[0].base, PathBuf::from(format!("/dev/shm/oxutrm-{uid}")));
    assert_eq!(got[1].base, PathBuf::from(format!("/var/tmp/oxutrm-{uid}")));
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --lib -- candidates lingering shared_memory home_state per_uid --test-threads 4`
Expected: FAIL to compile — `shm_exists` and `registry_root_candidates` do not exist.

- [ ] **Step 3: Extend `RootEnv` and `RegistryRootKind`**

```rust
/// Where the registry lives, and why.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegistryRootKind {
    /// `$OXUTRM_STATE_DIR`: the user said so. Never second-guessed.
    Explicit,
    /// `$XDG_RUNTIME_DIR`, which is known to survive logout here.
    RuntimeDir,
    /// `/dev/shm/oxutrm-<uid>`: tmpfs, cleared at boot, carries no ageing
    /// rule. The normal case on Linux without lingering.
    SharedMemory,
    /// `/var/tmp/oxutrm-<uid>`: local and logout-surviving everywhere, but it
    /// outlives a reboot, so staleness detection is load-bearing here.
    VarTmp,
    /// `$HOME/.local/state`: last resort. May be a network filesystem, where
    /// a Unix socket is unreliable, so it warns.
    HomeState,
}
```

Add to `RootEnv`:

```rust
    /// Whether this system has `/dev/shm` at all.
    ///
    /// `false` on macOS. A field rather than a `cfg` in
    /// [`registry_root_candidates`], so the whole decision table stays
    /// testable from either platform — the same reasoning as
    /// `runtime_dirs_exist` above.
    pub shm_exists: bool,
```

Populate it in `read_root_env`:

```rust
        shm_exists: cfg!(target_os = "linux") && Path::new("/dev/shm").is_dir(),
```

`RootEnv` derives `Default`, so any construction site using `..Default::default()`
keeps compiling and gets `false`. Sites that list every field will not — fix each
one the compiler names, and give it the value that site actually means rather
than `false` by reflex.

- [ ] **Step 4: Implement the candidate list**

```rust
/// The ordered places this environment could keep its registry, best first.
///
/// Pure: it decides *preference*, not *safety*. Whether a candidate is
/// actually usable is a filesystem question and belongs to [`prepare_root`].
/// Keeping them apart is what lets this table be exhaustively tested from
/// either platform.
#[must_use]
pub fn registry_root_candidates(env: &RootEnv) -> Vec<RegistryRoot> {
    // An explicit choice is the whole list: falling through would silently
    // put sessions somewhere the user did not ask for.
    if let Some(dir) = &env.override_dir {
        return vec![RegistryRoot {
            base: dir.clone(),
            kind: RegistryRootKind::Explicit,
            warning: None,
        }];
    }

    let uid = rustix::process::getuid().as_raw();
    let mut out = Vec::new();

    // Ahead of /dev/shm on purpose: it is tmpfs and boot-cleared like shm, and
    // additionally 0700 by construction rather than 1777, so there is no
    // squatting race to lose in the first place.
    if env.runtime_dirs_exist
        && let (Some(dir), Some(true)) = (&env.xdg_runtime_dir, env.linger)
    {
        out.push(RegistryRoot {
            base: dir.clone(),
            kind: RegistryRootKind::RuntimeDir,
            warning: None,
        });
    }

    if env.shm_exists {
        out.push(RegistryRoot {
            base: PathBuf::from(format!("/dev/shm/oxutrm-{uid}")),
            kind: RegistryRootKind::SharedMemory,
            warning: None,
        });
    }

    out.push(RegistryRoot {
        base: PathBuf::from(format!("/var/tmp/oxutrm-{uid}")),
        kind: RegistryRootKind::VarTmp,
        warning: None,
    });

    if let Some(home) = &env.home {
        out.push(RegistryRoot {
            base: state_base(home),
            kind: RegistryRootKind::HomeState,
            warning: Some(format!(
                "oxutrm: no local directory was usable, so sessions are recorded in {} \
                 instead. Sessions will survive, but on a networked home directory the \
                 session socket may be unreliable. Set OXUTRM_STATE_DIR to choose the \
                 location yourself.",
                state_base(home).join(REGISTRY_SUBDIR).display()
            )),
        });
    }

    out
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --lib -- candidates lingering shared_memory home_state per_uid --test-threads 4`
Expected: PASS, 8 tests.

- [ ] **Step 6: Injection — prove the ordering tests are guards**

Swap the `/dev/shm` and `/var/tmp` pushes. Re-run.
Expected: `without_lingering_shared_memory_comes_first` and `the_per_uid_directories_are_named_for_this_user` FAIL. Restore, re-run: PASS. Paste both.

- [ ] **Step 7: Run the full gate**

Run: `make check`
Expected: exit 0, zero warnings. `choose_registry_root` still exists at this point and is still called by `resolve_registry_root`; Task 5 removes it.

- [ ] **Step 8: Commit**

```bash
git add crates/oxutrm-host/src/registry.rs
git commit -m "feat(registry): rank the places a registry could live"
```

---

### Task 5: Walk the candidates, and refuse to be pushed downhill

**Files:**
- Modify: `crates/oxutrm-host/src/registry.rs`
- Modify: `crates/oxutrm-host/tests/registry_roots.rs`
- Modify: `CHANGES.md`

**Interfaces:**
- Consumes: `registry_root_candidates` (Task 4), `prepare_root` (Task 3).
- Produces: `resolve_registry_root` unchanged in signature — `anyhow::Result<RegistryRoot>` — so no caller changes. `choose_registry_root` is deleted.

- [ ] **Step 1: Write the failing tests**

Add to `crates/oxutrm-host/tests/registry_roots.rs`. These drive the walk through a seam rather than the real `/dev/shm`, so they are deterministic on any machine:

```rust
use oxutrm_host::registry::{RegistryRoot, RegistryRootKind, walk_candidates};

fn root(base: &std::path::Path, kind: RegistryRootKind) -> RegistryRoot {
    RegistryRoot { base: base.to_path_buf(), kind, warning: None }
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
    assert!(msg.contains("not a directory"), "must say what is wrong: {msg}");
    assert!(msg.contains("OXUTRM_STATE_DIR"), "must say how to recover: {msg}");
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
    let err = walk_candidates(vec![root(&missing, RegistryRootKind::VarTmp)]).expect_err("none usable");
    assert!(
        err.to_string().contains("nowhere to record sessions"),
        "the empty case needs its own message: {err}"
    );
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --test registry_roots -- --test-threads 4`
Expected: FAIL to compile — `walk_candidates` is not defined.

- [ ] **Step 3: Implement the walk**

```rust
/// Take the first candidate that is usable.
///
/// `Unavailable` and `Io` mean "nothing of ours is here" and move on.
/// **`Occupied` stops the walk.** That asymmetry is the whole point: falling
/// through on an occupied path would let any local user squat both local
/// candidates and silently push every session onto the last resort, which may
/// be a networked home directory. Two `mkdir`s, repeatable, invisible. A
/// squat is reported and acted on by a human, never routed around.
pub fn walk_candidates(candidates: Vec<RegistryRoot>) -> anyhow::Result<RegistryRoot> {
    let mut reasons = Vec::new();
    for root in candidates {
        match prepare_root(&root.base) {
            Ok(()) => return Ok(root),
            Err(PrepareError::Occupied(why)) => return Err(anyhow!("{why}")),
            Err(e) => reasons.push(format!("{}: {e}", root.base.display())),
        }
    }
    Err(anyhow!(
        "nowhere to record sessions. Tried:\n  {}\nSet OXUTRM_STATE_DIR to a \
         directory that survives logout.",
        reasons.join("\n  ")
    ))
}

pub fn resolve_registry_root() -> anyhow::Result<RegistryRoot> {
    walk_candidates(registry_root_candidates(&read_root_env()))
}
```

Then **delete `choose_registry_root`** and its now-dead tests, and delete the old `fallback` closure. Its decision table is superseded by `registry_root_candidates`; leaving both would be two sources of truth for one policy.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --test registry_roots -- --test-threads 4`
Expected: PASS, 11 tests.

- [ ] **Step 5: Injection — the one that matters**

Change the `Occupied` arm to push onto `reasons` like the others (i.e. fall through). Re-run.
Expected: `a_squatted_candidate_stops_the_walk_instead_of_downgrading` FAILS — the walk reaches and creates the `HomeState` candidate. Restore, re-run: PASS. Paste both. **This injection is the proof that the downgrade attack is closed**; without it the whole task rests on inspection.

- [ ] **Step 6: Verify the real resolution end to end**

Run: `cargo run --release -- host --list`
Expected on Linux: sessions listed with no lingering warning, and `ls -ld /dev/shm/oxutrm-$(id -u)` shows a `0700` directory owned by you. On macOS: `/var/tmp/oxutrm-$(id -u)`.

This is the step that catches a decision table that is right in tests and wrong in `read_root_env`. Record the actual path used.

- [ ] **Step 7: Write the changelog entry**

Under `## Unreleased` → `### Fixed` in `CHANGES.md`:

```markdown
- **Reattaching no longer needs `loginctl enable-linger`.** A detached session
  outlives your login on most systems, but the directory it registered itself
  in — `/run/user/<uid>` — does not, so oxutrm used to record sessions in your
  home directory instead. On a networked home that put the session's Unix
  socket on a filesystem where Unix sockets are not reliable, and on a shared
  home it meant `--list` could show sessions belonging to another machine and
  test their process ids against the wrong computer. Sessions are now recorded
  on local storage — `/dev/shm` on Linux, `/var/tmp` elsewhere — which survives
  logout without any administrative setup, and each entry records which boot it
  belongs to, so entries left behind by an earlier boot or a different machine
  are recognised instead of believed. If something else has taken the directory
  oxutrm wants, it now says so and stops rather than quietly falling back to
  the home directory.
```

- [ ] **Step 8: Run the full gate**

Run: `make check`
Expected: exit 0, zero warnings. Record the counts; the total should exceed the pre-plan baseline by the tests added across all five tasks.

- [ ] **Step 9: Commit**

```bash
git add crates/oxutrm-host/src/registry.rs crates/oxutrm-host/tests/registry_roots.rs CHANGES.md
git commit -m "feat(registry): keep sessions on local storage, and refuse a squatted root"
```

---

## Done when

- `make check` is green: fmt, clippy `-D warnings`, and the full suite with **zero failures**, re-run on the tree being merged rather than inherited.
- Every new assertion has been **seen to fail** against its injected bug, with the RED and GREEN output recorded.
- `oxutrm host --list` on a Linux host with lingering off prints **no** warning and uses `/dev/shm/oxutrm-<uid>`.
- A squatted `/dev/shm/oxutrm-<uid>` produces a message naming the path, the cause, and `OXUTRM_STATE_DIR` — and does **not** fall back to the home directory.
- `choose_registry_root` is gone, with no second policy left behind.

## What this plan deliberately does not do

- **No socket/registry split.** The socket stays beside `meta.json`. Once the registry is local the split buys nothing and costs a schema field plus two location policies.
- **No hostname field.** `boot` covers the wrong-machine case on Linux; a hostname collides, changes, and says nothing about reboots.
- **No abstract-namespace sockets.** Attractive on Linux, but they have no filesystem permissions, so `SO_PEERCRED` would become load-bearing security — and being Linux-only they need this design as a fallback anyway. Revisit only if `/dev/shm` disappoints.
- **No migration of existing entries** under `$HOME/.local/state/oxutrm`. They describe processes that are ending anyway, and that path stays a valid last resort, so nothing is orphaned that was not already ephemeral.
- **No change to `Registry::ensure_dir`** or the per-session directory inside the root. Once the root is proven ours, the directories beneath it are ours by construction.

//! Real directories, real symlinks, real modes. `prepare_root` is about the
//! filesystem, so it cannot be tested against a table.

use oxutrm_host::registry::{PrepareError, prepare_root};

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

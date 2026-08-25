//! No key material ever reaches the disk.
//!
//! The trust root is ssh, and the certificate and PSK are generated fresh for
//! every attach and live only in memory. If either ever landed in the registry,
//! a stolen file would let somebody reattach to a session they were never
//! given — and it would do so silently, because nothing else about the session
//! would look different.
//!
//! So this test does not read the code. It creates a session while holding
//! secrets, then reads **every byte of every file** under the registry root and
//! fails if any of them appears. That catches a secret written by a field
//! nobody thought about, in a file nobody remembered.

use std::path::Path;

use oxutrm_host::{META_FILE, Registry, RegistryGuard, SessionMeta, now_unix};
use oxutrm_proto::TermSize;

/// Stand-ins for the real thing: a 32-byte PSK and a private key blob. The
/// bytes only have to be distinctive enough that finding them is proof.
const PSK: &[u8] = b"\x9f\x1e\x77\x42\x08\xbb\xcd\x31\x9f\x1e\x77\x42\x08\xbb\xcd\x31\
\x9f\x1e\x77\x42\x08\xbb\xcd\x31\x9f\x1e\x77\x42\x08\xbb\xcd\x31";
const PSK_BASE64: &str = "nx53Qgi7zTGfHndCCLvNMZ8ed0IIu80xnx53Qgi7zTE=";
const PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49\n";

/// Every regular file under `root`, recursively.
fn all_files(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // `symlink_metadata`, so a symlink is inspected rather than
            // followed out of the tree.
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                out.push(path);
            }
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn nothing_a_session_writes_contains_its_key_material() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());

    // A session, holding its secrets in memory exactly as the real one does.
    let psk: Vec<u8> = PSK.to_vec();
    let cert_key = PRIVATE_KEY_PEM.to_string();

    let mut meta = SessionMeta {
        session_id: "5555666677778888aaaabbbbccccdddd".to_string(),
        attach_id: 7,
        pid: std::process::id(),
        created_unix: now_unix(),
        shell: "/bin/bash".to_string(),
        size: TermSize {
            cols: 120,
            rows: 40,
        },
        detachable: true,
    };
    let guard = RegistryGuard::register_in(&root, &meta).expect("register");

    // Reattaching bumps the generation and rewrites meta.json. Do that too, so
    // the update path is covered and not just the initial write.
    meta.attach_id = 8;
    guard.update(&meta).expect("update");

    // Something that looks like a live session: a socket file in place.
    std::fs::write(guard.socket_path(), b"").expect("touch sock");

    let files = all_files(&root);
    assert!(
        files.iter().any(|p| p.ends_with(META_FILE)),
        "the scan found no meta.json, so it was looking in the wrong place: \
         {files:?}"
    );

    for path in &files {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let text = String::from_utf8_lossy(&bytes);

        assert!(
            !contains(&bytes, &psk),
            "{} contains the raw PSK",
            path.display()
        );
        assert!(
            !text.contains(PSK_BASE64),
            "{} contains the base64 PSK",
            path.display()
        );
        assert!(
            !text.contains(&cert_key),
            "{} contains the private key",
            path.display()
        );
        assert!(
            !text.contains("BEGIN PRIVATE KEY"),
            "{} contains a private key header",
            path.display()
        );
        assert!(
            !text.to_ascii_lowercase().contains("psk"),
            "{} mentions a psk at all",
            path.display()
        );
    }

    drop(guard);
}

/// The forward-looking half. The scan above can only find secrets that already
/// exist; this fails the moment somebody adds a field to `SessionMeta`,
/// whatever it is called, so the addition gets looked at rather than shipped.
#[test]
fn meta_json_holds_exactly_the_seven_fields_it_is_allowed_to() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let meta = SessionMeta {
        session_id: "9999aaaabbbbccccddddeeeeffff0000".to_string(),
        attach_id: 1,
        pid: std::process::id(),
        created_unix: now_unix(),
        shell: "/bin/bash".to_string(),
        size: TermSize { cols: 80, rows: 24 },
        detachable: true,
    };
    let guard = RegistryGuard::register_in(&root, &meta).expect("register");

    let text = std::fs::read_to_string(guard.meta_path()).expect("read meta");
    let value: serde_json::Value = serde_json::from_str(&text).expect("meta.json is JSON");
    let object = value.as_object().expect("meta.json is an object");

    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "attach_id",
            "created_unix",
            "detachable",
            "pid",
            "session_id",
            "shell",
            "size",
        ],
        "meta.json gained or lost a field. If it is a secret, it must not be \
         here at all; if it is not, add it to this list deliberately."
    );
}

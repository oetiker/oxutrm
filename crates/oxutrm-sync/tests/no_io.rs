//! The boundary that makes this crate worth having, enforced rather than
//! remembered.
//!
//! `oxutrm-sync` performs no I/O. That is not tidiness: it is what lets the
//! convergence property — the riskiest thing in the protocol — be tested
//! exhaustively with no socket, no PTY and no clock. A single dependency that
//! reaches the outside world destroys it silently, and nobody notices until
//! they wonder why the pure crate pulls in `signal-hook`.
//!
//! `include_str!` reads the manifest at COMPILE time, so this test does no
//! file I/O of its own.

/// The manifest, inlined at compile time.
const MANIFEST: &str = include_str!("../Cargo.toml");

/// Everything this crate is allowed to depend on, and why each one is safe.
///
/// An **allowlist**, not a denylist. A denylist only catches the I/O crates
/// someone thought to name; this fails on anything unrecognised, which is the
/// only version that still works in two years.
const ALLOWED: &[&str] = &[
    // Pure serialisation.
    "serde",
    "postcard",
    // Compression. Byte slices in, byte slices out - no files, no streams
    // that touch the outside world.
    "zstd",
    // Error derive macro; no runtime behaviour at all.
    "thiserror",
    // The wire vocabulary, which is itself I/O-free by the same rule.
    "oxutrm-proto",
];

/// Dependency names that would each be a specific, named regression.
const NEVER: &[&str] = &[
    "tokio",
    "async-std",
    "smol",
    "quinn",
    "mio",
    "socket2",
    "polling",
    "signal-hook",
    "rustix",
    "rustix-openpty",
    "alacritty_terminal",
    "reqwest",
    "hyper",
    "chrono",
    "time",
    "oxutrm-term",
    "oxutrm-net",
    "oxutrm-host",
    "oxutrm-client",
];

/// The dependency names in `[dependencies]`, ignoring `[dev-dependencies]`.
///
/// Dev-dependencies are deliberately out of scope: `proptest` is not shipped
/// and cannot reach a socket at runtime in anyone's build.
fn declared_dependencies() -> Vec<String> {
    let mut names = Vec::new();
    let mut in_section = false;
    for line in MANIFEST.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[dependencies]";
            continue;
        }
        if !in_section || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, _)) = line.split_once('=') else {
            continue;
        };
        // `foo.workspace = true` and `foo = { ... }` both start with the name.
        let name = name.trim().split('.').next().unwrap_or("").trim();
        if !name.is_empty() {
            names.push(name.to_owned());
        }
    }
    names
}

#[test]
fn the_manifest_parser_actually_finds_something() {
    // A parser that silently returns nothing would make every test below pass
    // for the wrong reason. This is the guard against that.
    let deps = declared_dependencies();
    assert!(
        !deps.is_empty(),
        "no dependencies parsed out of Cargo.toml - the parser is broken, not the manifest"
    );
    assert!(
        deps.contains(&"oxutrm-proto".to_owned()),
        "expected oxutrm-proto among {deps:?}"
    );
}

#[test]
fn every_dependency_is_on_the_allowlist() {
    for dep in declared_dependencies() {
        assert!(
            ALLOWED.contains(&dep.as_str()),
            "`{dep}` is not on oxutrm-sync's allowlist.\n\
             This crate performs NO I/O - no sockets, no files, no clocks - because that is \
             what makes the convergence property testable without a network.\n\
             If `{dep}` is genuinely pure, add it to ALLOWED in this test with a note saying \
             why. If it is not, it does not belong here."
        );
    }
}

#[test]
fn no_named_io_crate_appears_anywhere_in_the_manifest() {
    // Belt and braces: catches a dependency smuggled in under a section this
    // test's parser does not model, such as a target-specific table.
    //
    // Comments are stripped first. The manifest's own comment explains the
    // no-I/O rule BY NAMING the crates it excludes, so a raw substring search
    // over the whole file fails on the very documentation that states the
    // rule - which is a satisfying kind of wrong, and still wrong.
    let code: String = MANIFEST
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");

    for bad in NEVER {
        let mentioned = code
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .any(|word| word == *bad);
        assert!(
            !mentioned,
            "`{bad}` appears in oxutrm-sync's Cargo.toml. This crate must have no I/O in its \
             dependency tree at all."
        );
    }
}

#[test]
fn the_denylist_test_would_actually_catch_something() {
    // A guard on the guard: if the comment-stripping above were too greedy it
    // could leave nothing to search, and the test would pass vacuously.
    let code: String = MANIFEST
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        code.contains("oxutrm-proto"),
        "comment stripping ate the manifest; the denylist test would pass on an empty string"
    );
    assert!(
        !code.contains("DELIBERATELY NO I/O"),
        "comment stripping did not actually strip the comments"
    );
}

#[test]
fn the_crate_source_reaches_for_no_io_module() {
    // The manifest is the main gate, but std needs no declaration - so the
    // source is checked too.
    const SOURCES: &[(&str, &str)] = &[
        ("lib.rs", include_str!("../src/lib.rs")),
        ("screen.rs", include_str!("../src/screen.rs")),
        ("input.rs", include_str!("../src/input.rs")),
        ("channel.rs", include_str!("../src/channel.rs")),
    ];
    for (name, text) in SOURCES {
        for bad in [
            "std::fs",
            "std::net",
            "std::io::stdin",
            "std::time::Instant",
            "SystemTime",
        ] {
            assert!(
                !text.contains(bad),
                "{name} reaches for `{bad}`; oxutrm-sync has no I/O and no clock"
            );
        }
    }
}

// ---------------------------------------------------------------- transitive

/// Every crate in `oxutrm-sync`'s **resolved** normal-dependency closure.
///
/// The manifest checks above are necessary and not sufficient. They read this
/// crate's own `[dependencies]` and allowlist `oxutrm-proto` with a comment
/// saying it is I/O-free "by the same rule" — and that rule was enforced
/// nowhere. Add `reqwest` to `oxutrm-proto` tomorrow and every test above
/// still passes while this crate transitively links an HTTP stack, a socket
/// and a thread pool. That is exactly the silent erosion the boundary exists
/// to prevent.
///
/// So this is the whole closure, and it is an allowlist rather than a
/// denylist: a denylist only catches the I/O crates someone thought to name,
/// and it can be defeated by adding a fourth crate to the chain. Anything
/// unrecognised fails here, wherever in the tree it appears.
const TRANSITIVE_ALLOWED: &[&str] = &[
    // Ours.
    "oxutrm-sync",
    "oxutrm-proto",
    // Serialisation, and what serde pulls in.
    "serde",
    "serde_core",
    "serde_derive",
    "serde_json",
    "itoa",
    "memchr",
    "zmij",
    // postcard and its no_std building blocks.
    "postcard",
    "cobs",
    "heapless",
    "hash32",
    "byteorder",
    "stable_deref_trait",
    "spin",
    "lock_api",
    "scopeguard",
    // Compression: byte slices in, byte slices out.
    "zstd",
    "zstd-safe",
    "zstd-sys",
    // The screen model's cell text.
    "compact_str",
    "castaway",
    "rustversion",
    "static_assertions",
    // Error derive; no runtime behaviour.
    "thiserror",
    "thiserror-impl",
    // Proc-macro machinery, compile time only.
    "proc-macro2",
    "quote",
    "syn",
    "unicode-ident",
    // Small leaves.
    "base64",
    "bitflags",
    "cfg-if",
];

/// The resolved normal-dependency closure, from cargo itself.
///
/// `cargo tree` rather than hand-parsing lock files: it is cargo's own answer
/// to the question, it already excludes dev- and build-dependencies with
/// `-e normal`, and it accounts for feature resolution. Running a subprocess
/// is I/O — in the **test**, which is allowed; the crate under test still
/// performs none.
fn transitive_closure() -> Vec<String> {
    let out = std::process::Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "oxutrm-sync",
            "-e",
            "normal",
            "--prefix",
            "none",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree must run");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut names: Vec<String> = text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| !n.is_empty() && *n != "(*)")
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
}

#[test]
fn the_closure_walk_actually_finds_the_tree() {
    // A guard on the guard: a walk that silently returned nothing would make
    // the test below pass for the wrong reason.
    let closure = transitive_closure();
    assert!(closure.len() > 5, "suspiciously small closure: {closure:?}");
    assert!(
        closure.contains(&"oxutrm-proto".to_owned()),
        "got {closure:?}"
    );
    assert!(closure.contains(&"postcard".to_owned()), "got {closure:?}");
}

#[test]
fn nothing_in_the_whole_dependency_tree_performs_io() {
    for dep in transitive_closure() {
        assert!(
            TRANSITIVE_ALLOWED.contains(&dep.as_str()),
            "`{dep}` is in oxutrm-sync's TRANSITIVE dependency tree and is not allowlisted.\n\
             This crate performs no I/O - no sockets, no files, no clocks - and that has to \
             hold for everything it links, not just for what its own manifest names.\n\
             If `{dep}` is genuinely pure, add it to TRANSITIVE_ALLOWED with a note saying \
             why. If it reaches the outside world, something upstream of us grew a \
             dependency it should not have."
        );
    }
}

#[test]
fn the_named_io_crates_are_absent_from_the_whole_tree() {
    // Redundant with the allowlist above, and kept anyway: it names the
    // specific regressions, so the failure message says "tokio appeared"
    // rather than "something unrecognised appeared".
    let closure = transitive_closure();
    for bad in NEVER {
        assert!(
            !closure.iter().any(|d| d == bad),
            "`{bad}` is in oxutrm-sync's transitive dependency tree"
        );
    }
}

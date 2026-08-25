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

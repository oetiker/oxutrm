//! The edge this crate must not grow, enforced rather than remembered.
//!
//! `oxutrm-host` MUST NOT depend on `oxutrm-net`. The manifest says the
//! narrowness is deliberate, and the reason it is worth defending is not
//! tidiness:
//!
//! * It would drag quinn, rcgen, stun_codec, crab_nat, igd-next and netdev
//!   into a crate that supervises a PTY and keeps a session registry. Measured
//!   with `cargo tree -e normal`: 49 crates today, 175 in `oxutrm-net` alone.
//!   Every one of them becomes a rebuild, an audit surface and a version
//!   constraint on a crate that needs none of it.
//! * It would not even buy the thing anybody would add it for. A crate at this
//!   layer still cannot own the `quinn::Endpoint`: the ladder driver goes in
//!   the ROOT BINARY, which already depends on both halves and is the only
//!   place where owning the endpoint is coherent.
//!
//! The pressure is real and imminent — the ladder driver is exactly the work
//! that makes `oxutrm_net::` look like the shortest path from here — and until
//! now the rule was enforced by the manifest's absence plus code review, which
//! is to say by whoever happens to read the diff. Its sibling rule,
//! `oxutrm-sync`'s I/O purity, has been machine-checked since it was written;
//! `crates/oxutrm-sync/tests/no_io.rs` is the model this file follows.
//!
//! `include_str!` reads the manifest at COMPILE time, so this test does no file
//! I/O of its own.

/// The manifest, inlined at compile time.
const MANIFEST: &str = include_str!("../Cargo.toml");

/// Everything this crate is allowed to depend on, and why each one belongs.
///
/// An **allowlist** for the crate's own `[dependencies]`, because that is the
/// short, curated list where the forbidden edge would actually be written, and
/// because a new dependency here deserves a sentence explaining itself
/// regardless of which crate it is.
const ALLOWED: &[&str] = &[
    // Error plumbing.
    "anyhow",
    "thiserror",
    // The syscalls the registry and `daemonize` need. `rustix` for everything
    // it can wrap safely, `libc` only for `fork`, which it cannot.
    "rustix",
    "libc",
    // The wire vocabulary. Types only — it is I/O-free by its own rule, which
    // `oxutrm-sync`'s no_io.rs enforces.
    "oxutrm-proto",
    // The registry's on-disk form.
    "serde",
    "serde_json",
    // The signalling channel is a pair of pipes on a child `ssh` and the attach
    // path is a Unix socket, so both are async. Note what this does NOT make
    // acceptable: tokio is a runtime, not a transport, and a QUIC endpoint is
    // still somebody else's job.
    "tokio",
    // The per-attach PSK: 32 bytes from the OS CSPRNG, travelling base64 in
    // HostHello.
    "rand",
    "base64",
];

/// Dependency names that would each be a specific, named regression.
///
/// `oxutrm-net` itself, and the heavy transitive offenders it would bring, so
/// that the failure message says which one arrived rather than "something
/// unrecognised appeared". The transitive check below searches for these by
/// name through the whole tree, which is what catches the edge when it arrives
/// INDIRECTLY — `oxutrm-term` growing a net dependency would not touch this
/// manifest at all.
const NEVER: &[&str] = &[
    "oxutrm-net",
    "quinn",
    "quinn-proto",
    "quinn-udp",
    "rcgen",
    "stun_codec",
    "stunclient",
    "bytecodec",
    "crab_nat",
    "igd-next",
    "netdev",
];

/// The dependency names in `[dependencies]`, ignoring `[dev-dependencies]`.
///
/// Dev-dependencies are deliberately out of scope: `tempfile` is not shipped,
/// and a test fixture that reached for a socket would be a test's problem, not
/// a shipped coupling.
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
    // A parser that silently returned nothing would make every test below pass
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
    assert!(
        deps.contains(&"tokio".to_owned()),
        "expected tokio among {deps:?}"
    );
}

#[test]
fn every_dependency_is_on_the_allowlist() {
    for dep in declared_dependencies() {
        assert!(
            ALLOWED.contains(&dep.as_str()),
            "`{dep}` is not on oxutrm-host's allowlist.\n\
             This crate is deliberately narrow: a session registry, daemonizing and PTY \
             supervision. It must not reach the network - `oxutrm-net` and its QUIC stack \
             belong to the root binary, which is the only place that can own the endpoint.\n\
             If `{dep}` genuinely belongs here, add it to ALLOWED in this test with a note \
             saying why. If it is a step towards speaking QUIC from this crate, it does not."
        );
    }
}

#[test]
fn no_named_network_crate_appears_anywhere_in_the_manifest() {
    // Belt and braces: catches a dependency smuggled in under a section this
    // test's parser does not model, such as a target-specific table.
    //
    // Comments are stripped first. The manifest's own comment explains the rule
    // BY NAMING oxutrm-net as one of the crates this one does not ride on, so a
    // raw substring search over the whole file fails on the very documentation
    // that states the rule - which is a satisfying kind of wrong, and still
    // wrong.
    let code = manifest_without_comments();

    for bad in NEVER {
        let mentioned = code
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .any(|word| word == *bad);
        assert!(
            !mentioned,
            "`{bad}` appears in oxutrm-host's Cargo.toml. The QUIC half of the project is \
             `oxutrm-net`, and the crate that drives both is the root binary."
        );
    }
}

#[test]
fn the_denylist_test_would_actually_catch_something() {
    // A guard on the guard: if the comment-stripping above were too greedy it
    // could leave nothing to search, and the test would pass vacuously.
    //
    // Both halves are asserted STRUCTURALLY rather than against the manifest's
    // wording. An earlier version quoted the comment's first two words back at
    // it, which made rewording the comment fail this test - for a reason
    // nobody would believe in, which is the failure mode this file's own doc
    // comment names as the way a guard gets deleted.
    let code = manifest_without_comments();
    assert!(
        code.contains("[dependencies]"),
        "comment stripping ate the manifest; the denylist test would pass on an empty string"
    );
    // Something was actually removed. If this goes red the manifest has no
    // comments left, so the stripping is no longer exercised by this fixture
    // and the test above proves less than it claims - which is worth being
    // told about, and is a different fact from "the comment was reworded".
    assert!(
        code.len() < MANIFEST.len(),
        "comment stripping removed nothing: the manifest has no comments, so \
         `no_named_network_crate_appears_anywhere_in_the_manifest` is no longer \
         exercising the strip it depends on"
    );
    // And the search itself must be able to say yes, not only no.
    assert!(
        code.split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .any(|word| word == "tokio"),
        "the word-splitting search cannot find a dependency that IS there"
    );
}

fn manifest_without_comments() -> String {
    MANIFEST
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------- transitive

/// The resolved normal-dependency closure, from cargo itself.
///
/// `cargo tree` rather than hand-parsing lock files: it is cargo's own answer
/// to the question, it already excludes dev- and build-dependencies with
/// `-e normal`, and it accounts for feature resolution.
///
/// **Computed at most once per test binary.** Every call is a NESTED cargo, and
/// a nested cargo blocks on the package-cache lock rather than failing when the
/// outer build still holds it - so a second call is not a slow test, it is a
/// test that can hang indefinitely. `OnceLock` also makes the count independent
/// of how many tests happen to ask.
fn transitive_closure() -> &'static [String] {
    static CLOSURE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    CLOSURE.get_or_init(compute_transitive_closure)
}

fn compute_transitive_closure() -> Vec<String> {
    let out = std::process::Command::new(env!("CARGO"))
        .args([
            "tree",
            "-p",
            "oxutrm-host",
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
    assert!(closure.contains(&"tokio".to_owned()), "got {closure:?}");
}

/// The named regressions, through the WHOLE tree rather than one manifest.
///
/// This is a denylist where `oxutrm-sync`'s equivalent is an allowlist, and the
/// difference is not laziness. That crate's rule is a property — it performs no
/// I/O — so anything unrecognised anywhere in its tree is a candidate violation
/// and only an allowlist survives. This crate's rule is one specific edge, and
/// it legitimately links tokio, rustix and libc; an allowlist over 49 crates
/// would fail on every unrelated version bump, and a test that fails for
/// reasons nobody believes in is a test somebody deletes.
///
/// So: allowlist where the edge would be written by hand, denylist by name
/// everywhere else — which is what catches it arriving through a third crate.
#[test]
fn the_network_stack_is_absent_from_the_whole_dependency_tree() {
    let closure = transitive_closure();
    for bad in NEVER {
        assert!(
            !closure.iter().any(|d| d == bad),
            "`{bad}` is in oxutrm-host's transitive dependency tree.\n\
             Either this crate grew a dependency on oxutrm-net, or something it already \
             depends on did. The ladder driver belongs in the root binary, which depends on \
             both halves and is the only place that can own the quinn::Endpoint."
        );
    }
}

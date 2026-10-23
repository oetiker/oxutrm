# Live paths, stage 1: per-family rungs and the traffic spike — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every rung exists once per address family (IPv4, IPv6), is planned, skipped, tried and reported per family, the NAT type is learned per family, the birthday blast only aims at a family both ends can use — and a spike measures what keeping a path warm costs.

**Architecture:** `Rung` becomes `method(Family)` in `oxutrm-proto`, with the one classifier (`Rung::of_candidate`) that ICE and the ladder report both call, so the mirrored copy in `src/ladder.rs` goes. Discovery returns a `FamilyNat`, which travels in both hellos. The policy (`oxutrm_host::LadderPlan`) decides per family from both ends' facts; the mechanism (`src/ladder.rs`) runs one blast per admitted family. The spike is an example binary that never ships.

**Tech Stack:** Rust (workspace), tokio, quinn, serde/serde_json.

**Spec:** `docs/superpowers/specs/2026-10-19-live-paths-design.md` — this plan is §8 stage 1 (§5.1 and §5.4).

## Global Constraints

- No protocol compatibility: bump `PROTO_VERSION` (6 → 7), no compat flags, no `#[serde(default)]` for the new fields.
- `oxutrm-host` must not depend on `oxutrm-net` (`crates/oxutrm-host/tests/no_net.rs` checks it).
- Labels name family and method: "IPv4 direct", "IPv6 punched". A LAN or VPN IPv4 path is "IPv4 direct", never "punched".
- Rung order everywhere: IPv6 direct, IPv4 direct, IPv6 port mapped, IPv4 port mapped, IPv6 punched, IPv4 punched, IPv6 birthday, IPv4 birthday, SSH tunnel.
- Skipped and failed stay distinct, per family (`Verdict::Skipped` vs `Verdict::Failed`).
- Every user-visible change gets a `CHANGES.md` entry under `## [Unreleased]` with a **bold headline** sentence.
- Real addresses never go into the repo, tests, commit messages or PR bodies — use 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24, 2001:db8::/32. The repo is public.
- ssh to thinlinc only with the owner's go-ahead at the time; never thinlinc-adm.
- Known flaky/failing on the Mac, not caused by this work: `a_hostile_link_never_reaches_the_terminal`, `new_in_a_session_starts_a_named_sibling…`, some session/serve tests under full-suite load.

## Review Focus

- A remote address that arrives v4-mapped (`::ffff:192.0.2.7`, from a dual-stack socket's peer-reflexive discovery) must count as IPv4, not IPv6 — Task 1 test `a_v4_mapped_address_is_ipv4`.
- A family whose NAT is `Unknown` (no STUN server answered in it) must not lose its punched or birthday rung — Task 3 test `an_unknown_family_nat_skips_nothing_in_that_family`.
- Client and host each run the ladder; the birthday blast only works if **both** decide to blast the same family. Their plans must agree on the birthday rungs — Task 3 test `both_ends_agree_on_which_family_to_blast`.
- An end with no IPv6 at all still gets a report naming all nine rungs, the IPv6 ones as skipped with a reason, not "not reached" — Task 4 test `a_missing_family_is_skipped_by_name_in_the_report`.
- A peer that offers an IPv6 srflx with higher priority than its IPv4 one, to an end that only has IPv4, must get the blast aimed at the IPv4 one (today's bug) — Task 4 test `the_blast_aims_at_the_family_being_blasted`.

---

### Task 1: `Rung` is family × method, one classifier, family-aware labels

**Files:**
- Modify: `crates/oxutrm-proto/src/types.rs` (`Rung`, new `Family`, tests)
- Modify: `crates/oxutrm-net/src/ice.rs:362-375` (`nominated_event` calls the classifier)
- Modify: `src/ladder.rs` (delete `rung_of`, `rung_name`; `EVERY_RUNG` → `Rung::ALL`; `rung_number` → `Rung::method_number`; tests)
- Modify: `crates/oxutrm-client/src/status.rs` (`rung_label` uses `Display`)
- Modify: `crates/oxutrm-host/src/ladder.rs` (`for_nat` mechanically emits the new rungs, both families; real policy in Task 3), `crates/oxutrm-host/src/registry.rs` and every other `Rung::` site the compiler names
- Modify: `docs/manual.md:115-131`, `CHANGES.md`

**Interfaces:**
- Produces (in `oxutrm_proto`, re-exported like `Rung` today):
  - `pub enum Family { V4, V6 }` — `Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize`
  - `impl Family { pub const BOTH: [Family; 2] = [Family::V6, Family::V4]; pub fn of(addr: &SocketAddr) -> Family; pub fn label(self) -> &'static str /* "IPv4" | "IPv6" */ }` — `of` uses `ip().to_canonical()` so a v4-mapped address is `V4`.
  - `pub enum Rung { Direct(Family), PortMapped(Family), Punched(Family), Birthday(Family), SshTunnel }`
  - `impl Rung { pub const ALL: [Rung; 9] /* the Global Constraints order */; pub fn family(self) -> Option<Family>; pub fn method_number(self) -> u8 /* Direct 0, PortMapped 1, Punched 2, Birthday 3, SshTunnel 4 */; pub fn of_candidate(kind: CandidateKind, remote: SocketAddr) -> Rung }`
  - `impl fmt::Display for Rung` → `"IPv4 direct"`, `"IPv6 port mapped"`, `"IPv4 punched"`, `"IPv6 birthday"`, `"SSH tunnel"`.
- Classifier rule: `PortMapped` → `PortMapped(f)`; `Host` → `Direct(f)` (both families — this is the fix to "IPv4 LAN is punched"); `ServerReflexive`/`PeerReflexive` → `Punched(f)`; `f = Family::of(&remote)`.

- [ ] **Step 1: Write the failing tests in `crates/oxutrm-proto/src/types.rs`**

Replace the `Rung` asserts in `the_enums_are_named_on_the_wire_not_numbered` with:
```rust
assert_eq!(serde_json::to_string(&Rung::Direct(Family::V6)).unwrap(), r#"{"Direct":"V6"}"#);
assert_eq!(serde_json::to_string(&Rung::Punched(Family::V4)).unwrap(), r#"{"Punched":"V4"}"#);
assert_eq!(serde_json::to_string(&Rung::SshTunnel).unwrap(), r#""SshTunnel""#);
```
Add:
```rust
#[test]
fn a_host_candidate_is_direct_in_either_family() {
    let v4: SocketAddr = "192.168.0.11:443".parse().unwrap();
    let v6: SocketAddr = "[2001:db8::2]:443".parse().unwrap();
    assert_eq!(Rung::of_candidate(CandidateKind::Host, v4), Rung::Direct(Family::V4));
    assert_eq!(Rung::of_candidate(CandidateKind::Host, v6), Rung::Direct(Family::V6));
    assert_eq!(Rung::of_candidate(CandidateKind::PortMapped, v6), Rung::PortMapped(Family::V6));
    assert_eq!(Rung::of_candidate(CandidateKind::ServerReflexive, v4), Rung::Punched(Family::V4));
    assert_eq!(Rung::of_candidate(CandidateKind::PeerReflexive, v6), Rung::Punched(Family::V6));
}

#[test]
fn a_v4_mapped_address_is_ipv4() {
    let mapped: SocketAddr = "[::ffff:192.0.2.7]:443".parse().unwrap();
    assert_eq!(Family::of(&mapped), Family::V4);
    assert_eq!(Rung::of_candidate(CandidateKind::PeerReflexive, mapped), Rung::Punched(Family::V4));
}

#[test]
fn every_rung_is_labelled_with_family_and_method_in_ladder_order() {
    let labels: Vec<String> = Rung::ALL.iter().map(ToString::to_string).collect();
    assert_eq!(labels, [
        "IPv6 direct", "IPv4 direct", "IPv6 port mapped", "IPv4 port mapped",
        "IPv6 punched", "IPv4 punched", "IPv6 birthday", "IPv4 birthday", "SSH tunnel",
    ]);
}
```
Update `a_path_description_round_trips` to `Rung::Birthday(Family::V4)`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p oxutrm-proto types::`
Expected: compile errors — `Family` and the new variants do not exist.

- [ ] **Step 3: Implement `Family` and the new `Rung` in `types.rs`**, per the Interfaces block. Move the `SshTunnel` doc comment onto the variant.

- [ ] **Step 4: Make the workspace compile against the new `Rung`**

- `ice.rs` `nominated_event`: `let rung = kind.map_or(Rung::Punched(Family::of(&remote)), |k| Rung::of_candidate(k, remote));` — an unknown kind is a peer-reflexive learn, as today's `_` arm.
- `src/ladder.rs`: delete `rung_of` (call `Rung::of_candidate(c.kind, c.addr)`), `rung_name` (use `Display`), `EVERY_RUNG` (use `Rung::ALL`) and `rung_number` (use `method_number`). Report line format stays `"  rung {}  {:<20}  {:<11}  {}"` with `method_number()` and the `Display` label. Delete `the_rung_classification_matches_the_one_ice_nominates_by` — the mirror it guarded is gone; the proto tests replace it.
- `crates/oxutrm-host/src/ladder.rs` `for_nat(nat)`: same rules as today, applied to both families: raced = `Direct(V6), Direct(V4), PortMapped(V6), PortMapped(V4)` then `Punched(V6), Punched(V4)` unless symmetric (then both `Punched` skipped with today's reason); sequential = `Birthday(V6), Birthday(V4), SshTunnel`. Task 3 replaces this.
- `src/ladder.rs` `nominate`: the blast block runs for `Rung::Birthday(f)` when the plan's sequential list contains it, recording the verdict on that rung; keep today's `blast_base` for now (Task 4 fixes the family).
- `status.rs`: `rung_label` = `path.rung.to_string()`, except `Birthday(_)` → `format!("{} ({} probes)", path.rung, path.probes_sent)`. `status_line` keeps its three shapes, matching `Rung::SshTunnel` and `Rung::Birthday(_)`. Delete the local `family` helper.
- `registry.rs`: `!matches!(rung, Rung::SshTunnel)` is unchanged; fix the other sites the compiler names, picking the same family as the test's addresses (e.g. `path_of(Rung::Punched(Family::V4), …)` where the remote is IPv4).

- [ ] **Step 5: Update the tests whose expected text changes**

`status.rs` tests: `"IPv6 direct"`, `"IPv4 port mapped"`, `"IPv4 punched"`, `"IPv4 birthday (312 probes)"` (use the probe counts the tests already have). Add to `status.rs`:
```rust
#[test]
fn a_lan_or_vpn_ipv4_path_is_direct_not_punched() {
    let p = path(Rung::Direct(Family::V4), "192.168.0.11:443", 3, 1389, 0, NatType::None);
    assert!(status_line(&p).contains("IPv4 direct"), "{}", status_line(&p));
}
```
`src/ladder.rs` `the_report_names_every_rung_by_number_and_by_name`: iterate `Rung::ALL`, assert `rung {}` with `method_number()` and the `Display` label. `crates/oxutrm-host/tests/ladder.rs`: adjust to the two-family lists; the symmetric case skips both `Punched` rungs.

- [ ] **Step 6: Run the workspace tests**

Run: `cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: every `test result: ok`, apart from the known flaky tests in Global Constraints.

- [ ] **Step 7: Manual and changelog**

`docs/manual.md`: the status-line example stays `IPv6 direct`. Replace the numbered list at line 124 with the order of the Global Constraints, as five method lines each saying "over IPv6, then over IPv4" (direct = both ends' own addresses, including a LAN or VPN; then port mapping; then STUN punch; then birthday; then the ssh tunnel). `CHANGES.md` under `### Changed`:
```markdown
- **The status line names the address family of every path.** A path over a
  LAN or VPN address now reads "IPv4 direct" instead of "IPv4 punched", and a
  router port mapping reads "IPv4 port mapped".
```

- [ ] **Step 8: Commit**

```bash
git add -u crates src docs CHANGES.md
git commit -m "feat(ladder): rungs per address family, one classifier"
```

---

### Task 2: NAT type per family, on the wire

**Files:**
- Modify: `crates/oxutrm-proto/src/types.rs` (`FamilyNat`), `crates/oxutrm-proto/src/signal.rs` (`HostHello.nat`, `ClientHello.nat`), `crates/oxutrm-proto/src/lib.rs:157` (`PROTO_VERSION = 7`)
- Modify: `crates/oxutrm-net/src/discover.rs:176-205` (`stun_discover` returns `FamilyNat`)
- Modify: `src/connect.rs`, `src/attach_exchange.rs`, `crates/oxutrm-net/src/bin/oxutrm-netns-peer.rs`, `crates/oxutrm-host/src/bin/oxutrm-fake-ssh.rs` and the test fixtures the compiler names

**Interfaces:**
- Consumes: `Family`, `Rung::family()` (Task 1).
- Produces:
  - `pub struct FamilyNat { pub v4: NatType, pub v6: NatType }` — `Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize`; `impl FamilyNat { pub const UNKNOWN: FamilyNat; pub fn get(self, f: Family) -> NatType }`
  - `oxutrm_net::stun_discover(socket, cfg) -> (Vec<Candidate>, FamilyNat)` — `v4` from `discover_family` over the IPv4 servers, `v6` over the IPv6 ones; an IPv4-only socket gets `v6: Unknown`.
  - Hello field `nat: FamilyNat` replaces `nat_type: NatType` in both `HostHello` and `ClientHello`.
  - `PathDescription.nat_type` stays a single `NatType`: the NAT of the nominated rung's family (`rung.family().map_or(NatType::Unknown, |f| nat.get(f))`).
  - `Ladder.nat` in `src/ladder.rs` becomes `FamilyNat`; until Task 3, `nominate` passes `nat.v4` to `for_nat` when it is not `Unknown`, else `nat.v6` — today's rule, moved.

- [ ] **Step 1: Write the failing tests**

`types.rs`:
```rust
#[test]
fn the_nat_type_travels_per_family() {
    let n = FamilyNat { v4: NatType::Symmetric, v6: NatType::None };
    assert_eq!(serde_json::to_string(&n).unwrap(), r#"{"v4":"Symmetric","v6":"None"}"#);
    assert_eq!(n.get(Family::V4), NatType::Symmetric);
    assert_eq!(n.get(Family::V6), NatType::None);
}
```
`discover.rs`: change the loopback test that asserts `nat == NatType::None` into `assert_eq!(nat.v4, NatType::None)` and add `assert_eq!(nat.v6, NatType::Unknown, "an IPv4 socket learns nothing about IPv6")`. `attach_exchange.rs:939`-style path tests: a client hello with `nat: FamilyNat { v4: NatType::Symmetric, v6: NatType::None }` and a nomination on `Rung::Birthday(Family::V4)` yields `path.nat_type == NatType::Symmetric`; on `Rung::Direct(Family::V6)` it yields `NatType::None`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p oxutrm-proto && cargo test -p oxutrm-net discover`
Expected: compile errors — `FamilyNat` does not exist.

- [ ] **Step 3: Implement `FamilyNat`, the hello fields, the discovery return value and `PROTO_VERSION = 7`.** Delete the "NAT type is the IPv4 family's when it answered" paragraph from `stun_discover`'s doc comment and say each family is classified on its own. Fix the JSON literals in the `signal.rs` tests (`"nat_type":"Unknown"` → `"nat":{"v4":"Unknown","v6":"Unknown"}`) and in `src/serve.rs:1113`, `src/door.rs:1032`.

- [ ] **Step 4: Run the workspace tests**

Run: `cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: all ok, except the known flaky tests.

- [ ] **Step 5: Commit**

```bash
git add -u crates src
git commit -m "feat(net): learn and exchange the NAT type per address family"
```

---

### Task 3: The plan decides per family, from both ends' facts

**Files:**
- Modify: `crates/oxutrm-host/src/ladder.rs` (replace `for_nat` with `for_ends`, new `EndFacts`)
- Test: `crates/oxutrm-host/tests/ladder.rs`

**Interfaces:**
- Consumes: `Family`, `Rung`, `FamilyNat` (Tasks 1–2).
- Produces:
  - `pub struct EndFacts { pub nat: FamilyNat, pub v4: bool, pub v6: bool }`
  - `impl EndFacts { pub fn from_candidates(nat: FamilyNat, candidates: &[Candidate]) -> EndFacts /* a family is present when any candidate is in it, by Family::of */; pub fn has(&self, f: Family) -> bool }`
  - `impl LadderPlan { pub fn for_ends(ours: &EndFacts, theirs: &EndFacts) -> LadderPlan }` — `LadderPlan`'s fields unchanged; `for_nat` deleted.
- Rules, per family `f` in `Family::BOTH` order:
  1. `!ours.has(f)` → every `f` rung skipped: `"this end has no {IPv6} address"`.
  2. else `!theirs.has(f)` → every `f` rung skipped: `"the peer has no {IPv6} address"`.
  3. else `ours.nat.get(f) == Symmetric` → `Punched(f)` skipped with today's reason, prefixed by the family (`"the IPv4 NAT is symmetric, so …"`).
  4. else-independent of 3: `Birthday(f)` is planned only if `ours.nat.get(f)` or `theirs.nat.get(f)` is `Symmetric` or `Unknown`; otherwise skipped: `"neither end's {IPv4} NAT is symmetric, so ordinary punching covers it"`.
  - raced = the not-skipped `Direct`, `PortMapped`, `Punched` rungs in `Rung::ALL` order; sequential = the not-skipped `Birthday` rungs in `Rung::ALL` order, then `SshTunnel` (always, last).
- Rule 4 depends only on the unordered pair of NATs and rules 1–2 on both ends having the family, so client and host reach the same birthday decision. That is the point of rule 4 reading both ends; the test below pins it.

- [ ] **Step 1: Write the failing tests in `crates/oxutrm-host/tests/ladder.rs`**

Replace the `for_nat` tests with `for_ends` ones. Helper: `fn end(v4: NatType, v6: NatType, has_v4: bool, has_v6: bool) -> EndFacts`. Tests:
```rust
#[test]
fn a_symmetric_ipv4_nat_does_not_stop_ipv6_punching() {
    let ours = end(NatType::Symmetric, NatType::EndpointIndependent, true, true);
    let theirs = end(NatType::EndpointIndependent, NatType::EndpointIndependent, true, true);
    let plan = LadderPlan::for_ends(&ours, &theirs);
    assert!(plan.raced.contains(&Rung::Punched(Family::V6)));
    assert!(!plan.raced.contains(&Rung::Punched(Family::V4)));
    assert!(plan.sequential.contains(&Rung::Birthday(Family::V4)));
    assert!(!plan.sequential.contains(&Rung::Birthday(Family::V6)));
}

#[test]
fn a_family_one_end_lacks_is_skipped_with_whose_it_is() {
    let ours = end(NatType::EndpointIndependent, NatType::Unknown, true, false);
    let theirs = end(NatType::EndpointIndependent, NatType::None, true, true);
    let plan = LadderPlan::for_ends(&ours, &theirs);
    for r in [Rung::Direct(Family::V6), Rung::PortMapped(Family::V6), Rung::Punched(Family::V6), Rung::Birthday(Family::V6)] {
        let why = &plan.skipped.iter().find(|(s, _)| *s == r).expect("skipped").1;
        assert!(why.contains("this end has no IPv6 address"), "{r}: {why}");
    }
    let plan = LadderPlan::for_ends(&theirs, &ours);
    let why = &plan.skipped.iter().find(|(s, _)| *s == Rung::Direct(Family::V6)).unwrap().1;
    assert!(why.contains("the peer has no IPv6 address"), "{why}");
}

#[test]
fn an_unknown_family_nat_skips_nothing_in_that_family() {
    let ours = end(NatType::Unknown, NatType::Unknown, true, true);
    let plan = LadderPlan::for_ends(&ours, &ours);
    assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
}

#[test]
fn both_ends_agree_on_which_family_to_blast() {
    let a = end(NatType::Symmetric, NatType::None, true, true);
    let b = end(NatType::EndpointIndependent, NatType::None, true, true);
    let births = |p: &LadderPlan| p.sequential.iter().filter(|r| matches!(r, Rung::Birthday(_))).copied().collect::<Vec<_>>();
    assert_eq!(births(&LadderPlan::for_ends(&a, &b)), births(&LadderPlan::for_ends(&b, &a)));
    assert_eq!(births(&LadderPlan::for_ends(&a, &b)), vec![Rung::Birthday(Family::V4)]);
}
```
Keep `the_tunnel_is_always_last` against `for_ends` over a few fact combinations, and add one asserting that every rung of `Rung::ALL` appears in exactly one of `raced`, `sequential`, `skipped`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p oxutrm-host --test ladder`
Expected: compile errors — `EndFacts` and `for_ends` do not exist.

- [ ] **Step 3: Implement `EndFacts` and `for_ends`** per the rules above. Rewrite the module doc's "Why the NAT type short-circuits" section for per-family NATs and the birthday rule, in the file's existing voice.

- [ ] **Step 4: Point `src/ladder.rs` at it, minimally** — `nominate` builds `EndFacts::from_candidates(ladder.nat, &ladder.local)` and, for the peer, `EndFacts::from_candidates(ladder.peer_nat, &ladder.remote)`; add `pub peer_nat: FamilyNat` to `Ladder` and fill it from the hello at `src/connect.rs:511` (`host.nat`) and `src/attach_exchange.rs:155` (`client.nat`). The test fixtures in `src/ladder.rs` set `peer_nat` to the same value as `nat`.

- [ ] **Step 5: Run the workspace tests**

Run: `cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: all ok, except the known flaky tests. Tests in `src/ladder.rs` that relied on the blast running after a failed race with an endpoint-independent NAT now see `Birthday` skipped: set their fixture's `nat` to `Symmetric` in the family they blast, as the blast tests already do.

- [ ] **Step 6: Commit**

```bash
git add -u crates src
git commit -m "feat(ladder): plan each address family from both ends' facts"
```

---

### Task 4: The mechanism runs per family; the blast aims at its own family

**Files:**
- Modify: `src/ladder.rs` (`blast_base`, `blast`, the sequential part of `nominate`, tests)
- Modify: `CHANGES.md`

**Interfaces:**
- Consumes: `LadderPlan::for_ends`, `Ladder.peer_nat` (Task 3).
- Produces:
  - `fn blast_base(remote: &[Candidate], family: Family) -> Option<SocketAddr>` — highest-priority `ServerReflexive` candidate **of `family`** (by `Family::of`).
  - `async fn blast(ladder: &Ladder<'_>, family: Family, already_sent: u32) -> Result<Nomination, Verdict>` — the no-base skip reason names the family: `"the peer offered no IPv4 server-reflexive candidate, so there is no observed external port to guess around"`. The `Nomination.rung` is `Rung::Birthday(family)`.
  - `nominate` walks `plan.sequential` in order: each `Birthday(f)` runs `blast(&ladder, f, probes)`, and a win returns at once; `SshTunnel` keeps its "not implemented" skip.

- [ ] **Step 1: Write the failing tests in `src/ladder.rs`**

```rust
#[test]
fn the_blast_aims_at_the_family_being_blasted() {
    let remote = [
        candidate("[2001:db8::9]:40000", CandidateKind::ServerReflexive),
        candidate("203.0.113.7:51234", CandidateKind::ServerReflexive),
    ];
    assert_eq!(blast_base(&remote, Family::V4), Some("203.0.113.7:51234".parse().unwrap()));
    assert_eq!(blast_base(&remote, Family::V6), Some("[2001:db8::9]:40000".parse().unwrap()));
    assert_eq!(blast_base(&remote[..1], Family::V4), None);
}

#[tokio::test]
async fn a_missing_family_is_skipped_by_name_in_the_report() {
    // Ours: IPv4 only. Theirs: both. Nothing listens, so the IPv4 race fails.
    let report = nominate(sock, Ladder { local: vec![candidate("127.0.0.1:1", Host)],
        remote: vec![candidate("127.0.0.1:9", ServerReflexive), candidate("[2001:db8::9]:9", ServerReflexive)],
        nat: FamilyNat { v4: NatType::EndpointIndependent, v6: NatType::Unknown }, peer_nat: /* same */, .. }, …)
        .await.expect_err("nothing listens");
    for rung in Rung::ALL {
        assert!(!matches!(report.verdict(rung), Verdict::NotReached), "{rung}: {report}");
    }
    assert!(matches!(report.verdict(Rung::Punched(Family::V6)), Verdict::Skipped(w) if w.contains("this end has no IPv6 address")));
    assert!(matches!(report.verdict(Rung::Punched(Family::V4)), Verdict::Failed(_)));
}
```
(Write the fixture in full the way `a_total_failure_still_accounts_for_every_rung` does; the sketch above shows only what differs.) Update `the_blast_guesses_around_a_server_reflexive_address_only` and `the_blast_is_skipped_with_a_reason_when_there_is_no_base_to_guess_around` to pass a family and expect the family in the reason. `a_blast_win_hands_back_the_socket_that_punched_the_hole` asserts `rung == Rung::Birthday(Family::V4)`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --bin oxutrm ladder::`
Expected: compile error on `blast_base`'s arity, then failures on the report test.

- [ ] **Step 3: Implement** the signatures above. Update the "Rung 3." comment in `nominate` to say it runs once per planned family.

- [ ] **Step 4: Run the workspace tests**

Run: `cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: all ok, except the known flaky tests.

- [ ] **Step 5: Changelog and commit**

`CHANGES.md` under `### Fixed`:
```markdown
- **The birthday punch aims at an address family both ends can use.** It
  guessed around the peer's best public address, which could be IPv6 when this
  end had none, and then could never succeed. It now runs per family, and only
  for a family where one end's NAT is symmetric or unknown.
```
Under `### Changed`, after the Task 1 entry:
```markdown
- **A symmetric IPv4 NAT no longer stops IPv6 punching.** The NAT type is
  learned for each address family, and a failure report lists every path
  class once per family, saying which end lacks a family.
```
```bash
git add -u src CHANGES.md
git commit -m "fix(ladder): blast per address family, aimed at that family"
```

---

### Task 5: The traffic spike tool (`path-cost` example)

**Files:**
- Create: `crates/oxutrm-net/examples/path-cost.rs` (never shipped: examples are not built by `cargo build` or installed)
- Modify: `crates/oxutrm-net/src/quic.rs` only if a keepalive override is unavoidable (prefer not; see Step 1)

**Interfaces:**
- Consumes: `oxutrm_net::{quic_server, quic_client, generate_cert}`.
- Produces: a CLI with two modes, printing one plain-text line per result:
  - `path-cost idle --secs 60 [--ping-hz 0,0.2,0.5,1,2]` — on loopback, builds a real oxutrm QUIC pair (shipped transport config: 10 s keepalive) and for each ping rate keeps it idle except for 16-byte QUIC datagram pings echoed by the other side; reads `Connection::stats()` (`udp_tx`/`udp_rx` bytes and datagrams) on both ends. Prints per rate: datagrams/s and bytes/s each way, UDP payload and on-wire with 28 B (IPv4) / 48 B (IPv6) header per datagram.
  - `path-cost hold --role listen|send --bind ADDR --peer ADDR --holds 15,30,60,120,240,480` — measures how long the **listener's** NAT keeps an idle UDP mapping open. One socket per hold, on ports `bind.port + i`. Both ends punch (send every 200 ms until they hear the peer, then 5 more); the listener then goes silent and sends one `"silent"` datagram; the sender, on hearing `"silent"` on socket `i`, waits `holds[i]` seconds and sends 3 datagrams 100 ms apart. The listener prints, per hold, `hold 60s: arrived` or `hold 60s: lost` after `holds[i] + 10 s`. Run it twice with the roles swapped to measure both NATs. Prints no addresses — only holds and outcomes.

The punch here is the owner's earlier `punch.py` done in Rust; no PSK, no STUN — it is a throwaway measuring tool and carries nothing but counters.

- [ ] **Step 1: Write `idle` mode** using the real `quic_server`/`quic_client`, so it measures the shipped transport config. Do not add a keepalive knob to `quic.rs`: keepalive cost is linear in its rate, so the 10 s figure scales; the ping rates stand for stage 3's measurement traffic.

- [ ] **Step 2: Run it**

Run: `cargo run -q --release -p oxutrm-net --example path-cost -- idle --secs 60`
Expected: five result lines (one per ping rate) with non-zero bytes/s, the 0 Hz line being keepalive alone (~one PING + ACK every 10 s each way).

- [ ] **Step 3: Write `hold` mode** and check it on loopback

Run, in two shells: `cargo run -q --release -p oxutrm-net --example path-cost -- hold --role listen --bind 127.0.0.1:47000 --peer 127.0.0.1:48000 --holds 2,4` and the same with `--role send --bind 127.0.0.1:48000 --peer 127.0.0.1:47000`.
Expected: the listener prints `hold 2s: arrived` and `hold 4s: arrived` (no NAT on loopback).

- [ ] **Step 4: Clippy and commit**

Run: `cargo clippy -p oxutrm-net --examples -- -D warnings`
Expected: no warnings.
```bash
git add crates/oxutrm-net/examples/path-cost.rs
git commit -m "chore(spike): path-cost tool for keepalive cost and NAT hold time"
```

---

### Task 6: Run the spike on real links and report

This task is the controller's, not a subagent's: it needs the owner's go-ahead for ssh to thinlinc, and the owner's own network.

**Files:**
- Create: `docs/superpowers/notes/2026-10-19-path-cost-spike.md` — numbers only: link class ("Mac ↔ thinlinc, public IPv4", "IPv6 NAT66", "VPN IPv4"), holds and outcomes, bytes/s. **No addresses, no host names beyond `thinlinc`.**

- [ ] **Step 1: Ask the owner for the go-ahead to ssh to thinlinc** and build the example there (`/scratch/oetiker/cargo-target-oxutrm-main` as `CARGO_TARGET_DIR`; build from this branch's tip in a worktree, not from `~/checkouts/oxutrm` on thinlinc).

- [ ] **Step 2: Run `idle`** on the Mac (loopback figures are link-independent; the per-family header overhead covers IPv4 vs IPv6).

- [ ] **Step 3: Run `hold` over public IPv4** both ways (Mac listens, then thinlinc listens), with the VPN down so the path really crosses both NATs. The owner reads off the public addresses; they are typed on the command line and go nowhere else.

- [ ] **Step 4: Run `hold` over IPv6** (thinlinc's NAT66 global address) both ways, and once over the VPN IPv4 path as the no-NAT baseline.

- [ ] **Step 5: Write the note and the recommendation**

The note holds a table: per link class, the longest hold that still arrived and the shortest that was lost, per direction; and bytes/s per kept path at each ping rate. The recommendation: keepalive interval = shortest observed lifetime ÷ 2, floored at today's 10 s if that is shorter; measurement ping rate; resulting bytes/s per kept path and for 2, 4 and 8 kept paths. Present it to the owner as the input for the §5.4 budget decision.

- [ ] **Step 6: Commit**

```bash
git add docs/superpowers/notes/2026-10-19-path-cost-spike.md
git commit -m "docs(spike): measured keepalive cost and NAT hold times"
```

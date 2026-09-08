//! Rebuilding the link the client is on, without being asked.
//!
//! One attempt is a **fresh ssh channel and the exchange every other connect
//! runs**: read the offer, answer it with `Attach { id }` naming the session
//! this client already has, then [`crate::connect::establish`]. There is no
//! reattach-only code path here and there must not be one --
//! `crates/oxutrm-host/src/attach.rs` states the rule and this is the client's
//! half of keeping it.
//!
//! What is new is not the exchange but the **judgement about failure**: an
//! answer from the far end ends the loop, and anything else is worth trying
//! again. See [`AttemptOutcome`].

use oxutrm_host::ssh::{BootstrapError, SshChannel, SshLauncher};
use oxutrm_net::NetConfig;
use oxutrm_proto::{Choice, Signal, TermSize};

use crate::connect::{Established, HostRefused, establish, read_offer};

/// A rebuild's ssh must never stop to ask a question.
///
/// Raw mode is held and the screen belongs to the renderer, so a passphrase
/// prompt would fight it for the terminal. `BatchMode=yes` turns that into a
/// clean failure the notice can explain instead. The real answer is B3's
/// askpass; until then this is the deliberate, legible degradation the spec
/// asks for.
const BATCH_MODE: [&str; 2] = ["-o", "BatchMode=yes"];

/// What one attempt produced, and what the loop should do about it.
///
/// The split between the two failures is the whole design (spec §5.4). A
/// `Definite` is an answer -- the far end spoke, and repeating the question
/// gets the same answer -- so the loop ends and the user is told. Everything
/// else is a `Retry`, because a network that is down is a network that may
/// come back, and giving up on it is precisely what oxutrm exists not to do.
pub(crate) enum AttemptOutcome {
    /// Boxed: an [`Established`] is an order of magnitude larger than the two
    /// strings beside it, and this enum travels inside `run_on`'s `Wake`,
    /// which is built on every lap of a loop that runs at the pacing rate. One
    /// allocation per successful rebuild is not a cost anybody can measure.
    Landed(Box<Established>),
    /// Worth trying again, with the reason kept for the notice.
    Retry(String),
    /// Not worth trying again, in the far end's own words where there are any.
    Definite(String),
}

impl std::fmt::Debug for AttemptOutcome {
    /// Hand-written because [`Established`] holds a live [`crate::link::Link`],
    /// which is a QUIC connection and an endpoint rather than anything
    /// printable. The two identities are what a failing test needs to read.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttemptOutcome::Landed(e) => f
                .debug_struct("Landed")
                .field("session_id", &e.session_id)
                .field("attach_id", &e.attach_id)
                .finish_non_exhaustive(),
            AttemptOutcome::Retry(why) => f.debug_tuple("Retry").field(why).finish(),
            AttemptOutcome::Definite(why) => f.debug_tuple("Definite").field(why).finish(),
        }
    }
}

/// One attempt at getting back into `session_id` on `target`.
///
/// `launcher` is the injection point, exactly as it is for
/// [`SshChannel::open`]: production passes [`SshLauncher::ssh`] and the tests
/// point it at a script that speaks the protocol on stdio. [`BATCH_MODE`] is
/// added here rather than by the caller, so every attempt carries it and the
/// tests exercise the argument list that ships.
///
/// `size` is the client's **current** terminal size, so the host adopts at the
/// right geometry from the `ClientHello` alone with no resize afterwards.
///
/// # Why the channel may be dropped when this returns
///
/// `connect` holds its `SshChannel` for the whole session; this drops it as
/// soon as the exchange is over, which kills that ssh. Nothing is lost: the
/// far end of a rebuild is `oxutrm host --attach`, which relays signalling and
/// exits when the exchange completes -- no session data ever crosses it. The
/// one shape that would need ssh afterwards is a rung-4 tunnel, and rung 4 has
/// no implementation anywhere in the tree (see `ladder::nominate`), so it
/// cannot be nominated.
pub(crate) async fn attempt(
    launcher: &SshLauncher,
    target: &str,
    session_id: &str,
    size: TermSize,
    cfg: &NetConfig,
) -> AttemptOutcome {
    let launcher = BATCH_MODE
        .iter()
        .fold(launcher.clone(), |batch, arg| batch.arg(arg));

    let mut channel = match SshChannel::open(&launcher, target).await {
        Ok(channel) => channel,
        Err(e) => return classify(target, &anyhow::Error::new(e)),
    };

    // The same first read `connect` makes, on the same concrete type and for
    // the same reason: `SshChannel::recv` reaps the child, so this is where a
    // far end that is missing, too old or dead is diagnosed. What was offered
    // is deliberately not consulted -- the host decides whether the session we
    // name is still there, and a client that re-decided it would be a second
    // place that can be wrong about it.
    if let Err(e) = read_offer(&mut channel).await {
        return classify(target, &e);
    }

    let choice = Choice::Attach {
        id: session_id.to_owned(),
    };
    if let Err(e) = channel.send(&Signal::Choose { choice }).await {
        return classify(target, &anyhow::Error::new(e));
    }

    let (reader, writer) = channel.halves();
    match establish(reader, writer, size, cfg).await {
        Ok(established) => AttemptOutcome::Landed(Box::new(established)),
        Err(e) => classify(target, &e),
    }
}

/// What a client needs to get back into the session it already has.
///
/// Held by [`crate::session::ClientSession`], which is where the two
/// identities come from: `session_id` is the one the host named in its
/// `HostHello` (so a rebuild resumes THIS session and not a new one), and
/// `target` is the ssh target off the command line.
///
/// The in-flight attempt lives here too, so that a frame arriving on the old
/// link can drop it. Everything the loop selects on stays a local in `run_on`;
/// this is only ever touched between laps.
pub(crate) struct Rebuild {
    target: String,
    session_id: String,
    /// How to start ssh. The injection point, as in [`attempt`].
    launcher: SshLauncher,
    cfg: NetConfig,
    in_flight: Option<tokio::task::JoinHandle<()>>,
    /// An attempt has begun and no swap has happened since, so a `TAKEN_OVER`
    /// on the link this client is holding may be our own doing.
    ///
    /// A latch and not `in_flight.is_some()`, because the two come apart in
    /// exactly the case that matters: an attempt can get far enough for the
    /// host to adopt it -- which is what closes the old link -- and then fail,
    /// so the failure is delivered, the attempt is over, and the close is
    /// still on its way.
    displacing: bool,
}

impl Rebuild {
    pub(crate) fn new(target: String, session_id: String) -> Rebuild {
        Rebuild {
            target,
            session_id,
            launcher: SshLauncher::ssh(),
            cfg: NetConfig::default(),
            in_flight: None,
            displacing: false,
        }
    }

    /// Run attempts through `launcher` and `cfg` instead of real ssh and the
    /// default network configuration.
    ///
    /// `#[cfg(test)]` rather than an `#[allow(dead_code)]`: nothing in the
    /// shipped binary rebuilds through anything but `ssh`.
    #[cfg(test)]
    pub(crate) fn via(mut self, launcher: SshLauncher, cfg: NetConfig) -> Rebuild {
        self.launcher = launcher;
        self.cfg = cfg;
        self
    }

    /// Is an attempt running right now?
    pub(crate) fn is_running(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Could a `TAKEN_OVER` on the current link be this client's own rebuild?
    ///
    /// True from the moment an attempt starts until a swap, and it stays true
    /// across a failed attempt on purpose -- see `displacing`.
    pub(crate) fn may_have_displaced_us(&self) -> bool {
        self.displacing
    }

    /// A rebuilt link is in place, so nothing still outstanding can displace
    /// us out of a link we did not build.
    pub(crate) fn swapped(&mut self) {
        self.displacing = false;
    }

    /// Start one, reporting its outcome on `outcomes`.
    pub(crate) fn begin(
        &mut self,
        size: TermSize,
        outcomes: tokio::sync::mpsc::Sender<AttemptOutcome>,
    ) {
        let launcher = self.launcher.clone();
        let target = self.target.clone();
        let session_id = self.session_id.clone();
        let cfg = self.cfg.clone();
        self.displacing = true;
        self.in_flight = Some(tokio::spawn(async move {
            let outcome = attempt(&launcher, &target, &session_id, size, &cfg).await;
            // A closed receiver means the session this was for has ended.
            // There is nobody to tell, and that is not a failure.
            let _ = outcomes.send(outcome).await;
        }));
    }

    /// The attempt reported back, so there is nothing left to hold.
    pub(crate) fn finished(&mut self) {
        self.in_flight = None;
    }

    /// Give up on the attempt in flight, if there is one.
    ///
    /// `abort` and not merely dropping the handle: dropping a `JoinHandle`
    /// DETACHES the task, which would leave the attempt running, its ssh
    /// alive, and -- if it went on to land -- a swap performed on a session
    /// that had already come back by itself. Aborting drops the task's
    /// `SshChannel`, and `SshChannel::open` spawns with `kill_on_drop`, so the
    /// ssh goes with it.
    pub(crate) fn cancel(&mut self) {
        if let Some(task) = self.in_flight.take() {
            task.abort();
        }
    }
}

impl Drop for Rebuild {
    /// An attempt does not outlive the session that wanted it.
    ///
    /// The loop cancels on every path it takes itself, so this is about the
    /// paths it does not take: the quit key pressed while `Recovering` --
    /// which is the key the notice on screen is offering at that exact moment
    /// -- the shell exiting mid-attempt, and any error returned out of
    /// `run_on`. Today all of those end with the runtime being dropped
    /// normally, which happens to take the task down with it. That is one
    /// `std::process::exit` away from leaving somebody an ssh nobody reaps,
    /// and this makes the guarantee belong to the type instead.
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Which side of the split (spec §5.4) a failure falls on.
fn classify(target: &str, err: &anyhow::Error) -> AttemptOutcome {
    // The far end answered, and the answer was no: the session is gone, or its
    // socket did not respond within `ATTACH_TIMEOUT`. Its own words are the
    // only explanation anybody has, so they are what the user gets.
    //
    // `chain()` rather than `downcast_ref` on the error itself: this refusal
    // reaches us from three different depths inside `establish`, and any
    // `.context()` added on the way out would hide a top-level downcast.
    if let Some(refused) = err.chain().find_map(|e| e.downcast_ref::<HostRefused>()) {
        return AttemptOutcome::Definite(refused.0.clone());
    }

    // A far end whose `oxutrm host` predates `--connect` rejects the option and
    // exits; ssh reports that as a non-zero exit with the usage error on
    // stderr, and `diagnose` has no way to tell it from any other remote
    // failure (spec §2.3). The remote's own sentence is the marker, because it
    // is the only thing that distinguishes them.
    //
    // Being wrong in this direction is the safe one: the same sentence out of
    // a local `ssh` would mean the command line oxutrm built is not one this
    // ssh accepts, which is also not fixed by asking again every eight
    // seconds.
    if let Some(BootstrapError::SshFailed { stderr, .. }) =
        err.chain().find_map(|e| e.downcast_ref::<BootstrapError>())
        && stderr.to_lowercase().contains("unknown option")
    {
        return AttemptOutcome::Definite(format!(
            "the oxutrm on {target} does not understand `oxutrm host --connect`, \
             so it is too old to reconnect to; both ends have to be upgraded \
             together. It said: {stderr}"
        ));
    }

    // ssh dying, no reply, the ladder finding no rung, the QUIC handshake
    // failing: all of it is a network that may be back in eight seconds.
    // `{:#}` keeps the whole context chain on one line, which is what the
    // notice has room for.
    AttemptOutcome::Retry(format!("{err:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    use oxutrm_host::ssh::SshLauncher;
    use oxutrm_proto::TermSize;

    /// STUN-free, like every other test in this repo: a test that reaches STUN
    /// makes every timing in it non-deterministic, and an injected bug once
    /// passed because a real gather ate the ordering.
    fn test_config() -> oxutrm_net::NetConfig {
        oxutrm_net::NetConfig {
            stun_servers: vec![],
            enable_port_mapping: false,
            enable_birthday: false,
            ..Default::default()
        }
    }

    fn a_size() -> TermSize {
        TermSize { cols: 80, rows: 24 }
    }

    /// A `/bin/sh` script standing in for `ssh <target> oxutrm host --connect`,
    /// and the directory it lives in.
    ///
    /// A script rather than a compiled fixture because the root crate has no
    /// `[lib]` target: `crates/oxutrm-host/src/bin/oxutrm-fake-ssh.rs` is
    /// reachable from that crate's integration tests through
    /// `env!("CARGO_BIN_EXE_...")`, and these tests are unit tests of a private
    /// module in the binary crate, which cannot depend on a fixture binary of
    /// its own. Everything each of these fakes has to do is three lines of
    /// shell, and it is driven as a real subprocess over real pipes for the
    /// same reason `ssh_bootstrap.rs` gives: the pipes are a large part of what
    /// can go wrong here.
    ///
    /// The directory is held for the life of the fake. Dropping it takes the
    /// script -- and anything the script recorded -- with it.
    struct FakeHost {
        dir: tempfile::TempDir,
        launcher: SshLauncher,
    }

    impl FakeHost {
        /// Write `body` as an executable script in `dir` and point a launcher
        /// at it.
        fn new(dir: tempfile::TempDir, body: &str) -> FakeHost {
            use std::os::unix::fs::PermissionsExt as _;

            let script = dir.path().join("fake-ssh");
            std::fs::write(&script, body).expect("writing the fake host script");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("making the fake host script executable");
            let launcher = SshLauncher::command(&script);
            FakeHost { dir, launcher }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }
    }

    /// The offer, and then the host's own refusal once it has seen the choice.
    ///
    /// This is the shape `oxutrm host --connect` actually has: the refusal is
    /// an answer TO the choice (`main.rs`'s `run_host_connect` checks the
    /// registry only after reading it), so a fake that refused before reading
    /// would exercise a message ordering the real host never produces.
    fn fake_host_replying_failed(reason: &str) -> FakeHost {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let body = format!(
            "#!/bin/sh\n\
             printf '%s\\n' '{{\"t\":\"Sessions\",\"sessions\":[]}}'\n\
             read -r choice\n\
             printf '%s\\n' '{{\"t\":\"Failed\",\"reason\":\"{reason}\"}}'\n"
        );
        FakeHost::new(dir, &body)
    }

    /// `ssh` itself failing: it exits non-zero having said why on stderr, and
    /// nothing ever appears on stdout.
    fn fake_host_that_exits_immediately() -> FakeHost {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let body = "#!/bin/sh\n\
                    echo 'ssh: connect to host bastion.example.net port 22: \
                    Network is unreachable' >&2\n\
                    exit 255\n";
        FakeHost::new(dir, body)
    }

    /// A far end whose `oxutrm host` predates `--connect`: its own dispatcher
    /// rejects the option and exits, which reaches us as ssh exiting non-zero
    /// with that usage error on stderr. Word for word what `run_host` prints.
    fn fake_host_rejecting_the_option() -> FakeHost {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let body = "#!/bin/sh\n\
                    echo 'oxutrm host: unknown option \"--connect\"' >&2\n\
                    echo 'Try `oxutrm host --help`.' >&2\n\
                    exit 2\n";
        FakeHost::new(dir, body)
    }

    /// The offer, and then whatever the client answered it with, written to
    /// `choice.json` exactly as it arrived on the wire.
    fn fake_host_recording_the_choice() -> FakeHost {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let record = dir.path().join("choice.json");
        let body = format!(
            "#!/bin/sh\n\
             printf '%s\\n' '{{\"t\":\"Sessions\",\"sessions\":[]}}'\n\
             read -r choice\n\
             printf '%s' \"$choice\" > '{}'\n",
            record.display()
        );
        FakeHost::new(dir, &body)
    }

    async fn run_attempt(fake: &FakeHost, session_id: &str) -> AttemptOutcome {
        attempt(
            &fake.launcher,
            "bastion.example.net",
            session_id,
            a_size(),
            &test_config(),
        )
        .await
    }

    async fn attempt_against(fake: FakeHost, session_id: &str) -> AttemptOutcome {
        run_attempt(&fake, session_id).await
    }

    /// [`attempt_against`], reporting the `Choice` the far end was sent rather
    /// than what the attempt returned.
    ///
    /// The fake is borrowed rather than consumed, because dropping it deletes
    /// the directory the recording is in -- which is how the first run of this
    /// test failed, on a missing file rather than on the choice.
    async fn attempt_against_recording(fake: FakeHost, session_id: &str) -> serde_json::Value {
        let record = fake.path("choice.json");
        let _ = run_attempt(&fake, session_id).await;
        let line = std::fs::read_to_string(&record).expect("the fake host recorded no choice");
        let signal: serde_json::Value =
            serde_json::from_str(line.trim()).expect("the choice is not JSON");
        signal
            .get("choice")
            .cloned()
            .unwrap_or_else(|| panic!("the answer to the offer was not a Choose: {signal}"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_host_that_says_the_session_is_gone_is_definite() {
        // The distinction the whole loop rests on: a transport failure is worth
        // retrying for ever, and an answer from the far end is not. Retrying a
        // Failed would spawn ssh every eight seconds until the user noticed.
        let outcome = attempt_against(
            fake_host_replying_failed("no such session on this host"),
            "abc123",
        )
        .await;
        match outcome {
            AttemptOutcome::Definite(reason) => {
                assert!(reason.contains("no such session"), "{reason}")
            }
            other => panic!("expected Definite, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_ssh_that_dies_is_retried() {
        match attempt_against(fake_host_that_exits_immediately(), "abc123").await {
            // The reason, and not merely the variant: `Retry` is what
            // `classify` returns for everything that is not one of the two
            // definite cases, so a `classify` that had stopped reading its
            // argument at all would satisfy `Retry(_)`. What this has to show
            // is that the sentence reaching the notice is the one ssh gave.
            AttemptOutcome::Retry(why) => assert!(
                why.contains("Network is unreachable"),
                "the reason ssh gave was thrown away: {why}"
            ),
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    /// A far end too old to know `--connect` will never learn it by being
    /// asked again. Spec 2.3: both ends have to be upgraded together, and
    /// saying so beats an ssh every eight seconds for ever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_remote_that_rejects_the_option_is_definite() {
        match attempt_against(fake_host_rejecting_the_option(), "abc123").await {
            AttemptOutcome::Definite(reason) => {
                assert!(
                    reason.contains("bastion.example.net"),
                    "must name the host to upgrade: {reason}"
                );
                assert!(
                    reason.contains("unknown option"),
                    "the far end's own words are the whole explanation: {reason}"
                );
            }
            other => panic!("expected Definite, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_attempt_asks_for_the_session_it_came_from() {
        // The side effect, not the return value: a rebuild that answered `New`
        // would silently start a second shell and look like it had worked.
        let recorded = attempt_against_recording(fake_host_recording_the_choice(), "abc123").await;
        assert_eq!(recorded, serde_json::json!({"c": "Attach", "id": "abc123"}));
    }
}

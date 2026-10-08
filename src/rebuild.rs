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

// A rebuild attempt runs while the client session it is rebuilding already
// owns the screen: nothing here may print, or it lands raw on the painted
// raw-mode terminal. `AttemptOutcome::Retry`'s reason is shown
// through the popup, not printed.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use oxutrm_host::ssh::{BootstrapError, SshChannel, SshLauncher};
use oxutrm_net::NetConfig;
use oxutrm_proto::{Choice, Signal, TermSize};

use crate::connect::{Established, HostRefused, establish, read_offer};

/// A rebuild's ssh must never stop to ask a question.
///
/// Raw mode is held and the screen belongs to the renderer, so a passphrase
/// prompt would fight it for the terminal. `BatchMode=yes` turns that into a
/// clean failure the popup can explain instead. The real answer is B3's
/// askpass; until then this is the deliberate, legible degradation the spec
/// asks for.
const BATCH_MODE: [&str; 2] = ["-o", "BatchMode=yes"];

/// A bound on ssh's TCP connect, for a rebuild whose ssh has none of its own.
///
/// ssh's default is no timeout at all, which leaves the bound to the
/// operating system. On macOS that is 75 s. On 2026-10-04 a VPN dropped,
/// the rebuild started 20 s in, and its target was only reachable over that
/// VPN: the SYN went nowhere, and ssh sat out the full 75 s while the
/// standby, which answered a second after ssh gave up, was not asked. The
/// outage lasted 96 s.
///
/// **Ten seconds**, and not the two or three a LAN would allow: a VPN that
/// comes back takes several seconds to settle its routes, and an attempt
/// that times out just before the route appears costs a whole backoff. Ten
/// still turns the 75 s wait into the next attempt.
///
/// Added only where the user's ssh has no `ConnectTimeout` of its own (see
/// [`needs_connect_timeout`]): one somebody set is their call, not ours.
///
/// The default of `recovery.connect_timeout`; a session's own value is
/// [`Rebuild::retune`]'s, and every attempt carries it in its [`Ties`].
pub(crate) const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long `ssh -G` may take to say what ssh would do.
///
/// It reads config files and connects to nothing, so it takes milliseconds;
/// this bound is for the configuration that runs something (a `Match exec`
/// that hangs). Running out is treated as not knowing, which adds
/// the connect timeout.
const CONFIG_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether a rebuild's ssh needs the connect timeout, given what `ssh -G`
/// printed -- `None` when it failed or ran out of time.
///
/// `connecttimeout none` is the default, and the case this is for. A number
/// is the user's own setting, and wins. Anything else -- no answer, no such
/// line, a value this does not understand -- is not knowing, and an ssh
/// that may wait 75 s on a dead route is the thing not to risk.
fn needs_connect_timeout(ssh_g_output: Option<&str>) -> bool {
    let Some(output) = ssh_g_output else {
        return true;
    };
    let value = output.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        let keyword = words.next()?;
        keyword
            .eq_ignore_ascii_case("connecttimeout")
            .then(|| words.next())
            .flatten()
    });
    !value.is_some_and(|v| v.parse::<u64>().is_ok())
}

/// The launcher a rebuild attempt runs ssh through: `launcher` with
/// [`BATCH_MODE`], and the connect timeout where the user's ssh would
/// otherwise have no bound on its connect -- asked of `ssh -G` the first
/// time, and of `bound` once ssh has answered (see [`ConnectBound`]).
///
/// Asked of the same launcher, so production asks `ssh -G` and a test's fake
/// ssh is asked the same question, and the argument list the tests see is
/// the one that ships.
async fn rebuild_launcher(
    launcher: &SshLauncher,
    target: &str,
    bound: &ConnectBound,
    connect_timeout: std::time::Duration,
) -> SshLauncher {
    let launcher = BATCH_MODE
        .iter()
        .fold(launcher.clone(), |batch, arg| batch.arg(arg));
    let needed = match bound.0.get() {
        Some(&known) => known,
        None => {
            let config =
                tokio::time::timeout(CONFIG_QUERY_TIMEOUT, launcher.effective_config(target))
                    .await
                    .ok()
                    .and_then(Result::ok);
            let needed = needs_connect_timeout(config.as_deref());
            if config.is_some() {
                // One attempt runs at a time, so nobody else can have set
                // it meanwhile; and if somebody had, theirs is as good.
                let _ = bound.0.set(needed);
            }
            needed
        }
    };
    if needed {
        // Whole seconds: `recovery.connect_timeout` refuses anything finer.
        launcher
            .arg("-o")
            .arg(format!("ConnectTimeout={}", connect_timeout.as_secs()))
    } else {
        launcher
    }
}

/// The outer bound on one whole attempt.
///
/// Nothing inside [`attempt`] bounds the wait for a far end that accepts the
/// connection and then says nothing -- a hung registry read, a stalled NFS
/// home directory, an ssh that connected and never ran the command. Without
/// this the task stays alive for ever, `Rebuild::is_running` stays true, the
/// loop starts no further attempt, and the popup's `ssh rebuild` row counts
/// one attempt's running time up for ever. The feature dead-ends at the
/// exact moment it is needed, and the link it would otherwise fall back on
/// is by definition the dead one.
///
/// **Two minutes, and it is deliberately the largest number in the picture.**
/// Every step inside an attempt already has the right budget for itself, and
/// this is not a second opinion about any of them: the candidate gather has
/// `NetConfig::gather_timeout` (3 s), the birthday blast 6 s,
/// `oxutrm_net::CONNECT_TIMEOUT` 30 s, and the far end applies
/// [`crate::listener::ATTACH_TIMEOUT`] -- 90 s -- to the whole exchange from
/// its own side. A cap at or below 90 s would race that one, cutting off
/// attaches the host was still legitimately working on and reporting them as
/// network failures. Sitting clear of it means this fires only where the far
/// end's own guard cannot: before the relay, in front of everything that has
/// a budget of its own.
const ATTEMPT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

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
    /// Worth trying again, with the reason kept for the popup.
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

/// One attempt's [`AttemptOutcome`], as its task sends it to the loop,
/// carrying the number [`Rebuild::begin`] gave that attempt.
///
/// The number is what lets the loop tell the attempt in flight from one it
/// has already given up. Giving up cannot unsend anything: `abort` only
/// lands at the task's next yield, and an attempt that has already finished
/// has nothing left to abort -- its outcome sits in the channel, and the
/// loop reads it on a later lap, after the abandon or stand-down that should
/// have silenced it. Read as current, a queued `Definite` ended a session
/// that had just switched to its standby or come back by itself, and a
/// queued `Retry` logged a failure, counted it and pushed the schedule back
/// for an attempt the log had already called abandoned.
///
/// Only a failure is read as stale, though. A `Landed` is the host's own
/// word that it has ADOPTED that attach, and adopting closed every other
/// link it held for the session as taken over: whatever the loop has given
/// up since, the landed link is the only one the host still knows. See
/// [`Rebuild::accept`].
pub(crate) struct Report {
    generation: u64,
    outcome: AttemptOutcome,
}

#[cfg(test)]
impl Report {
    /// The same attempt's report, had it landed `established` instead: for
    /// a session test that needs a landing from a real attempt's generation
    /// without an ssh fake that can complete an attach.
    pub(crate) fn landed_instead(self, established: Established) -> Report {
        Report {
            generation: self.generation,
            outcome: AttemptOutcome::Landed(Box::new(established)),
        }
    }
}

/// How far the attempt in flight has got, as far as the standby cares.
///
/// The line that matters is the `Attach`. The host can only adopt a rebuild
/// -- which is what closes every other link it holds for the session,
/// standby included, as taken over -- after it has read the `Choose` naming
/// it. An attempt given up before that line is invisible to the host: there
/// is no attach for it to adopt, so it cannot close a standby the client has
/// promoted in the meantime. That was the only reason for ruling B1 (no
/// failover while a rebuild runs), so B1 now holds only from the line on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RebuildStage {
    /// No attempt is in flight.
    Idle,
    /// An attempt is in flight and has not yet sent its `Attach`: ssh is
    /// still connecting, or the offer has not arrived. Abandoning it is free.
    Connecting,
    /// The `Attach` has gone, or is going, to the host. The host may adopt
    /// this attempt, so nothing may be promoted over it.
    Committed,
}

/// The line between [`RebuildStage::Connecting`] and
/// [`RebuildStage::Committed`] for one attempt, shared between the attempt's
/// task and the loop that may abandon it.
///
/// Three states and a compare-and-swap rather than a flag the task sets,
/// because the two sides race. The loop decides to fail over on one thread
/// while the attempt runs on another, and `JoinHandle::abort` only takes
/// effect at the task's next yield: a task that set a plain flag and wrote
/// its `Choose` in the same poll would send the `Attach` AFTER the loop had
/// read "not committed" and promoted the standby -- the exact adoption B1
/// exists to prevent. With one atomic that each side moves out of
/// `CONNECTING` only if the other has not, exactly one of them wins: either
/// the `Attach` goes and the failover is held off, or the failover goes and
/// the `Attach` never does.
#[derive(Clone, Debug, Default)]
pub(crate) struct Commitment(Arc<AtomicU8>);

impl Commitment {
    const CONNECTING: u8 = 0;
    const COMMITTED: u8 = 1;
    const ABANDONED: u8 = 2;

    /// The attempt's side: claim the right to send the `Attach`. False when
    /// the loop abandoned the attempt first, and then it must not.
    fn commit(&self) -> bool {
        self.claim(Self::COMMITTED)
    }

    /// The loop's side: give the attempt up before it commits. False when
    /// it has already committed.
    fn abandon(&self) -> bool {
        self.claim(Self::ABANDONED)
    }

    fn claim(&self, to: u8) -> bool {
        self.0
            .compare_exchange(Self::CONNECTING, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn is_committed(&self) -> bool {
        self.0.load(Ordering::Acquire) == Self::COMMITTED
    }
}

/// Whether a rebuild's ssh needs the connect timeout, kept once `ssh -G`
/// has given a definite answer, and shared by every attempt of one
/// [`Rebuild`].
///
/// Asked once and not per attempt, because the question is not always
/// cheap. `ssh -G` connects to nothing, but it does evaluate the
/// configuration, and a `Match exec` or a canonicalised host name can cost
/// seconds -- on every attempt, during exactly the kind of VPN drop the
/// timeout is for. The answer cannot change within one session unless
/// somebody edits their ssh configuration, and a session that outlives such
/// an edit keeps the old answer, as an ssh already running keeps its options.
///
/// Only an answer is kept. A query that failed or ran out of
/// [`CONFIG_QUERY_TIMEOUT`] is not knowing, and the next attempt asks again
/// (meanwhile this one adds the timeout, as [`needs_connect_timeout`] does
/// for `None`).
#[derive(Clone, Debug, Default)]
struct ConnectBound(Arc<OnceLock<bool>>);

/// What one attempt shares with the [`Rebuild`] that began it.
#[derive(Clone, Debug)]
pub(crate) struct Ties {
    /// This attempt's own line. A fresh one per attempt.
    commitment: Commitment,
    /// The rebuild's, shared by all its attempts.
    bound: ConnectBound,
    /// `recovery.connect_timeout` when the attempt began.
    connect_timeout: std::time::Duration,
}

impl Default for Ties {
    fn default() -> Ties {
        Ties {
            commitment: Commitment::default(),
            bound: ConnectBound::default(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }
}

/// Where a rebuild goes (switcher spec §3.5): the session the client is in,
/// or -- while it is in a lobby -- a fresh lobby. A lobby is never
/// reattached: it ended after `DETACH_AFTER` of silence, or will.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Aim {
    Session(String),
    Lobby,
}

impl Aim {
    /// What the attempt answers the offer with.
    fn choice(&self) -> Result<Choice, String> {
        match self {
            Aim::Session(id) => id
                .parse()
                .map(|id| Choice::Attach { id })
                .map_err(|_| format!("{id:?} is not a session id")),
            Aim::Lobby => Ok(Choice::Lobby),
        }
    }
}

/// One attempt at getting back to `aim` on `target`.
///
/// `launcher` is the injection point, exactly as it is for
/// [`SshChannel::open`]: production passes [`SshLauncher::ssh`] and the tests
/// point it at a script that speaks the protocol on stdio. [`BATCH_MODE`] and
/// the connect timeout are added here rather than by the caller (see
/// [`rebuild_launcher`]), so every attempt carries them and the tests
/// exercise the argument list that ships.
///
/// `size` is the client's **current** terminal size, so the host adopts at the
/// right geometry from the `ClientHello` alone with no resize afterwards.
///
/// `ties` are what the attempt shares with the [`Rebuild`] that began it:
/// its commitment, claimed immediately before the `Attach` is sent (see
/// [`Commitment`]) -- an attempt that finds it already abandoned stops there
/// -- and what `ssh -G` has already said ([`ConnectBound`]).
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
    aim: &Aim,
    size: TermSize,
    cfg: &NetConfig,
    ties: &Ties,
) -> AttemptOutcome {
    attempt_within(ATTEMPT_DEADLINE, launcher, target, aim, size, cfg, ties).await
}

/// [`attempt`], with its outer bound as a parameter.
///
/// A parameter rather than a constant read inside, for exactly the reason
/// `crate::listener::serve_attaches` takes its `attach_timeout` that way: the
/// guard can then be exercised in a fraction of a second instead of in two
/// minutes. It is not a knob -- the one production caller is [`attempt`] and
/// it passes [`ATTEMPT_DEADLINE`].
///
/// Timing out is a [`AttemptOutcome::Retry`] and not a `Definite`: a far end
/// that hung is not a far end that answered, and the loop's whole split
/// (spec §5.4) turns on that difference.
async fn attempt_within(
    deadline: std::time::Duration,
    launcher: &SshLauncher,
    target: &str,
    aim: &Aim,
    size: TermSize,
    cfg: &NetConfig,
    ties: &Ties,
) -> AttemptOutcome {
    let body = one_attempt(launcher, target, aim, size, cfg, ties);
    match tokio::time::timeout(deadline, body).await {
        Ok(outcome) => outcome,
        Err(_) => AttemptOutcome::Retry(format!(
            "the attempt to reach {target} timed out after {}s -- \
             it was accepted and then nothing came back",
            deadline.as_secs()
        )),
    }
}

/// The attempt itself, unbounded. Only [`attempt_within`] calls it, and it is
/// what that puts the clock on.
async fn one_attempt(
    launcher: &SshLauncher,
    target: &str,
    aim: &Aim,
    size: TermSize,
    cfg: &NetConfig,
    ties: &Ties,
) -> AttemptOutcome {
    let launcher = rebuild_launcher(launcher, target, &ties.bound, ties.connect_timeout).await;

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

    // The line (see `RebuildStage`). Past it the host may adopt this attempt,
    // so the standby is no longer the loop's to promote; short of it the loop
    // has already promoted it, and this attempt must leave the host alone.
    // The task got here before the loop's abort did, so this outcome may well
    // be sent -- the channel is likely empty -- and read. It is read as stale:
    // the abandon ended this attempt's generation (see `Report`).
    if !ties.commitment.commit() {
        return AttemptOutcome::Retry("abandoned for the standby".to_owned());
    }
    let choice = match aim.choice() {
        Ok(choice) => choice,
        Err(why) => return AttemptOutcome::Definite(why),
    };
    if let Err(e) = channel.send(&Signal::Choose { choice }).await {
        return classify(target, &anyhow::Error::new(e));
    }

    let (reader, writer) = channel.halves();
    match establish(reader, writer, size, cfg, None).await {
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
    aim: Aim,
    /// How to start ssh. The injection point, as in [`attempt`].
    launcher: SshLauncher,
    cfg: NetConfig,
    in_flight: Option<tokio::task::JoinHandle<()>>,
    /// The number of the most recent attempt begun, which its [`Report`]
    /// carries back. It is the attempt in flight only while `in_flight` is
    /// set: [`Rebuild::cancel`] clears that, and with it the one generation
    /// whose report is still wanted.
    generation: u64,
    /// Whether the attempt in flight has sent its `Attach`. A fresh one per
    /// attempt, so an old attempt's line can never be read as the new one's.
    commitment: Commitment,
    /// What `ssh -G` said, once it has said it: one answer for every attempt
    /// of this rebuild ([`ConnectBound`]).
    bound: ConnectBound,
    /// `recovery.connect_timeout`, for the next attempt.
    connect_timeout: std::time::Duration,
    /// When the attempt in flight began, for the popup's clock on it.
    started: Option<Instant>,
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
        Rebuild::aimed(target, Aim::Session(session_id))
    }

    /// A rebuild that goes to `aim`.
    pub(crate) fn aimed(target: String, aim: Aim) -> Rebuild {
        Rebuild {
            target,
            aim,
            launcher: SshLauncher::ssh(),
            cfg: NetConfig::default(),
            in_flight: None,
            generation: 0,
            commitment: Commitment::default(),
            bound: ConnectBound::default(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            started: None,
            displacing: false,
        }
    }

    /// Where the next attempt goes, after a switch, a new session or a kill
    /// moved the client (switcher spec §3.5).
    ///
    /// An attempt already running for another aim is cancelled: it would
    /// reach a session that is gone (`Killed` of this one, which ends the
    /// client as a definite failure) or a fresh lobby the client is no longer
    /// in (`Started`). The next one goes where the client now is, after the
    /// backoff the cancelled one began. The displacing latch is left as it
    /// is: the outage is not over, and an attempt that had already sent its
    /// `Attach` may still be adopted ([`Rebuild::may_have_displaced_us`]).
    pub(crate) fn retarget(&mut self, aim: Aim) {
        if aim != self.aim {
            self.cancel();
        }
        self.aim = aim;
    }

    /// Where the next attempt goes.
    #[cfg(test)]
    pub(crate) fn aim(&self) -> &Aim {
        &self.aim
    }

    /// The network settings and the connect timeout the next attempt runs
    /// with, from `apply`. An attempt already running keeps its own.
    pub(crate) fn retune(&mut self, cfg: NetConfig, connect_timeout: std::time::Duration) {
        self.cfg = cfg;
        self.connect_timeout = connect_timeout;
    }

    /// The network settings the next attempt runs with.
    #[cfg(test)]
    pub(crate) fn cfg(&self) -> &NetConfig {
        &self.cfg
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

    /// How far the attempt in flight has got (see [`RebuildStage`]).
    pub(crate) fn stage(&self) -> RebuildStage {
        if self.in_flight.is_none() {
            RebuildStage::Idle
        } else if self.commitment.is_committed() {
            RebuildStage::Committed
        } else {
            RebuildStage::Connecting
        }
    }

    /// Give up the attempt in flight for a standby that answered, if it has
    /// not committed. Returns whether it was given up; false when there was
    /// none or it has already sent its `Attach`, and then nothing changes.
    ///
    /// Not [`Rebuild::stood_down`]: the displacing latch stays as it is. An
    /// EARLIER attempt in this same outage may have committed before it
    /// failed, and the host may still adopt it and close the standby being
    /// promoted now as taken over; that close has to go on being read as our
    /// own doing.
    pub(crate) fn abandon(&mut self) -> bool {
        if self.in_flight.is_none() || !self.commitment.abandon() {
            return false;
        }
        self.cancel();
        true
    }

    /// When the attempt now running began; `None` while none is.
    pub(crate) fn running_since(&self) -> Option<Instant> {
        self.in_flight.as_ref().and(self.started)
    }

    /// Could a `TAKEN_OVER` on the current link be this client's own rebuild?
    ///
    /// True from the moment an attempt starts until the OUTAGE ENDS, however
    /// it ends: [`Rebuild::swapped`] when a rebuilt link took over, and
    /// [`Rebuild::stood_down`] when the old link came back by itself. It stays
    /// true across a failed attempt on purpose -- see `displacing` -- but it
    /// must not outlive the outage, or a genuine third-party takeover hours
    /// later would be excused as our own doing.
    pub(crate) fn may_have_displaced_us(&self) -> bool {
        self.displacing
    }

    /// A rebuilt link is in place, so nothing still outstanding can displace
    /// us out of a link we did not build -- unless an attempt is still in
    /// flight, which can only be when the link that landed was an older
    /// attempt's ([`Rebuild::accept`]). The one running may land too, and
    /// its adopt closes the link just swapped in as taken over. Defensive:
    /// it needs a stale landing left unread across a whole new outage, which
    /// the loop's one-deep outcome channel, read every lap, all but rules out.
    pub(crate) fn swapped(&mut self) {
        self.displacing = self.in_flight.is_some();
    }

    /// The outage ended on the OLD link, so the attempt is given up and
    /// nothing it started can have displaced us.
    ///
    /// The two halves are one operation on purpose: cancelling without
    /// dropping the latch is what left `displacing` set for the whole
    /// remaining session, and a `TAKEN_OVER` arriving hours later -- somebody
    /// else genuinely attaching -- was then attributed to a rebuild that ended
    /// long ago. The client sat on a connection the host had closed, showing a
    /// screen that would never change again.
    ///
    /// **Why dropping the latch here is safe.** Reaching this needs the phase
    /// to have left `Recovering`, and only a FRAME on the old link does that.
    /// A host that had adopted a rebuilt link would have closed this one
    /// instead of sending on it, so a frame is proof that no adopt had
    /// happened WHEN THE HOST SENT IT. Not that none happens afterwards: an
    /// attempt whose `Attach` was already on its way can still be adopted,
    /// and its landing, read after this, is taken all the same
    /// ([`Rebuild::accept`]).
    pub(crate) fn stood_down(&mut self) {
        self.cancel();
        self.displacing = false;
    }

    /// Start one at `now`, reporting its outcome on `outcomes`.
    pub(crate) fn begin(
        &mut self,
        size: TermSize,
        outcomes: tokio::sync::mpsc::Sender<Report>,
        now: Instant,
    ) {
        self.started = Some(now);
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let launcher = self.launcher.clone();
        let target = self.target.clone();
        let aim = self.aim.clone();
        let cfg = self.cfg.clone();
        let ties = Ties {
            commitment: Commitment::default(),
            bound: self.bound.clone(),
            connect_timeout: self.connect_timeout,
        };
        self.commitment = ties.commitment.clone();
        self.displacing = true;
        self.in_flight = Some(tokio::spawn(async move {
            let outcome = attempt(&launcher, &target, &aim, size, &cfg, &ties).await;
            // A closed receiver means the session this was for has ended.
            // There is nobody to tell, and that is not a failure.
            let _ = outcomes
                .send(Report {
                    generation,
                    outcome,
                })
                .await;
        }));
    }

    /// An attempt reported back: its outcome, for the loop to act on, if it
    /// is the attempt in flight or if it landed; `None` for a failure of an
    /// attempt the loop has given up since (see [`Report`]), and then
    /// nothing changes.
    ///
    /// Before [`Rebuild::finished`], which is what would otherwise make an
    /// old report look like the end of the attempt now running.
    ///
    /// A given-up attempt that LANDED is handed on all the same, because the
    /// host has already moved to its link: `HostSession::adopt` closes the
    /// link it displaces as taken over the moment it adopts, before the
    /// attempt can even read its answer. Only a stand-down can leave one --
    /// an attempt is abandoned for the standby only before its `Attach`, and
    /// an attempt that never sent one cannot land -- and a stand-down means
    /// a frame on the old link that the host sent BEFORE it adopted. Closing
    /// the landed link instead, as this once did, left the client on the
    /// link the host had just closed, and the session ended as "taken over"
    /// a moment after it had come back.
    ///
    /// A stale landing leaves any attempt in flight now running: that one
    /// may yet land too, and then has to be heard from to be swapped in.
    pub(crate) fn accept(&mut self, report: Report) -> Option<AttemptOutcome> {
        if self.in_flight.is_some() && report.generation == self.generation {
            self.finished();
            return Some(report.outcome);
        }
        match report.outcome {
            landed @ AttemptOutcome::Landed(_) => Some(landed),
            AttemptOutcome::Retry(_) | AttemptOutcome::Definite(_) => None,
        }
    }

    /// The attempt reported back, so there is nothing left to hold.
    fn finished(&mut self) {
        self.in_flight = None;
        self.started = None;
    }

    /// Give up on the attempt in flight, if there is one. Its report, if it
    /// has already sent one, is no longer wanted ([`Rebuild::accept`]).
    ///
    /// `abort` and not merely dropping the handle: dropping a `JoinHandle`
    /// DETACHES the task, which would leave the attempt running, its ssh
    /// alive, and -- if it went on to land -- a swap performed on a session
    /// that had already come back by itself. Aborting drops the task's
    /// `SshChannel`, and `SshChannel::open` spawns with `kill_on_drop`, so the
    /// ssh goes with it.
    pub(crate) fn cancel(&mut self) {
        self.started = None;
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
    /// which is the key the popup is offering at that exact moment
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
    // popup has room for.
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
        /// at it. Asked `ssh -G`, it says `connecttimeout none`, as an ssh
        /// with no configuration of its own does.
        fn new(dir: tempfile::TempDir, body: &str) -> FakeHost {
            FakeHost::configured(dir, "connecttimeout none", body)
        }

        /// [`FakeHost::new`], answering `ssh -G` with `config` instead.
        ///
        /// Every attempt asks `ssh -G` first (see `rebuild_launcher`), and a
        /// fake that did not answer it would run `body` for the question as
        /// well: one that hangs would cost every test the whole
        /// `CONFIG_QUERY_TIMEOUT`, and one that records something would
        /// record the question too.
        fn configured(dir: tempfile::TempDir, config: &str, body: &str) -> FakeHost {
            use std::os::unix::fs::PermissionsExt as _;

            let body = body
                .strip_prefix("#!/bin/sh\n")
                .expect("a fake host is a /bin/sh script");
            let text = format!(
                "#!/bin/sh\n\
                 case \" $* \" in *' -G '*) printf '%s\\n' '{config}'; exit 0;; esac\n\
                 {body}"
            );
            let script = dir.path().join("fake-ssh");
            std::fs::write(&script, text).expect("writing the fake host script");
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
             printf '%s\\n' '{{\"t\":\"Sessions\",\"list\":[]}}'\n\
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

    /// A far end that accepts the connection and then says nothing at all.
    ///
    /// The shape of a hung registry read or a stalled home directory: stdout
    /// stays open, so there is no EOF for `SshChannel::recv` to diagnose, and
    /// the read simply never returns. `exec` rather than a plain `sleep`, so
    /// the process the launcher spawned IS the sleeper and `kill_on_drop`
    /// reaches it -- a `sleep` left as a child of the shell would outlive the
    /// test as an orphan.
    fn fake_host_that_never_answers() -> FakeHost {
        let dir = tempfile::tempdir().expect("a scratch directory");
        FakeHost::new(dir, "#!/bin/sh\nexec sleep 300\n")
    }

    /// The offer, and then whatever the client answered it with, written to
    /// `choice.json` exactly as it arrived on the wire.
    fn fake_host_recording_the_choice() -> FakeHost {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let record = dir.path().join("choice.json");
        let body = format!(
            "#!/bin/sh\n\
             printf '%s\\n' '{{\"t\":\"Sessions\",\"list\":[]}}'\n\
             read -r choice\n\
             printf '%s' \"$choice\" > '{}'\n",
            record.display()
        );
        FakeHost::new(dir, &body)
    }

    async fn run_attempt(fake: &FakeHost, session_id: &str) -> AttemptOutcome {
        run_attempt_to(fake, &Aim::Session(session_id.to_owned())).await
    }

    async fn run_attempt_to(fake: &FakeHost, aim: &Aim) -> AttemptOutcome {
        attempt(
            &fake.launcher,
            "bastion.example.net",
            aim,
            a_size(),
            &test_config(),
            &Ties::default(),
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
            "3ff1218f5e0c4b7d9a1c2e3f40516273",
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
        match attempt_against(
            fake_host_that_exits_immediately(),
            "3ff1218f5e0c4b7d9a1c2e3f40516273",
        )
        .await
        {
            // The reason, and not merely the variant: `Retry` is what
            // `classify` returns for everything that is not one of the two
            // definite cases, so a `classify` that had stopped reading its
            // argument at all would satisfy `Retry(_)`. What this has to show
            // is that the sentence reaching the popup is the one ssh gave.
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
        match attempt_against(
            fake_host_rejecting_the_option(),
            "3ff1218f5e0c4b7d9a1c2e3f40516273",
        )
        .await
        {
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

    /// A far end that hangs must not hang the loop.
    ///
    /// Nothing inside an attempt bounds the wait for a far end that accepted
    /// the connection and then said nothing, so before [`ATTEMPT_DEADLINE`]
    /// this task stayed alive for ever: `is_running()` never cleared,
    /// `rebuild_step` started no further attempt, and the notice sat at "next
    /// try in 0s" for the rest of the session -- the feature dead-ending at
    /// the exact moment it is needed.
    ///
    /// The OUTER timeout is what makes a regression fail rather than hang:
    /// with the deadline taken back out, the attempt returns nothing at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_far_end_that_never_answers_is_given_up_on_and_retried() {
        let fake = fake_host_that_never_answers();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            attempt_within(
                std::time::Duration::from_millis(200),
                &fake.launcher,
                "bastion.example.net",
                &Aim::Session("3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned()),
                a_size(),
                &test_config(),
                &Ties::default(),
            ),
        )
        .await
        .expect("an attempt against a far end that never answers hung for ever");

        match outcome {
            // The sentence, not merely the variant: `Retry` is what
            // `classify` returns for nearly everything that can go wrong, so
            // `Retry(_)` would be satisfied by the fake failing to start at
            // all. Only the deadline's own branch writes "timed out after".
            //
            // And `Retry` rather than `Definite`: a far end that hung did not
            // answer, and ending the loop on it would strand a session that
            // asking again might well recover.
            AttemptOutcome::Retry(why) => {
                assert!(
                    why.contains("timed out after"),
                    "the deadline was not what ended this: {why}"
                );
                assert!(
                    why.contains("bastion.example.net"),
                    "the popup has to name what could not be reached: {why}"
                );
            }
            other => panic!("expected a Retry naming the deadline, got {other:?}"),
        }
    }

    /// The offer, and then whatever the client answered it with recorded in
    /// `choice.json` -- after which the far end hangs, as a host still working
    /// on the attach would. `exec` for the reason
    /// [`fake_host_that_never_answers`] gives.
    fn fake_host_that_takes_the_choice_and_hangs() -> FakeHost {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let record = dir.path().join("choice.json");
        let body = format!(
            "#!/bin/sh\n\
             printf '%s\\n' '{{\"t\":\"Sessions\",\"list\":[]}}'\n\
             read -r choice\n\
             printf '%s' \"$choice\" > '{}'\n\
             exec sleep 300\n",
            record.display()
        );
        FakeHost::new(dir, &body)
    }

    /// A `Rebuild` that runs its attempts against `fake`, with one begun.
    fn rebuilding_against(fake: &FakeHost) -> Rebuild {
        let mut rebuild = Rebuild::new(
            "bastion.example.net".to_owned(),
            "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
        )
        .via(fake.launcher.clone(), test_config());
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        rebuild.begin(a_size(), tx, Instant::now());
        rebuild
    }

    /// An attempt whose ssh never gets as far as the offer has sent no
    /// `Attach`, so the host cannot adopt it and the standby may be promoted
    /// over it: it stays `Connecting` for as long as it hangs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_attempt_that_hangs_before_the_offer_never_commits() {
        let fake = fake_host_that_never_answers();
        let mut rebuild = rebuilding_against(&fake);
        assert_eq!(rebuild.stage(), RebuildStage::Connecting);

        // Long enough for a real subprocess to have started and hung; a
        // commit that did not wait for the offer would have happened by now.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            rebuild.stage(),
            RebuildStage::Connecting,
            "an attempt that never saw an offer counts as having sent an Attach"
        );

        assert!(
            rebuild.abandon(),
            "an uncommitted attempt was not abandoned"
        );
        assert_eq!(rebuild.stage(), RebuildStage::Idle);
        assert!(!rebuild.is_running());
    }

    /// The line is the `Attach`: once the far end has the choice in hand, the
    /// attempt is `Committed`, and it can no longer be abandoned for the
    /// standby -- the host may be adopting it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_attempt_commits_once_it_sends_its_attach() {
        let fake = fake_host_that_takes_the_choice_and_hangs();
        let record = fake.path("choice.json");
        let mut rebuild = rebuilding_against(&fake);

        // The far end having READ the choice, which is later than the
        // client claiming the line, so the stage is settled by then.
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !std::fs::read_to_string(&record).is_ok_and(|c| !c.is_empty()) {
            assert!(Instant::now() < deadline, "the attempt never sent a choice");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(rebuild.stage(), RebuildStage::Committed);

        assert!(
            !rebuild.abandon(),
            "an attempt the host may adopt was abandoned for the standby"
        );
        assert_eq!(
            rebuild.stage(),
            RebuildStage::Committed,
            "a refused abandon changed the attempt anyway"
        );
        rebuild.cancel();
        assert_eq!(rebuild.stage(), RebuildStage::Idle);
    }

    /// A link landed by the attempt `generation` named, as its report.
    async fn a_landed_report(generation: u64) -> (Report, crate::link::Link) {
        let (far, near) = crate::link::fixtures::link_pair().await;
        let established = Established {
            link: near,
            path: oxutrm_proto::PathDescription {
                rung: oxutrm_proto::Rung::StunPunch,
                local: "127.0.0.1:1".parse().expect("an address"),
                remote: "203.0.113.7:443".parse().expect("an address"),
                probes_sent: 0,
                nat_type: oxutrm_proto::NatType::Unknown,
                rtt_ms: 38,
                mtu: 1400,
            },
            session_id: "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
            attach_id: 2,
            host_features: vec![],
        };
        let report = Report {
            generation,
            outcome: AttemptOutcome::Landed(Box::new(established)),
        };
        (report, far)
    }

    /// Only the attempt in flight is heard from when it FAILED. A `Retry` or
    /// `Definite` queued by an attempt the loop has since given up --
    /// abandoned for the standby, or stood down because the old link came
    /// back -- is not handed on, and does not end the attempt that is in
    /// flight now.
    ///
    /// A `Landed` is handed on whatever its generation. The host adopted
    /// that attach before the attempt could report it, and adopting closed
    /// every other link the host held for the session as taken over: the
    /// landed link is the only one the host still knows. Closing it instead,
    /// as this once did, left the client on a link the host had just closed,
    /// and the session ended as "taken over" a moment after it was rescued.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn only_the_attempt_in_flight_is_heard_from() {
        let fake = fake_host_that_never_answers();
        let mut rebuild = rebuilding_against(&fake);
        let given_up = rebuild.generation;
        rebuild.stood_down();

        let (stale, far) = a_landed_report(given_up).await;
        assert!(
            matches!(rebuild.accept(stale), Some(AttemptOutcome::Landed(_))),
            "a link the host has already adopted was not handed on, because \
             the attempt that built it had been given up"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(300),
                far.sink.connection().closed(),
            )
            .await
            .is_err(),
            "the landed link was closed under the host that adopted it"
        );

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        rebuild.begin(a_size(), tx, Instant::now());
        for old in [
            AttemptOutcome::Definite("the session is gone".to_owned()),
            AttemptOutcome::Retry("ssh exited".to_owned()),
        ] {
            let old = Report {
                generation: given_up,
                outcome: old,
            };
            assert!(
                rebuild.accept(old).is_none(),
                "a stale failure was handed on"
            );
            assert!(
                rebuild.is_running(),
                "a stale failure ended the attempt in flight"
            );
        }

        let (stale, _far) = a_landed_report(given_up).await;
        assert!(
            matches!(rebuild.accept(stale), Some(AttemptOutcome::Landed(_))),
            "a stale landing was not handed on with another attempt in flight"
        );
        assert!(
            rebuild.is_running(),
            "a stale landing ended the attempt in flight, which may still \
             land itself and then never be heard from"
        );

        let current = Report {
            generation: rebuild.generation,
            outcome: AttemptOutcome::Retry("ssh exited".to_owned()),
        };
        assert!(
            matches!(rebuild.accept(current), Some(AttemptOutcome::Retry(_))),
            "the attempt in flight was not heard from"
        );
        assert!(!rebuild.is_running());
    }

    /// The offer, from a far end that has already stopped reading: its
    /// stdin is closed before the offer goes out, so the `Choose` cannot be
    /// written at all, and it then hangs with stdout open. `exec` for the
    /// reason [`fake_host_that_never_answers`] gives.
    fn fake_host_that_will_not_read_the_choice() -> FakeHost {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let body = "#!/bin/sh\n\
                    exec 0<&-\n\
                    printf '%s\\n' '{\"t\":\"Sessions\",\"list\":[]}'\n\
                    exec sleep 300\n";
        FakeHost::new(dir, body)
    }

    /// The line is claimed BEFORE the `Choose` is written, not after. A
    /// commit after the write would leave a window in which the `Attach` is
    /// on its way to the host and the loop still reads `Connecting` -- and
    /// may promote the standby over an attempt the host is about to adopt.
    ///
    /// Pinned by a write that fails: the far end closed its stdin before it
    /// offered anything, so the `Choose` is refused with a broken pipe and
    /// the attempt ends on the spot. Only an attempt that committed before
    /// writing has committed by then; one that committed after would have
    /// returned on the failed write first and never claimed the line at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_attempt_commits_before_it_writes_its_attach() {
        let fake = fake_host_that_will_not_read_the_choice();
        let mut rebuild = Rebuild::new(
            "bastion.example.net".to_owned(),
            "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
        )
        .via(fake.launcher.clone(), test_config());
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        rebuild.begin(a_size(), tx, Instant::now());

        let report = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("the attempt hung instead of failing on the write")
            .expect("the attempt's sender went away");
        // Read while the report is still unaccepted: the attempt is in
        // flight as far as the stage is concerned.
        assert_eq!(
            rebuild.stage(),
            RebuildStage::Committed,
            "the attempt wrote its Choose before it claimed the line"
        );
        match rebuild.accept(report) {
            Some(AttemptOutcome::Retry(why)) if why.to_lowercase().contains("broken pipe") => {}
            other => panic!("the write was not what ended the attempt: {other:?}"),
        }
    }

    /// An ssh that writes the arguments it was started with to `args`, one
    /// per line, and then fails as an unreachable host does. `-G` is answered
    /// with `config` first, as [`FakeHost::configured`] does.
    fn fake_ssh_recording_its_arguments(config: &str) -> FakeHost {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let record = dir.path().join("args");
        let body = format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > '{}'\n\
             exit 255\n",
            record.display()
        );
        FakeHost::configured(dir, config, &body)
    }

    /// The arguments the rebuild's ssh was started with, given what `ssh -G`
    /// said about its configuration.
    async fn rebuild_arguments(config: &str) -> Vec<String> {
        let fake = fake_ssh_recording_its_arguments(config);
        let _ = run_attempt(&fake, "3ff1218f5e0c4b7d9a1c2e3f40516273").await;
        std::fs::read_to_string(fake.path("args"))
            .expect("the fake ssh recorded no arguments")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The argument list that ships: batch mode always, and the connect
    /// timeout where the user's ssh has none -- placed before the target,
    /// where an ssh option belongs, and dropped where it has one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_rebuild_bounds_the_connect_only_where_ssh_does_not() {
        let remote = ["bastion.example.net", "oxutrm", "host", "--connect"];
        assert_eq!(
            rebuild_arguments("connecttimeout none").await,
            [
                &["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"][..],
                &remote[..]
            ]
            .concat()
        );
        assert_eq!(
            rebuild_arguments("connecttimeout 30").await,
            [&["-o", "BatchMode=yes"][..], &remote[..]].concat(),
            "the user's own ConnectTimeout was overridden"
        );
    }

    /// `recovery.connect_timeout` reaches the next attempt's ssh, and the
    /// network settings with it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_retuned_rebuild_runs_its_next_attempt_with_the_new_settings() {
        let fake = fake_ssh_recording_its_arguments("connecttimeout none");
        let mut rebuild = Rebuild::new(
            "bastion.example.net".to_owned(),
            "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
        )
        .via(fake.launcher.clone(), test_config());
        let cfg = oxutrm_net::NetConfig {
            enable_birthday: false,
            ..test_config()
        };
        rebuild.retune(cfg, std::time::Duration::from_secs(25));
        assert!(!rebuild.cfg().enable_birthday);
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        rebuild.begin(a_size(), tx, Instant::now());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("the attempt never reported");
        let args = std::fs::read_to_string(fake.path("args")).expect("no arguments recorded");
        assert!(
            args.lines().any(|a| a == "ConnectTimeout=25"),
            "the retuned timeout did not reach ssh: {args}"
        );
    }

    /// An ssh that counts the `ssh -G` questions it is asked in `asked`, one
    /// line each, answering them with `config` -- or failing them, for
    /// `None` -- and otherwise records its arguments in `args`, one per
    /// line, and fails as an unreachable host does.
    fn fake_ssh_counting_questions(config: Option<&str>) -> FakeHost {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a scratch directory");
        let asked = dir.path().join("asked");
        let args = dir.path().join("args");
        let answer = match config {
            Some(config) => format!("printf '%s\\n' '{config}'; exit 0"),
            None => "echo 'ssh: cannot resolve' >&2; exit 255".to_owned(),
        };
        let text = format!(
            "#!/bin/sh\n\
             case \" $* \" in *' -G '*) echo asked >> '{}'; {answer};; esac\n\
             printf '%s\\n' \"$@\" > '{}'\n\
             exit 255\n",
            asked.display(),
            args.display()
        );
        let script = dir.path().join("fake-ssh");
        std::fs::write(&script, text).expect("writing the fake ssh");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("making the fake ssh executable");
        let launcher = SshLauncher::command(&script);
        FakeHost { dir, launcher }
    }

    /// Two attempts of one rebuild, run to their end against `fake`: how
    /// often each was asked `ssh -G`, and whether each carried the
    /// connect timeout.
    async fn two_attempts(fake: &FakeHost) -> (usize, [bool; 2]) {
        let mut rebuild = Rebuild::new(
            "bastion.example.net".to_owned(),
            "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
        )
        .via(fake.launcher.clone(), test_config());
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut bounded = [false; 2];
        for b in &mut bounded {
            rebuild.begin(a_size(), tx.clone(), Instant::now());
            let report = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("the attempt never reported")
                .expect("the attempt's sender went away");
            assert!(
                matches!(rebuild.accept(report), Some(AttemptOutcome::Retry(_))),
                "the fake ssh did not fail as expected"
            );
            *b = std::fs::read_to_string(fake.path("args"))
                .expect("the fake ssh recorded no arguments")
                .lines()
                .any(|a| a == "ConnectTimeout=10");
        }
        let asked = std::fs::read_to_string(fake.path("asked"))
            .unwrap_or_default()
            .lines()
            .count();
        (asked, bounded)
    }

    /// `ssh -G` is asked once per rebuild, not once per attempt: a `Match
    /// exec` or a canonicalised host name can make it cost seconds, on every
    /// attempt of an outage. A definite answer is kept either way it goes;
    /// a question that failed is asked again, and in the meantime the
    /// attempt is bounded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ssh_is_asked_about_its_connect_timeout_once_per_rebuild() {
        let none = fake_ssh_counting_questions(Some("connecttimeout none"));
        assert_eq!(two_attempts(&none).await, (1, [true, true]));

        let own = fake_ssh_counting_questions(Some("connecttimeout 30"));
        assert_eq!(
            two_attempts(&own).await,
            (1, [false, false]),
            "the user's own ConnectTimeout was overridden, or asked for twice"
        );

        let failing = fake_ssh_counting_questions(None);
        assert_eq!(
            two_attempts(&failing).await,
            (2, [true, true]),
            "a question that got no answer was not asked again"
        );
    }

    #[test]
    fn the_connect_timeout_is_added_unless_ssh_has_one() {
        let config = |timeout: &str| {
            format!("user tobi\nhostname 10.0.0.7\nconnecttimeout {timeout}\nport 22\n")
        };
        assert!(needs_connect_timeout(Some(&config("none"))));
        assert!(!needs_connect_timeout(Some(&config("30"))));
        assert!(!needs_connect_timeout(Some("ConnectTimeout 5\n")));
        // Not knowing is not a reason to wait 75 s.
        assert!(needs_connect_timeout(None), "ssh -G failed");
        assert!(needs_connect_timeout(Some("")), "ssh -G said nothing");
        assert!(needs_connect_timeout(Some("user tobi\nport 22\n")));
        assert!(needs_connect_timeout(Some("connecttimeout\n")));
        assert!(needs_connect_timeout(Some(&config("soon"))));
        // A keyword that merely starts the same is not this one.
        assert!(needs_connect_timeout(Some("connecttimeoutx 30\n")));
    }

    /// The two sides of the line race, and exactly one wins: an attempt the
    /// loop abandoned first does not send its `Attach`, and one that
    /// committed first is not abandoned.
    #[test]
    fn a_commitment_goes_to_whichever_side_claims_it_first() {
        let abandoned = Commitment::default();
        assert!(abandoned.abandon());
        assert!(!abandoned.commit(), "an abandoned attempt still committed");
        assert!(!abandoned.is_committed());

        let committed = Commitment::default();
        assert!(committed.commit());
        assert!(!committed.abandon(), "a committed attempt was abandoned");
        assert!(committed.is_committed());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_attempt_asks_for_the_session_it_came_from() {
        // The side effect, not the return value: a rebuild that answered `New`
        // would silently start a second shell and look like it had worked.
        let recorded = attempt_against_recording(
            fake_host_recording_the_choice(),
            "3ff1218f5e0c4b7d9a1c2e3f40516273",
        )
        .await;
        assert_eq!(
            recorded,
            serde_json::json!({"c": "Attach", "id": "3ff1218f5e0c4b7d9a1c2e3f40516273"})
        );
    }

    /// A client in a lobby is rebuilt into a fresh lobby, never into the old
    /// one, which has ended or will (switcher spec §3.5).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_attempt_from_a_lobby_asks_for_a_fresh_lobby() {
        let fake = fake_host_recording_the_choice();
        let record = fake.path("choice.json");
        let _ = run_attempt_to(&fake, &Aim::Lobby).await;
        let line = std::fs::read_to_string(&record).expect("the fake host recorded no choice");
        let signal: serde_json::Value = serde_json::from_str(line.trim()).expect("JSON");
        assert_eq!(signal["choice"], serde_json::json!({"c": "Lobby"}));
    }

    #[test]
    fn a_rebuild_is_retargeted_by_a_move() {
        let mut r = Rebuild::new(
            "bastion.example.net".to_owned(),
            "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
        );
        r.retarget(Aim::Lobby);
        assert_eq!(r.aim(), &Aim::Lobby);
        r.retarget(Aim::Session("a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60".to_owned()));
        assert_eq!(
            r.aim(),
            &Aim::Session("a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60".to_owned())
        );
    }

    /// An attempt in flight for the old aim is cancelled by a retarget, so
    /// it never reaches a session that is gone; one for the same aim runs on.
    /// The displacing latch stays: the outage is not over.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_retarget_cancels_an_attempt_for_the_old_aim() {
        let fake = fake_host_that_never_answers();
        let mut rebuild = rebuilding_against(&fake);
        let here = rebuild.aim().clone();
        rebuild.retarget(here);
        assert!(rebuild.is_running(), "the same aim cancelled the attempt");

        rebuild.retarget(Aim::Lobby);
        assert!(!rebuild.is_running(), "the attempt for the old aim runs on");
        assert_eq!(rebuild.aim(), &Aim::Lobby);
        assert!(
            rebuild.may_have_displaced_us(),
            "a retarget ended the outage's claim on a TAKEN_OVER"
        );
    }
}

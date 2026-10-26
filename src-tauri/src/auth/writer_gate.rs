// src-tauri/src/auth/writer_gate.rs
//
// The writer lease gate (items.id=778, decisions.id=896). A data-key rotation
// re-keys every database and blob of an account in place, so nothing may be
// writing (or about to write) to the account while it runs. Every writer takes
// a LEASE from this one gate before it touches any database and releases it
// when done; a rotation DRAINS the gate first. This replaces the scheduler's
// run-slot count, which nothing in the real app ever registered with
// (ConductorScheduler::acquire_run_slot was called only from tests).
//
// TWO GRANULARITIES, ONE GATE
//   - Operation leases (Reply, FocusRun, Ingest, Sweep) span a whole piece of
//     work that can outlive the command that started it: a chat reply is a
//     Focus run streaming for a minute with no database open in between. The
//     FocusRun owns its lease (FocusRun::lease) so it lives until the spawned
//     task has finished `finalize_chat_reply`. Reply and FocusRun are the
//     "visible work" a rotation asks the user about.
//   - Command leases span one IPC command that touches account data. They are
//     short and make a multi-step command (ingest: blob, then run row, then
//     output row) either wholly before a rotation or refused at its entry.
// Store openers are NOT leased: epoch_guard::ensure_current still refuses an
// open while a rotation runs (the runtime backstop), and the engine's Busy
// probe still catches an open WAL file. Completeness of the leases themselves
// is enforced by the source-scan tests at the bottom of this file, which fail
// when a key-using command or a spawn site is neither leased nor listed with a
// reason.
//
// PHASES (per user)
//   Open      everything is admitted.
//   Draining  a rotation has been requested. New visible work, ingest and
//             sweeps are refused so nothing new starts; Command leases are
//             still admitted so the in-flight reply can finish and the UI can
//             keep working while the user decides.
//   Closed    visible work has ended. Every new lease is refused; the rotation
//             waits for in-flight commands to end, then runs.
// A rotation held by THIS process (epoch_guard::begin_rotation) also refuses
// every lease for its user, whatever the phase: the resume and cancel paths do
// not drain, but nothing may start under them either.
//
// THE LEASE PINS THE KEY EPOCH (items.id=781). A store key derives from the
// data key AND the key epoch, and the epoch is resident only while a session is
// unlocked (epoch_guard, cleared by KeyRegistry::clear at sign-out and at every
// lock). A reply or run that outlives its session keeps its own copy of the
// data key but not the epoch, so without a pin every store open after the lock
// failed and the work was lost. `acquire` records the resident epoch on the
// lease; `pinned_epoch` hands it to epoch_guard::effective_epoch while the
// lease is live, and it ends with the lease.
//
// A USER'S CANCEL REACHES A RUN BY ITS ID (items.id=816). A run binds its lease
// to its Focus run id (FocusRun::attach_lease -> WriterLease::bind_run), and
// `request_stop_for_run(user, run_id)` trips that lease's stop token: the same
// token a rotation's 'Stop and save' trips, so the run ends through the same
// `stopped_result` path. The binding is removed with the lease, so it lives
// exactly as long as the run.
//
// RELEASE: WriterLease and DrainGuard are RAII. Dropping releases, so a normal
// end, an early `?` return, a panic unwind and an aborted task all release. A
// DrainGuard dropped for any reason reopens the gate.
//
// EXEMPT FROM THE DRAIN (never take a lease, listed with reasons in
// COMMAND_EXEMPT): the commands that drive the drain itself, the rotation
// status read, cancel of a wait, and logout (it must always work).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::auth::registry::KeyRegistry;

/// How long a rotation waits for in-flight short work (commands, ingests,
/// sweeps) once the visible work is gone, before it gives up with nothing
/// changed (decisions.id=896 point 7, Jason's stuck-writer ruling; stage 2
/// turns the give-up into the Cancel / Keep waiting choice).
pub const COMMAND_DRAIN_CEILING: Duration = Duration::from_secs(30);

/// How long 'Finish first' waits for a chat reply or Focus run before the wait
/// ends with a message offering 'Stop and save' or 'Keep waiting' (Jason's
/// ruling, items.id=778). 'Keep waiting' starts the ceiling again.
pub const FINISH_FIRST_CEILING: Duration = Duration::from_secs(10 * 60);

/// How long 'Stop and save' waits for the stopped work to actually end before
/// it tells the user nothing was changed. Stopping is cooperative; nothing is
/// ever force-killed.
pub const STOP_GRACE: Duration = Duration::from_secs(60);

/// What a lease is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// One IPC command that touches account data.
    Command,
    /// A chat reply (a Focus run that owns a chat reply row).
    Reply,
    /// A Focus run that is not a chat reply.
    FocusRun,
    /// A document import.
    Ingest,
    /// One tick of a periodic sweep.
    Sweep,
}

impl Kind {
    /// Work the user is asked about before a rotation.
    pub fn is_visible_work(self) -> bool {
        matches!(self, Kind::Reply | Kind::FocusRun)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Open,
    Draining,
    Closed,
}

/// Live leases of one user, by kind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub commands: usize,
    pub replies: usize,
    pub focus_runs: usize,
    pub ingests: usize,
    pub sweeps: usize,
}

impl Counts {
    /// Chat replies and Focus runs.
    pub fn visible(&self) -> usize {
        self.replies + self.focus_runs
    }
    /// Everything that is not visible work.
    pub fn short(&self) -> usize {
        self.commands + self.ingests + self.sweeps
    }
    pub fn total(&self) -> usize {
        self.visible() + self.short()
    }
    fn slot(&mut self, kind: Kind) -> &mut usize {
        match kind {
            Kind::Command => &mut self.commands,
            Kind::Reply => &mut self.replies,
            Kind::FocusRun => &mut self.focus_runs,
            Kind::Ingest => &mut self.ingests,
            Kind::Sweep => &mut self.sweeps,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GateError {
    /// No account is unlocked in this process.
    #[error("not logged in")]
    NotLoggedIn,
    /// The account's data key is being changed; nothing new may start.
    #[error("the account's data key is being changed, try again in a moment")]
    Rotating,
    /// A drain is already under way for this account.
    #[error("a key rotation is already in progress for this account")]
    DrainInProgress,
    /// close() was called while chat replies or Focus runs are still running.
    #[error("{0} task(s) are still running")]
    VisibleWork(usize),
    /// Short work did not end within the ceiling.
    #[error("{0} operation(s) did not finish in time")]
    StillBusy(usize),
}

/// Commands that return `Result<_, String>` can `?` a gate error directly.
impl From<GateError> for String {
    fn from(e: GateError) -> String {
        e.to_string()
    }
}

#[derive(Default)]
struct UserState {
    phase: Option<Phase>,
    counts: Counts,
    /// Stop tokens of the visible leases, by lease id.
    stops: BTreeMap<u64, CancellationToken>,
    /// The key epoch each live lease started under, by lease id (items.id=781).
    /// Only leases taken while an epoch was resident appear here.
    epochs: BTreeMap<u64, u64>,
    /// The Focus run each visible lease carries, by lease id (items.id=816).
    /// Bound by `WriterLease::bind_run`, removed with the lease.
    runs: BTreeMap<u64, String>,
}

impl UserState {
    fn phase(&self) -> Phase {
        self.phase.unwrap_or(Phase::Open)
    }
    fn is_idle(&self) -> bool {
        self.phase() == Phase::Open && self.counts.total() == 0
    }
}

struct Gate {
    next_id: u64,
    users: BTreeMap<String, UserState>,
}

static GATE: Mutex<Gate> = Mutex::new(Gate {
    next_id: 0,
    users: BTreeMap::new(),
});

/// Woken on every release and every phase change; waiters re-check.
static CHANGED: Notify = Notify::const_new();

fn gate() -> MutexGuard<'static, Gate> {
    // A poisoned lock still holds consistent counts (every critical section is
    // a few map operations), and refusing to release would hang a rotation.
    GATE.lock().unwrap_or_else(|p| p.into_inner())
}

/// A held permission to write. Dropping it releases.
#[must_use = "the lease is released when this is dropped; hold it for the whole operation"]
#[derive(Debug)]
pub struct WriterLease {
    id: u64,
    user_id: String,
    kind: Kind,
    stop: CancellationToken,
}

impl WriterLease {
    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn user_id(&self) -> &str {
        &self.user_id
    }
    /// Tripped when the user chose to stop this work for a rotation. Only
    /// visible leases are ever cancelled.
    pub fn stop_token(&self) -> &CancellationToken {
        &self.stop
    }

    /// Record that this lease carries Focus run `run_id`, so a user cancel can
    /// reach its stop token by run id (`request_stop_for_run`, items.id=816).
    /// Only visible leases (Reply, FocusRun) are ever bound: the token of a
    /// Command, Ingest or Sweep lease is not a run's, and nothing watches it.
    /// The binding ends with the lease.
    pub fn bind_run(&self, run_id: &str) {
        if !self.kind.is_visible_work() {
            return;
        }
        if let Some(u) = gate().users.get_mut(&self.user_id) {
            u.runs.insert(self.id, run_id.to_owned());
        }
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        {
            let mut g = gate();
            if let Some(u) = g.users.get_mut(&self.user_id) {
                let slot = u.counts.slot(self.kind);
                *slot = slot.saturating_sub(1);
                u.stops.remove(&self.id);
                u.epochs.remove(&self.id);
                u.runs.remove(&self.id);
                if u.is_idle() {
                    g.users.remove(&self.user_id);
                }
            }
        }
        CHANGED.notify_waiters();
    }
}

/// Take a lease for `user_id`.
///
/// ORDERING WITH A ROTATION (items.id=778, the acquire/begin_rotation gap): the
/// rotating flag (epoch_guard::begin_rotation) and this gate are two locks.
/// The two sides use opposite orders so one of them always sees the other:
///   - here: register the lease FIRST, then look at the flag. If a rotation set
///     the flag in the meantime, the lease is undone and refused.
///   - the engine: set the flag FIRST, then count the leases (run_rotation,
///     resume_rotation, cancel_rotation: `check_no_writers`), refusing if any.
///
/// A lease can therefore never be admitted unseen while a rotation starts. The
/// three start commands go further and close the gate first.
pub fn acquire(user_id: &str, kind: Kind) -> Result<WriterLease, GateError> {
    if crate::auth::epoch_guard::is_rotating_for(user_id) {
        return Err(GateError::Rotating);
    }
    // Read before the gate lock is taken: the two locks are never nested here.
    let epoch = crate::auth::epoch_guard::resident_epoch(user_id);
    let lease = {
        let mut g = gate();
        g.next_id += 1;
        let id = g.next_id;
        let u = g.users.entry(user_id.to_owned()).or_default();
        let admitted = match (u.phase(), kind) {
            (Phase::Open, _) => true,
            (Phase::Draining, Kind::Command) => true,
            (Phase::Draining, _) | (Phase::Closed, _) => false,
        };
        if !admitted {
            return Err(GateError::Rotating);
        }
        *u.counts.slot(kind) += 1;
        let stop = CancellationToken::new();
        if kind.is_visible_work() {
            u.stops.insert(id, stop.clone());
        }
        if let Some(epoch) = epoch {
            u.epochs.insert(id, epoch);
        }
        WriterLease {
            id,
            user_id: user_id.to_owned(),
            kind,
            stop,
        }
    };
    // Registered; now look at the flag again (see above). Dropping `lease`
    // undoes the registration.
    if crate::auth::epoch_guard::is_rotating_for(user_id) {
        return Err(GateError::Rotating);
    }
    Ok(lease)
}

/// Trip the stop token of every live run lease of `user_id` that carries
/// `run_id` (a user's Cancel, items.id=816). Returns how many tokens it tripped:
/// 0 for an unknown run, another user's run, or a run that has ended, and the
/// three are indistinguishable. Tripping again is harmless. More than one lease
/// can carry a run id (`resume_run` accepts a run that is still `running`), and
/// all of them are stopped. The run notices through `FocusRun::stop_requested`
/// and the executor's waits and model calls, exactly as for a rotation's 'Stop
/// and save'.
pub fn request_stop_for_run(user_id: &str, run_id: &str) -> usize {
    let g = gate();
    let Some(u) = g.users.get(user_id) else {
        return 0;
    };
    let mut tripped = 0;
    for (lease_id, bound) in &u.runs {
        if bound != run_id {
            continue;
        }
        if let Some(token) = u.stops.get(lease_id) {
            token.cancel();
            tripped += 1;
        }
    }
    tripped
}

/// The engine's half of the ordering: with the rotating flag already set, no
/// lease may be live (a rotation that did not drain first, such as the
/// unlock-time resume and cancel). Returns how many are.
pub fn live_leases(user_id: &str) -> usize {
    counts(user_id).total()
}

/// A lease for the account unlocked in this process. Commands call this first.
pub async fn lease_for_session(
    key_registry: &KeyRegistry,
    kind: Kind,
) -> Result<WriterLease, GateError> {
    let user_id = key_registry.user_id().await.ok_or(GateError::NotLoggedIn)?;
    acquire(&user_id, kind)
}

/// `lease_for_session(.., Kind::Command)`: the one line a command adds.
pub async fn command(key_registry: &KeyRegistry) -> Result<WriterLease, GateError> {
    lease_for_session(key_registry, Kind::Command).await
}

/// The key epoch `user_id`'s live work started under, if any live lease was
/// taken while an epoch was resident (items.id=781). The newest lease wins; they
/// agree in practice, because a rotation waits for every lease to end.
pub fn pinned_epoch(user_id: &str) -> Option<u64> {
    gate()
        .users
        .get(user_id)
        .and_then(|u| u.epochs.values().next_back().copied())
}

/// The live leases of `user_id`.
pub fn counts(user_id: &str) -> Counts {
    gate()
        .users
        .get(user_id)
        .map(|u| u.counts)
        .unwrap_or_default()
}

/// The phase of `user_id`'s gate.
pub fn phase(user_id: &str) -> Phase {
    gate()
        .users
        .get(user_id)
        .map(UserState::phase)
        .unwrap_or(Phase::Open)
}

/// Whether the gate holds no lease and is open for `user_id`.
pub fn is_idle(user_id: &str) -> bool {
    gate().users.get(user_id).is_none_or(UserState::is_idle)
}

/// What a rotation start requires of the gate: closed and empty.
pub fn ensure_closed_and_empty(user_id: &str) -> Result<(), GateCheckError> {
    let g = gate();
    let Some(u) = g.users.get(user_id) else {
        return Err(GateCheckError::NotClosed);
    };
    if u.phase() != Phase::Closed {
        return Err(GateCheckError::NotClosed);
    }
    match u.counts.total() {
        0 => Ok(()),
        n => Err(GateCheckError::Busy(n)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateCheckError {
    /// The caller did not drain and close the gate first.
    NotClosed,
    /// Leases are still held.
    Busy(usize),
}

/// A drain in progress. Created by `begin_drain`; dropping it reopens the gate.
#[must_use = "dropping the guard reopens the gate"]
#[derive(Debug)]
pub struct DrainGuard {
    user_id: String,
}

/// Start draining `user_id`: from now on no new visible work, ingest or sweep
/// is admitted. Only one drain per user.
pub fn begin_drain(user_id: &str) -> Result<DrainGuard, GateError> {
    {
        let mut g = gate();
        let u = g.users.entry(user_id.to_owned()).or_default();
        if u.phase() != Phase::Open {
            return Err(GateError::DrainInProgress);
        }
        u.phase = Some(Phase::Draining);
    }
    CHANGED.notify_waiters();
    Ok(DrainGuard {
        user_id: user_id.to_owned(),
    })
}

impl DrainGuard {
    pub fn counts(&self) -> Counts {
        counts(&self.user_id)
    }

    /// Cancel the stop tokens of every visible lease. Returns how many.
    pub fn request_stop(&self) -> usize {
        let g = gate();
        let Some(u) = g.users.get(&self.user_id) else {
            return 0;
        };
        for token in u.stops.values() {
            token.cancel();
        }
        u.stops.len()
    }

    /// Wait until no chat reply or Focus run holds a lease. No ceiling here:
    /// the caller owns the wait policy.
    pub async fn wait_visible_idle(&self) {
        wait_until(|| counts(&self.user_id).visible() == 0).await;
    }

    /// Close the gate and wait for short work to end, up to `ceiling`. Errors
    /// with `VisibleWork` if visible work is still running (nothing is closed
    /// then) and with `StillBusy` if short work outlives the ceiling (the gate
    /// stays closed; the caller drops the guard to reopen it).
    pub async fn close(&self, ceiling: Duration) -> Result<(), GateError> {
        {
            let mut g = gate();
            let Some(u) = g.users.get_mut(&self.user_id) else {
                return Ok(());
            };
            let visible = u.counts.visible();
            if visible > 0 {
                return Err(GateError::VisibleWork(visible));
            }
            u.phase = Some(Phase::Closed);
        }
        CHANGED.notify_waiters();
        let user = self.user_id.clone();
        match tokio::time::timeout(ceiling, wait_until(|| counts(&user).total() == 0)).await {
            Ok(()) => Ok(()),
            Err(_) => Err(GateError::StillBusy(counts(&self.user_id).total())),
        }
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        {
            let mut g = gate();
            if let Some(u) = g.users.get_mut(&self.user_id) {
                u.phase = None;
                if u.is_idle() {
                    g.users.remove(&self.user_id);
                }
            }
        }
        CHANGED.notify_waiters();
    }
}

// ---------------------------------------------------------------------------
// The drain a rotation runs before it starts
// ---------------------------------------------------------------------------

/// What the user chose when a rotation found a chat reply or Focus run
/// running. Nothing is cancelled without this choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// No choice made yet: report what is running and change nothing.
    Ask,
    /// Wait for the work to end, then rotate.
    FinishFirst,
    /// Stop the work cooperatively (a reply is saved as stopped, a Focus run
    /// is marked cancelled), then rotate.
    StopAndSave,
}

/// The waits of a drain. `Default` is the ruled constants; tests shorten them.
#[derive(Debug, Clone, Copy)]
pub struct DrainPolicy {
    pub finish_first: Duration,
    pub stop_grace: Duration,
    pub commands: Duration,
}

impl Default for DrainPolicy {
    fn default() -> Self {
        Self {
            finish_first: FINISH_FIRST_CEILING,
            stop_grace: STOP_GRACE,
            commands: COMMAND_DRAIN_CEILING,
        }
    }
}

/// Why a drain did not produce a closed gate. In every case nothing was
/// changed and the gate is open again.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DrainError {
    #[error(transparent)]
    Gate(#[from] GateError),
    /// Visible work is running and the user has not chosen yet.
    #[error("{replies} reply(ies) and {focus_runs} Focus run(s) are running")]
    WritersActive { replies: usize, focus_runs: usize },
    /// The wait reached its ceiling. The counts are what was still running.
    #[error("{replies} reply(ies) and {focus_runs} Focus run(s) are still running")]
    StillRunning { replies: usize, focus_runs: usize },
    /// 'Stop and save' was asked and the work had not stopped after the grace.
    #[error("the running work did not stop in time")]
    NotStopping,
    /// The user pressed Cancel during the wait.
    #[error("the wait was cancelled")]
    Cancelled,
}

static WAITS: Mutex<BTreeMap<String, (u64, CancellationToken)>> = Mutex::new(BTreeMap::new());
static WAIT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn waits() -> MutexGuard<'static, BTreeMap<String, (u64, CancellationToken)>> {
    WAITS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Registers a drain's Cancel token for its user; removes it on drop.
struct WaitRegistration {
    user_id: String,
    generation: u64,
    token: CancellationToken,
}

impl WaitRegistration {
    fn new(user_id: &str) -> Self {
        let generation = WAIT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let token = CancellationToken::new();
        waits().insert(user_id.to_owned(), (generation, token.clone()));
        Self {
            user_id: user_id.to_owned(),
            generation,
            token,
        }
    }
}

impl Drop for WaitRegistration {
    fn drop(&mut self) {
        let mut w = waits();
        // Only our own entry: a newer drain for the same user keeps its token.
        if w.get(&self.user_id)
            .is_some_and(|(g, _)| *g == self.generation)
        {
            w.remove(&self.user_id);
        }
    }
}

/// Cancel the wait of the drain running for `user_id`, if any. The waiting
/// call returns `DrainError::Cancelled`, the gate reopens and nothing changes.
pub fn cancel_wait(user_id: &str) -> bool {
    match waits().get(user_id) {
        Some((_, token)) => {
            token.cancel();
            true
        }
        None => false,
    }
}

/// Cancel the wait that is under way, whichever account it belongs to. For the
/// restore screen, where nobody is signed in so there is no session to name
/// the account: cancel the wait of `resident_user` if it has one, otherwise
/// every registered wait (at most one is ever running; a drain is per
/// account but only one rotation runs per process). Returns whether any wait
/// was cancelled.
pub fn cancel_any_wait(resident_user: Option<&str>) -> bool {
    if let Some(user) = resident_user {
        if cancel_wait(user) {
            return true;
        }
    }
    let waits = waits();
    for (_, token) in waits.values() {
        token.cancel();
    }
    !waits.is_empty()
}

enum WaitEnd {
    Done,
    Timeout,
    Cancelled,
}

async fn visible_idle_or(
    guard: &DrainGuard,
    cancel: &CancellationToken,
    limit: Duration,
) -> WaitEnd {
    tokio::select! {
        () = guard.wait_visible_idle() => WaitEnd::Done,
        () = tokio::time::sleep(limit) => WaitEnd::Timeout,
        () = cancel.cancelled() => WaitEnd::Cancelled,
    }
}

/// Drain `user_id`'s writers and close the gate, following the user's choice
/// (decisions.id=896 points 2 and 3). On `Ok` the returned guard keeps the
/// gate closed; dropping it reopens. On `Err` nothing was changed.
pub async fn drain_for_rotation(
    user_id: &str,
    choice: Choice,
    policy: DrainPolicy,
) -> Result<DrainGuard, DrainError> {
    let drain = begin_drain(user_id)?;
    let registration = WaitRegistration::new(user_id);
    let cancel = &registration.token;

    let running = drain.counts();
    if running.visible() > 0 {
        match choice {
            Choice::Ask => {
                return Err(DrainError::WritersActive {
                    replies: running.replies,
                    focus_runs: running.focus_runs,
                })
            }
            Choice::FinishFirst => {
                match visible_idle_or(&drain, cancel, policy.finish_first).await {
                    WaitEnd::Done => {}
                    WaitEnd::Cancelled => return Err(DrainError::Cancelled),
                    WaitEnd::Timeout => {
                        let now = drain.counts();
                        return Err(DrainError::StillRunning {
                            replies: now.replies,
                            focus_runs: now.focus_runs,
                        });
                    }
                }
            }
            Choice::StopAndSave => {
                drain.request_stop();
                match visible_idle_or(&drain, cancel, policy.stop_grace).await {
                    WaitEnd::Done => {}
                    WaitEnd::Cancelled => return Err(DrainError::Cancelled),
                    WaitEnd::Timeout => return Err(DrainError::NotStopping),
                }
            }
        }
    }

    // Visible work is gone; close the gate and wait for short work.
    let closing = drain.close(policy.commands);
    tokio::select! {
        closed = closing => match closed {
            Ok(()) => {}
            Err(GateError::VisibleWork(_)) | Err(GateError::StillBusy(_)) => {
                let now = drain.counts();
                return Err(DrainError::StillRunning {
                    replies: now.replies,
                    focus_runs: now.focus_runs,
                });
            }
            Err(other) => return Err(DrainError::Gate(other)),
        },
        () = cancel.cancelled() => return Err(DrainError::Cancelled),
    }
    Ok(drain)
}

/// Re-check `cond` on every gate change. Registers for the wake-up BEFORE
/// checking, so a release between the check and the await is not missed.
async fn wait_until(mut cond: impl FnMut() -> bool) {
    loop {
        let notified = CHANGED.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if cond() {
            return;
        }
        notified.await;
    }
}

/// The wait registry is process-global and `cancel_any_wait` cancels every
/// wait, so tests that drain or cancel a wait run one at a time.
#[cfg(test)]
pub(crate) static DRAIN_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    // The gate is process-global: every test uses its own user id.

    // -- items.id=816: a user's cancel reaches a run by its id ---------------

    #[test]
    fn a_cancel_trips_the_token_of_the_lease_bound_to_the_run() {
        let u = "gate-cancel-run";
        let lease = acquire(u, Kind::FocusRun).unwrap();
        lease.bind_run("run-1");
        assert!(!lease.stop_token().is_cancelled());
        assert_eq!(request_stop_for_run(u, "run-1"), 1);
        assert!(lease.stop_token().is_cancelled());
        // Idempotent: a second cancel is a harmless repeat.
        assert_eq!(request_stop_for_run(u, "run-1"), 1);
        assert!(lease.stop_token().is_cancelled());
    }

    #[test]
    fn another_users_run_and_an_unknown_run_trip_nothing_and_look_alike() {
        let owner = "gate-cancel-owner";
        let lease = acquire(owner, Kind::Reply).unwrap();
        lease.bind_run("run-1");
        assert_eq!(request_stop_for_run("gate-cancel-other", "run-1"), 0);
        assert_eq!(request_stop_for_run(owner, "no-such-run"), 0);
        assert_eq!(request_stop_for_run("gate-cancel-nobody", "no-such-run"), 0);
        assert!(!lease.stop_token().is_cancelled());
    }

    #[test]
    fn another_run_of_the_same_user_is_left_running() {
        let u = "gate-cancel-two-runs";
        let a = acquire(u, Kind::FocusRun).unwrap();
        let b = acquire(u, Kind::Reply).unwrap();
        a.bind_run("run-a");
        b.bind_run("run-b");
        assert_eq!(request_stop_for_run(u, "run-a"), 1);
        assert!(a.stop_token().is_cancelled());
        assert!(!b.stop_token().is_cancelled());
    }

    #[test]
    fn the_binding_ends_with_the_lease() {
        let u = "gate-cancel-ended";
        let lease = acquire(u, Kind::FocusRun).unwrap();
        lease.bind_run("run-1");
        drop(lease);
        assert_eq!(request_stop_for_run(u, "run-1"), 0);
        // Nothing is left behind: the user's state is gone with the lease.
        assert!(is_idle(u));
        assert!(!gate().users.contains_key(u));
    }

    #[test]
    fn a_lease_of_a_kind_that_is_not_visible_work_is_never_bound() {
        let u = "gate-cancel-kinds";
        let leases: Vec<WriterLease> = [Kind::Command, Kind::Ingest, Kind::Sweep]
            .into_iter()
            .map(|k| acquire(u, k).unwrap())
            .collect();
        for l in &leases {
            l.bind_run("run-1");
        }
        assert_eq!(request_stop_for_run(u, "run-1"), 0);
        assert!(leases.iter().all(|l| !l.stop_token().is_cancelled()));
    }

    #[test]
    fn two_leases_on_one_run_id_are_both_stopped() {
        // resume_run accepts a run that is still 'running'.
        let u = "gate-cancel-double";
        let a = acquire(u, Kind::FocusRun).unwrap();
        let b = acquire(u, Kind::FocusRun).unwrap();
        a.bind_run("run-1");
        b.bind_run("run-1");
        assert_eq!(request_stop_for_run(u, "run-1"), 2);
        assert!(a.stop_token().is_cancelled() && b.stop_token().is_cancelled());
    }

    #[test]
    fn a_user_cancel_does_not_change_the_phase_or_the_counts() {
        let u = "gate-cancel-counts";
        let lease = acquire(u, Kind::FocusRun).unwrap();
        lease.bind_run("run-1");
        let before = counts(u);
        request_stop_for_run(u, "run-1");
        assert_eq!(counts(u), before);
        assert_eq!(phase(u), Phase::Open);
        // A tripped lease is still held until its run drops it.
        assert!(!is_idle(u));
    }

    #[test]
    fn a_lease_is_released_on_drop() {
        let u = "gate-drop";
        let lease = acquire(u, Kind::Command).unwrap();
        assert_eq!(counts(u).commands, 1);
        drop(lease);
        assert!(is_idle(u));
    }

    #[test]
    fn a_lease_is_released_on_an_early_return() {
        fn work(u: &str) -> Result<(), GateError> {
            let _lease = acquire(u, Kind::Reply)?;
            Err(GateError::Rotating)?;
            Ok(())
        }
        let u = "gate-early";
        assert!(work(u).is_err());
        assert!(is_idle(u));
    }

    #[test]
    fn a_lease_is_released_on_a_panic() {
        let u = "gate-panic";
        let result = std::panic::catch_unwind(|| {
            let _lease = acquire(u, Kind::FocusRun).unwrap();
            panic!("writer died");
        });
        assert!(result.is_err());
        assert!(is_idle(u));
    }

    #[tokio::test]
    async fn a_lease_is_released_when_its_task_is_aborted() {
        let u = "gate-abort";
        // CHANGED is notified on release and phase changes, never on acquire, so
        // wait_until cannot see the lease being taken: the task reports it.
        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _lease = acquire(u, Kind::Reply).unwrap();
            held_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        held_rx.await.unwrap();
        assert_eq!(counts(u).replies, 1);
        task.abort();
        let _ = task.await;
        assert!(is_idle(u));
    }

    #[test]
    fn draining_refuses_new_work_but_admits_commands() {
        let u = "gate-draining";
        let reply = acquire(u, Kind::Reply).unwrap();
        let drain = begin_drain(u).unwrap();
        assert_eq!(phase(u), Phase::Draining);
        for kind in [Kind::Reply, Kind::FocusRun, Kind::Ingest, Kind::Sweep] {
            assert_eq!(acquire(u, kind).unwrap_err(), GateError::Rotating);
        }
        // The in-flight reply and the UI keep working.
        let cmd = acquire(u, Kind::Command).unwrap();
        drop(cmd);
        drop(reply);
        drop(drain);
        assert!(is_idle(u));
    }

    #[test]
    fn a_second_drain_for_the_same_user_is_refused_and_other_users_are_unaffected() {
        let u = "gate-second";
        let _first = begin_drain(u).unwrap();
        assert_eq!(begin_drain(u).unwrap_err(), GateError::DrainInProgress);
        assert!(acquire("gate-second-other", Kind::Reply).is_ok());
    }

    #[test]
    fn dropping_the_drain_guard_reopens_the_gate() {
        let u = "gate-reopen";
        let drain = begin_drain(u).unwrap();
        assert!(acquire(u, Kind::Reply).is_err());
        drop(drain);
        assert_eq!(phase(u), Phase::Open);
        assert!(acquire(u, Kind::Reply).is_ok());
    }

    #[test]
    fn a_drain_guard_dropped_by_a_panic_reopens_the_gate() {
        let u = "gate-reopen-panic";
        let _ = std::panic::catch_unwind(|| {
            let _drain = begin_drain(u).unwrap();
            panic!("rotation command died");
        });
        assert_eq!(phase(u), Phase::Open);
    }

    #[tokio::test]
    async fn close_refuses_while_visible_work_runs_and_closes_when_it_ends() {
        let u = "gate-close";
        let reply = acquire(u, Kind::Reply).unwrap();
        let drain = begin_drain(u).unwrap();
        assert_eq!(
            drain.close(Duration::from_secs(1)).await.unwrap_err(),
            GateError::VisibleWork(1)
        );
        assert_eq!(phase(u), Phase::Draining);
        drop(reply);
        drain.wait_visible_idle().await;
        drain.close(Duration::from_secs(1)).await.unwrap();
        assert_eq!(phase(u), Phase::Closed);
        assert!(ensure_closed_and_empty(u).is_ok());
        // Closed refuses everything, commands included.
        assert_eq!(acquire(u, Kind::Command).unwrap_err(), GateError::Rotating);
    }

    #[tokio::test]
    async fn close_waits_for_in_flight_commands() {
        let u = "gate-close-wait";
        let cmd = acquire(u, Kind::Command).unwrap();
        let drain = begin_drain(u).unwrap();
        let closing = tokio::spawn({
            let u = u.to_owned();
            async move {
                // Same user, moved into the task with the guard.
                let drain = drain;
                let r = drain.close(Duration::from_secs(5)).await;
                (r, phase(&u), drain)
            }
        });
        // Not done while the command still holds its lease.
        tokio::task::yield_now().await;
        assert!(!closing.is_finished());
        drop(cmd);
        let (result, phase_then, _drain) = closing.await.unwrap();
        result.unwrap();
        assert_eq!(phase_then, Phase::Closed);
    }

    #[tokio::test(start_paused = true)]
    async fn close_gives_up_after_the_ceiling_and_leaves_the_guard_to_reopen() {
        let u = "gate-ceiling";
        let _stuck = acquire(u, Kind::Command).unwrap();
        let drain = begin_drain(u).unwrap();
        let err = drain.close(COMMAND_DRAIN_CEILING).await.unwrap_err();
        assert_eq!(err, GateError::StillBusy(1));
        assert_eq!(
            ensure_closed_and_empty(u).unwrap_err(),
            GateCheckError::Busy(1)
        );
        drop(drain);
        assert_eq!(phase(u), Phase::Open);
    }

    #[test]
    fn a_rotation_start_needs_a_closed_gate() {
        let u = "gate-needs-closed";
        assert_eq!(
            ensure_closed_and_empty(u).unwrap_err(),
            GateCheckError::NotClosed
        );
        let _drain = begin_drain(u).unwrap();
        assert_eq!(
            ensure_closed_and_empty(u).unwrap_err(),
            GateCheckError::NotClosed
        );
    }

    #[test]
    fn request_stop_trips_visible_leases_only() {
        let u = "gate-stop";
        let reply = acquire(u, Kind::Reply).unwrap();
        let run = acquire(u, Kind::FocusRun).unwrap();
        let cmd = acquire(u, Kind::Command).unwrap();
        let drain = begin_drain(u).unwrap();
        assert_eq!(drain.request_stop(), 2);
        assert!(reply.stop_token().is_cancelled());
        assert!(run.stop_token().is_cancelled());
        assert!(!cmd.stop_token().is_cancelled());
    }

    #[tokio::test]
    async fn a_lease_registered_just_before_the_flag_is_seen_by_the_engine() {
        // The other order of the race: the lease won, then the flag was set.
        // The engine, checking after it set the flag, must see the lease.
        let _serial = crate::test_support::ENV_MUTEX.lock().await;
        let u = "gate-race";
        let lease = acquire(u, Kind::Command).unwrap();
        let guard = crate::auth::epoch_guard::begin_rotation(u).unwrap();
        assert_eq!(live_leases(u), 1);
        drop(guard);
        drop(lease);
        assert_eq!(live_leases(u), 0);
    }

    #[tokio::test]
    async fn a_rotation_held_by_this_process_refuses_every_lease() {
        let _serial = crate::test_support::ENV_MUTEX.lock().await;
        let u = "gate-rotating";
        let guard = crate::auth::epoch_guard::begin_rotation(u).unwrap();
        assert_eq!(acquire(u, Kind::Command).unwrap_err(), GateError::Rotating);
        drop(guard);
        assert!(acquire(u, Kind::Command).is_ok());
    }

    #[test]
    fn a_lease_pins_the_resident_epoch_until_it_drops() {
        // items.id=796: the resident slot is one process-wide value that the
        // ENV_MUTEX tests (login, key rotation, messages) also write.
        let _serial = crate::test_support::ENV_MUTEX.blocking_lock();
        let u = "gate-pin";
        // Taken with no session: nothing to pin.
        let none = acquire(u, Kind::Command).unwrap();
        assert_eq!(pinned_epoch(u), None);
        drop(none);

        crate::auth::epoch_guard::set_resident(u, 7);
        let a = acquire(u, Kind::Reply).unwrap();
        let b = acquire(u, Kind::Command).unwrap();
        crate::auth::epoch_guard::clear_resident_for(u);
        assert_eq!(pinned_epoch(u), Some(7));
        drop(a);
        assert_eq!(pinned_epoch(u), Some(7), "the other lease still pins it");
        drop(b);
        assert_eq!(pinned_epoch(u), None);
        assert!(is_idle(u));
    }

    #[test]
    fn a_gate_error_converts_to_the_string_commands_return() {
        let s: String = GateError::NotLoggedIn.into();
        assert_eq!(s, "not logged in");
    }

    // ------------------------------------------------------------------
    // Source scans: how we know the inventory of writers is complete
    // (items.id=778). Modelled on epoch_guard's opener scan. A new command,
    // spawn site or pool that is not leased and not listed here, with a
    // reason, fails the build.
    // ------------------------------------------------------------------

    /// Registered IPC commands that take the session's KeyRegistry but do not
    /// take a lease, each with the reason it is safe (or required) to skip.
    const COMMAND_EXEMPT: &[(&str, &str)] = &[
        (
            "auth::login",
            "runs before a session exists; its account-data work is inside finish_login, which takes its own lease",
        ),
        (
            "auth::logout",
            "must always work: ending a session is never held up by a drain (items.id=778 ruling)",
        ),
        (
            "auth::get_session",
            "reads shared.db's auth_sessions only, no account database",
        ),
        (
            "auth::record_activity",
            "writes shared.db's auth_sessions only, no account database",
        ),
        (
            "auth::get_recovery_key_display",
            "reads the in-memory registry slot only",
        ),
        (
            "key_rotation::change_password",
            "is the drainer: it drains and closes the gate itself",
        ),
        (
            "key_rotation::begin_new_recovery_phrase",
            "part of the rotation flow: verifies the password and fills the in-memory phrase slot, no account database",
        ),
        (
            "key_rotation::cancel_new_recovery_phrase",
            "clears the in-memory phrase slot only",
        ),
        (
            "key_rotation::confirm_new_recovery_phrase",
            "is the drainer: it drains and closes the gate itself",
        ),
        (
            "key_rotation::restore_with_recovery_phrase",
            "is the drainer: it drains and closes the gate itself",
        ),
        (
            "key_rotation::get_rotation_status",
            "the status read must never be blocked by the drain it reports on (items.id=778 ruling)",
        ),
        (
            "key_rotation::cancel_writer_wait",
            "cancels a drain's wait; it must work while the gate is closed (items.id=778 ruling)",
        ),
        (
            "chat_stream::cancel_chat_stream",
            "trips an in-memory stream token only; it must keep working while a drain waits",
        ),
    ];

    /// Non-test source files that spawn work and take no lease, each with why.
    const SPAWN_EXEMPT: &[(&str, &str)] = &[
        (
            "commands/model_install.rs",
            "local model download task: model files and shared.db rows only, no account-keyed store",
        ),
        (
            "conductor/privacy/privacy_filter.rs",
            "CPU-bound classifier on spawn_blocking, no database",
        ),
        (
            "persistence/ingest_blob.rs",
            "spawn_blocking around AEAD on bytes already in memory; the callers hold the lease",
        ),
        (
            "providers/loopback_test_server.rs",
            "local test HTTP server, no database",
        ),
        (
            "auth/key_rotation_io.rs",
            "the rotation engine's own inventory walk and re-key primitives",
        ),
        ("auth/kdf.rs", "Argon2 on spawn_blocking, no database"),
        (
            "task_supervision.rs",
            "generic supervisor: the supervised bodies take their own leases (see MAIN_TASKS)",
        ),
        (
            "ollama_spawn_thread.rs",
            "child-process wait, no database",
        ),
        (
            "provider_credentials/oauth/flow.rs",
            "token exchange into the in-memory connect session; the database write is save_provider_key, which is leased",
        ),
    ];

    /// main.rs's supervised background tasks and how each is covered. main.rs
    /// contains a lease call, which would hide a new task from the file-level
    /// scan, so its tasks are pinned by name.
    const MAIN_TASKS: &[(&str, &str)] = &[
        ("ollama_detection", "no database"),
        ("ollama_liveness_watch", "no database"),
        (
            "group_persona_sync_sweep",
            "takes a Sweep lease for the tick; the persona-share sweeps take their own",
        ),
        (
            "idle_timeout_check",
            "shared.db only (auth_sessions); stands down while rotating (idle_timeout::run_periodic_check)",
        ),
        (
            "scheduled_step_sweep",
            "scheduled_sweep::run_periodic_sweep takes a Sweep lease and a FocusRun lease per resumed run",
        ),
        (
            "nightly_batch_sweep",
            "shared.db only today (nightly_batch::run_periodic_sweep); it must take a lease before it ever touches an account store",
        ),
    ];

    fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rs_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    fn src_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    /// The production part of a source file: everything before its test
    /// module. Test files themselves are skipped by the callers.
    fn production_text(text: &str) -> &str {
        // Cut at the test MODULE, not at the first #[cfg(test)] attribute: a
        // test-only item can sit mid-file, with production code after it. An
        // inline module only (`{`): an out-of-line `mod tests;` is a one-line
        // declaration, and the production code after it must stay visible.
        let re =
            regex::Regex::new(r"(?m)^#\[cfg\(test\)\]\s*\n\s*(?:pub\(crate\)\s+)?mod\s+\w+\s*\{")
                .unwrap();
        re.find(text).map_or(text, |m| &text[..m.start()])
    }

    fn is_test_file(rel: &str) -> bool {
        rel.ends_with("tests.rs")
            || rel.ends_with("_tests.rs")
            || rel.contains("/tests/")
            || rel == "test_support.rs"
    }

    /// The signature and body of `pub ... fn name(` in `text`: from the `fn`
    /// to the next top-level closing brace line.
    fn fn_text<'a>(text: &'a str, name: &str) -> Option<&'a str> {
        let re = regex::Regex::new(&format!(
            r"(?m)^\s*pub(?:\([a-z]+\))?\s+(?:async\s+)?fn\s+{name}\s*[(<]"
        ))
        .unwrap();
        let start = re.find(text)?.start();
        let end = text[start..]
            .find("\n}\n")
            .map_or(text.len(), |i| start + i);
        Some(&text[start..end])
    }

    #[test]
    fn every_registered_command_with_a_session_takes_a_lease_or_is_listed() {
        let ipc = std::fs::read_to_string(src_dir().join("ipc.rs")).unwrap();
        let start = ipc.find("collect_commands![").unwrap();
        let end = start + ipc[start..].find("])").unwrap();
        let re = regex::Regex::new(r"commands::(\w+)::(\w+),").unwrap();
        let registered: Vec<(String, String)> = re
            .captures_iter(&ipc[start..end])
            .map(|c| (c[1].to_owned(), c[2].to_owned()))
            .collect();
        assert!(
            registered.len() > 100,
            "ipc.rs parse found only {} commands",
            registered.len()
        );

        let mut unleased = Vec::new();
        let mut seen_exempt = std::collections::BTreeSet::new();
        let mut leased = 0usize;
        let mut sessionless = 0usize;
        for (module, name) in &registered {
            let path = src_dir().join("commands").join(format!("{module}.rs"));
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let text = production_text(&text);
            let body = fn_text(text, name)
                .unwrap_or_else(|| panic!("registered command {module}::{name} not found"));
            let sig_end = body.find(" -> ").unwrap_or(body.len());
            if !body[..sig_end].contains("KeyRegistry") {
                sessionless += 1;
                continue;
            }
            let key = format!("{module}::{name}");
            let exempt = COMMAND_EXEMPT.iter().any(|(k, _)| *k == key);
            if exempt {
                seen_exempt.insert(key);
            } else if body.contains("writer_gate::") {
                leased += 1;
            } else {
                unleased.push(key);
            }
        }
        assert!(
            unleased.is_empty(),
            "these commands take the session's KeyRegistry but neither take a writer lease nor \
             are listed in COMMAND_EXEMPT with a reason (a missed writer is silent data loss \
             during a key rotation): {unleased:?}"
        );
        // A stale exemption hides a command that changed.
        for (key, _) in COMMAND_EXEMPT {
            assert!(
                seen_exempt.contains(*key),
                "COMMAND_EXEMPT lists {key}, which is not a registered command with a KeyRegistry parameter"
            );
        }
        // Reconciliation numbers (reported in the stage 1 summary).
        println!(
            "writer_gate scan: {} registered commands, {leased} leased, {} exempt, {sessionless} without a session",
            registered.len(),
            seen_exempt.len()
        );
    }

    #[test]
    fn every_command_attribute_is_registered() {
        // 130 attribute lines were counted by hand, 123 are registered; the
        // difference is test modules and test files. This pins the real
        // relation: every production #[tauri::command] is in ipc.rs.
        let ipc = std::fs::read_to_string(src_dir().join("ipc.rs")).unwrap();
        let mut missing = Vec::new();
        let mut files = Vec::new();
        rs_files(&src_dir().join("commands"), &mut files);
        let fn_re = regex::Regex::new(r"\bfn\s+(\w+)\s*[(<]").unwrap();
        for path in files {
            let rel = path
                .strip_prefix(src_dir())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if is_test_file(&rel) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let module = path.file_stem().unwrap().to_string_lossy().into_owned();
            let prod = production_text(&text);
            for (at, _) in prod.match_indices("#[tauri::command]") {
                // Only a real attribute line, not a mention in a comment.
                let line_start = prod[..at].rfind('\n').map_or(0, |i| i + 1);
                if !prod[line_start..at].trim().is_empty() {
                    continue;
                }
                // The command is the first `fn` after its attribute.
                let Some(c) = fn_re.captures(&prod[at..]) else {
                    continue;
                };
                let needle = format!("commands::{module}::{},", &c[1]);
                if !ipc.contains(&needle) {
                    missing.push(format!("{module}::{}", &c[1]));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "#[tauri::command] functions that are not registered in ipc.rs: {missing:?}"
        );
    }

    /// Spawn sites that sit in a function with no lease in sight, each with why
    /// the work they start is safe. Keyed `file::function`.
    const SPAWN_SITE_EXEMPT: &[(&str, &str)] = &[];

    /// The function a byte offset sits in: from the nearest `fn` line at or
    /// before it to the next `fn` line, with that function's name.
    fn enclosing_fn(code: &str, at: usize) -> (String, &str) {
        let fn_re =
            regex::Regex::new(r"(?m)^[ \t]*(?:pub(?:\([a-z]+\))?\s+)?(?:async\s+)?fn\s+(\w+)")
                .unwrap();
        let starts: Vec<(usize, String)> = fn_re
            .captures_iter(code)
            .map(|c| (c.get(0).unwrap().start(), c[1].to_owned()))
            .collect();
        let idx = starts.iter().rposition(|(s, _)| *s <= at).unwrap_or(0);
        let (start, name) = starts
            .get(idx)
            .cloned()
            .unwrap_or((0, "<module>".to_owned()));
        let end = starts.get(idx + 1).map_or(code.len(), |(s, _)| *s);
        (name, &code[start..end])
    }

    #[test]
    fn every_spawn_site_takes_a_lease_or_is_listed() {
        let spawn_re = regex::Regex::new(
            r"\b(tokio::spawn|spawn_supervised|tauri::async_runtime::spawn|std::thread::spawn|thread::spawn|spawn_blocking)\s*\(",
        )
        .unwrap();
        let mut files = Vec::new();
        rs_files(&src_dir(), &mut files);
        let mut unleased = Vec::new();
        let mut seen_exempt = std::collections::BTreeSet::new();
        let mut seen_site_exempt = std::collections::BTreeSet::new();
        for path in files {
            let rel = path
                .strip_prefix(src_dir())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if is_test_file(&rel) || rel == "auth/writer_gate.rs" || rel == "main.rs" {
                continue; // main.rs: pinned task by task (MAIN_TASKS)
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let prod = production_text(&text);
            // Comments out, byte offsets kept (blank lines keep the layout).
            let code: String = prod
                .lines()
                .map(|l| {
                    if l.trim_start().starts_with("//") {
                        ""
                    } else {
                        l
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            let sites: Vec<usize> = spawn_re.find_iter(&code).map(|m| m.start()).collect();
            if sites.is_empty() {
                continue;
            }
            if let Some((f, _)) = SPAWN_EXEMPT.iter().find(|(f, _)| *f == rel) {
                seen_exempt.insert(*f);
                continue;
            }
            // Per SITE: the function that spawns must take a lease or hand one
            // to the task (a `lease` it moves in), or be listed.
            for at in sites {
                let (name, window) = enclosing_fn(&code, at);
                let key = format!("{rel}::{name}");
                if SPAWN_SITE_EXEMPT.iter().any(|(k, _)| *k == key) {
                    seen_site_exempt.insert(key);
                } else if !(window.contains("writer_gate::") || window.contains("lease")) {
                    unleased.push(key);
                }
            }
        }
        assert!(
            unleased.is_empty(),
            "these functions spawn work but neither take a writer lease nor pass one into the \
             task, and are not listed in SPAWN_SITE_EXEMPT (or their file in SPAWN_EXEMPT) \
             with a reason: {unleased:?}"
        );
        for (f, _) in SPAWN_EXEMPT {
            assert!(
                seen_exempt.contains(f),
                "SPAWN_EXEMPT lists {f}, which no longer spawns: remove it"
            );
        }
        for (k, _) in SPAWN_SITE_EXEMPT {
            assert!(
                seen_site_exempt.contains(*k),
                "SPAWN_SITE_EXEMPT lists {k}, which no longer spawns: remove it"
            );
        }
    }

    /// The production code of every non-test source file, comment lines
    /// blanked, as `(relative path, text)`.
    fn production_sources() -> Vec<(String, String)> {
        let mut files = Vec::new();
        rs_files(&src_dir(), &mut files);
        let mut out = Vec::new();
        for path in files {
            let rel = path
                .strip_prefix(src_dir())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if is_test_file(&rel) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let code = production_text(&text)
                .lines()
                .map(|l| {
                    if l.trim_start().starts_with("//") {
                        ""
                    } else {
                        l
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            out.push((rel, code));
        }
        out
    }

    #[test]
    fn a_run_gets_its_lease_only_through_attach_lease() {
        // items.id=816: attach_lease binds the lease to the run id, which is
        // how a user's cancel finds the run. A direct assignment would leave a
        // run that Cancel cannot reach.
        let assign = regex::Regex::new(r"\.lease\s*=\s*Some\(").unwrap();
        let offenders: Vec<String> = production_sources()
            .into_iter()
            .filter(|(rel, code)| rel != "conductor/lifecycle.rs" && assign.is_match(code))
            .map(|(rel, _)| rel)
            .collect();
        assert!(
            offenders.is_empty(),
            "these files assign FocusRun::lease directly instead of calling attach_lease: \
             {offenders:?}"
        );
    }

    #[test]
    fn user_stop_has_exactly_one_producer() {
        // items.id=816: the lifecycle now ends a run as cancelled on a step
        // failure whose mode is USER_STOP, so only executor.rs's
        // `stopped_failure` may build one.
        let literal = regex::Regex::new(r#""USER_STOP""#).unwrap();
        let producer =
            regex::Regex::new(r"failure_mode\s*:\s*Some\(\s*(?:\w+::)*USER_STOP_MODE").unwrap();
        let mut literals = Vec::new();
        let mut producers = Vec::new();
        for (rel, code) in production_sources() {
            for _ in literal.find_iter(&code) {
                literals.push(rel.clone());
            }
            for m in producer.find_iter(&code) {
                producers.push((rel.clone(), enclosing_fn(&code, m.start()).0));
            }
        }
        assert_eq!(
            literals,
            vec!["conductor/executor.rs".to_owned()],
            "the \"USER_STOP\" literal belongs once, in executor.rs's USER_STOP_MODE constant"
        );
        assert_eq!(
            producers,
            vec![(
                "conductor/executor.rs".to_owned(),
                "stopped_failure".to_owned()
            )],
            "a USER_STOP failure may be built only by executor.rs stopped_failure"
        );
    }

    #[test]
    fn every_supervised_task_in_main_is_accounted_for() {
        let main = std::fs::read_to_string(src_dir().join("main.rs")).unwrap();
        let re = regex::Regex::new(r#"spawn_supervised\(\s*"(\w+)""#).unwrap();
        let found: std::collections::BTreeSet<String> =
            re.captures_iter(&main).map(|c| c[1].to_owned()).collect();
        let listed: std::collections::BTreeSet<String> =
            MAIN_TASKS.iter().map(|(n, _)| (*n).to_owned()).collect();
        assert_eq!(
            found, listed,
            "main.rs spawns a supervised task that MAIN_TASKS does not list (or lists one it no \
             longer spawns): decide whether it needs a writer lease and record why"
        );
    }

    #[test]
    fn no_pool_is_built_over_an_encrypted_database() {
        // Account databases are opened as single connections so that nothing
        // outlives the key registry's clear (CLAUDE.md topology). The only
        // pool is the unencrypted shared.db one. A pool over an encrypted
        // database would be a handle a rotation could not close.
        let pool_re = regex::Regex::new(r"SqlitePoolOptions|SqlitePool::connect").unwrap();
        let mut files = Vec::new();
        rs_files(&src_dir(), &mut files);
        let mut bad = Vec::new();
        for path in files {
            let rel = path
                .strip_prefix(src_dir())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if is_test_file(&rel) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let prod = production_text(&text);
            for m in pool_re.find_iter(prod) {
                let window_end = (m.end() + 400).min(prod.len());
                let window_end = (window_end..=prod.len())
                    .find(|i| prod.is_char_boundary(*i))
                    .unwrap_or(prod.len());
                if prod[m.start()..window_end].contains("connect_options_encrypted") {
                    bad.push(rel.clone());
                }
            }
        }
        assert!(
            bad.is_empty(),
            "a pool is built over an encrypted database: {bad:?}"
        );
    }

    // -- the resident-epoch slot (items.id=797) -----------------------------
    //
    // epoch_guard keeps ONE process-wide (user, epoch) slot, and cargo runs the
    // tests of one binary in parallel threads of one process, so two tests that
    // write it race (items.id=796). A lib test serializes on the shared
    // test_support::ENV_MUTEX; an integration test (tests/*.rs, its own
    // process) on a file-local mutex. This scan fails a test that writes the
    // slot without holding one. It reads TEST code, where the spawn scan above
    // reads production code.

    /// Test helpers that take the slot guard and hand it to the test that
    /// calls them: (file, fn, call, why). `file` is relative to src/ (or
    /// `tests/...` for an integration test); `call` is how a test names the
    /// helper. The scan checks every row: the fn exists in that file, locks,
    /// returns a value (a guard has to be handed out) and is still called.
    const SLOT_GUARD_HELPERS: &[(&str, &str, &str, &str)] = &[
        (
            "auth/idle_timeout.rs",
            "setup",
            "setup",
            "the TestEnv it returns holds ENV_MUTEX for the whole test",
        ),
        (
            "commands/auth.rs",
            "setup",
            "setup",
            "the TestEnv it returns holds ENV_MUTEX for the whole test",
        ),
        (
            "auth/key_rotation/tests.rs",
            "new",
            "Fixture::new",
            "the Fixture it returns holds ENV_MUTEX for the whole test",
        ),
        (
            "commands/key_rotation.rs",
            "new",
            "Harness::new",
            "the Harness it returns holds ENV_MUTEX for the whole test",
        ),
        (
            "tests/subkeys_resident_epoch.rs",
            "hold_resident_slot",
            "hold_resident_slot",
            "a process-local mutex: the integration binary is its own process",
        ),
    ];

    fn tests_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
    }

    /// Full-line comments blanked, line layout kept.
    fn blank_comments(text: &str) -> String {
        text.lines()
            .map(|l| {
                if l.trim_start().starts_with("//") {
                    ""
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every fn in `code` as (name, start offset, text up to the next fn).
    fn fn_windows(code: &str) -> Vec<(String, usize, &str)> {
        let fn_re =
            regex::Regex::new(r"(?m)^[ \t]*(?:pub(?:\([a-z]+\))?\s+)?(?:async\s+)?fn\s+(\w+)")
                .unwrap();
        let starts: Vec<(usize, String)> = fn_re
            .captures_iter(code)
            .map(|c| (c.get(0).unwrap().start(), c[1].to_owned()))
            .collect();
        starts
            .iter()
            .enumerate()
            .map(|(i, (s, name))| {
                let end = starts.get(i + 1).map_or(code.len(), |(e, _)| *e);
                (name.clone(), *s, &code[*s..end])
            })
            .collect()
    }

    /// Whether the fn starting at `fn_start` carries `#[test]` or
    /// `#[tokio::test..]`, looking up past blank lines and other attributes.
    fn has_test_attr(code: &str, fn_start: usize) -> bool {
        static ATTR: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let attr = ATTR.get_or_init(|| regex::Regex::new(r"^#\[(?:tokio::)?test\b").unwrap());
        for line in code[..fn_start].lines().rev() {
            let t = line.trim();
            if attr.is_match(t) {
                return true;
            }
            if !(t.is_empty() || t.starts_with("#[")) {
                return false;
            }
        }
        false
    }

    /// Offset of the first call to each name in `window` (`name(`, not a
    /// `fn name(` definition). Built once per fn: the scan asks about many
    /// names, and a pattern scan per name took ten seconds.
    fn call_index(window: &str) -> std::collections::HashMap<&str, usize> {
        static CALL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = CALL.get_or_init(|| regex::Regex::new(r"\b(\w+)\s*\(").unwrap());
        let mut index = std::collections::HashMap::new();
        for m in re.captures_iter(window).filter_map(|c| c.get(1)) {
            if !window[..m.start()].ends_with("fn ") {
                index.entry(m.as_str()).or_insert(m.start());
            }
        }
        index
    }

    /// Offset of the first `let <name> = ENV_MUTEX.lock()` (or
    /// `blocking_lock()`): a guard bound to a real name. A bare `_` drops it
    /// on that line, so it protects nothing.
    fn env_mutex_guard_pos(window: &str) -> Option<usize> {
        static GUARD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = GUARD.get_or_init(|| {
            regex::Regex::new(
                r"let\s+(?:mut\s+)?(\w+)\s*=\s*(?:crate::test_support::)?ENV_MUTEX\s*\.\s*(?:blocking_lock|lock)\s*\(\s*\)",
            )
            .unwrap()
        });
        first_named_bind_at(re, window, |_| true)
    }

    /// Offset of the first `let <name> = <call>(` with a real name (see above).
    fn helper_guard_pos(window: &str, call: &str) -> Option<usize> {
        static BIND: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = BIND.get_or_init(|| {
            regex::Regex::new(r"let\s+(?:mut\s+)?(\w+)\s*=\s*([\w:]+)\s*\(").unwrap()
        });
        first_named_bind_at(re, window, |c| &c[2] == call)
    }

    /// Offset of the first match of `re` whose capture 1 (the binding) is not
    /// a bare `_`.
    fn first_named_bind_at(
        re: &regex::Regex,
        window: &str,
        keep: impl Fn(&regex::Captures) -> bool,
    ) -> Option<usize> {
        re.captures_iter(window)
            .find(|c| &c[1] != "_" && keep(c))
            .map(|c| c.get(0).unwrap().start())
    }

    struct SlotSources {
        /// (path relative to src/, production code, comments blanked)
        prod: Vec<(String, String)>,
        /// Test code: src/ test files and test modules, and tests/**/*.rs as
        /// `tests/...`.
        test: Vec<(String, String)>,
    }

    fn slot_sources() -> SlotSources {
        let mut files = Vec::new();
        rs_files(&src_dir(), &mut files);
        files.sort();
        let all: Vec<(String, String)> = files
            .iter()
            .map(|p| {
                let rel = p
                    .strip_prefix(src_dir())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                (rel, std::fs::read_to_string(p).unwrap())
            })
            .collect();
        // Files that exist only for tests: `#[cfg(test)] mod x;` declares
        // d/x.rs or d/x/mod.rs (below d/<file stem>/ unless the declaring file
        // is a mod.rs, lib.rs or main.rs). is_test_file knows only the names.
        let decl_re = regex::Regex::new(
            r"(?m)^#\[cfg\(test\)\]\s*\n\s*(?:pub(?:\([a-z]+\))?\s+)?mod\s+(\w+)\s*;",
        )
        .unwrap();
        let mut declared = std::collections::BTreeSet::new();
        for (rel, text) in &all {
            let p = std::path::Path::new(rel);
            let dir = p.parent().unwrap_or(std::path::Path::new(""));
            let stem = p.file_stem().unwrap().to_string_lossy().into_owned();
            let base = if matches!(stem.as_str(), "mod" | "lib" | "main") {
                dir.to_path_buf()
            } else {
                dir.join(&stem)
            };
            for c in decl_re.captures_iter(text) {
                for cand in [
                    base.join(format!("{}.rs", &c[1])),
                    base.join(&c[1]).join("mod.rs"),
                ] {
                    declared.insert(cand.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        let mut out = SlotSources {
            prod: Vec::new(),
            test: Vec::new(),
        };
        for (rel, text) in all {
            if is_test_file(&rel) || declared.contains(&rel) {
                out.test.push((rel, blank_comments(&text)));
                continue;
            }
            // From an inline test module to the end of the file is test code
            // (clippy's items_after_test_module keeps production out of it).
            let prod = production_text(&text);
            out.test
                .push((rel.clone(), blank_comments(&text[prod.len()..])));
            out.prod.push((rel, blank_comments(prod)));
        }
        let mut files = Vec::new();
        rs_files(&tests_dir(), &mut files);
        files.sort();
        for p in files {
            let rel = p
                .strip_prefix(tests_dir())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.test.push((
                format!("tests/{rel}"),
                blank_comments(&std::fs::read_to_string(&p).unwrap()),
            ));
        }
        out
    }

    /// Production fns that write the slot: those that call set_resident, and,
    /// to a fixed point by name, every production fn that calls one of them
    /// (login -> finish_login -> install_session today). Returns (direct
    /// callers, all).
    fn production_slot_writers(
        prod: &[(String, String)],
    ) -> (
        std::collections::BTreeSet<String>,
        std::collections::BTreeSet<String>,
    ) {
        let calls: Vec<(String, std::collections::HashMap<&str, usize>)> = prod
            .iter()
            .flat_map(|(_, code)| fn_windows(code))
            .map(|(name, _, w)| (name, call_index(w)))
            .collect();
        let mut direct = std::collections::BTreeSet::new();
        for (name, index) in &calls {
            if index.contains_key("set_resident") {
                direct.insert(name.clone());
            }
        }
        let mut all = direct.clone();
        loop {
            let mut grew = false;
            for (name, index) in &calls {
                if !all.contains(name) && all.iter().any(|n| index.contains_key(n.as_str())) {
                    all.insert(name.clone());
                    grew = true;
                }
            }
            if !grew {
                return (direct, all);
            }
        }
    }

    #[test]
    fn every_resident_slot_writer_test_holds_the_slot_guard() {
        let src = slot_sources();
        let (direct, writers) = production_slot_writers(&src.prod);
        assert!(
            !direct.is_empty(),
            "found no production fn that calls set_resident: the derivation is broken, so this \
             scan would pass for the wrong reason"
        );

        let mut violations = Vec::new();
        let mut guarded_lib = 0;
        let mut guarded_integration = 0;
        for (rel, code) in &src.test {
            let rows: Vec<_> = SLOT_GUARD_HELPERS
                .iter()
                .filter(|(file, ..)| file == rel)
                .collect();
            let fns: Vec<_> = fn_windows(code)
                .into_iter()
                .map(|(name, start, w)| (name, start, w, call_index(w)))
                .collect();
            // The guard has to be taken before the first write.
            let guard_pos = |w: &str| {
                env_mutex_guard_pos(w)
                    .into_iter()
                    .chain(
                        rows.iter()
                            .filter_map(|(_, _, call, _)| helper_guard_pos(w, call)),
                    )
                    .min()
            };
            // Every slot write in `w`, in order: set_resident itself, and a
            // call to a writer (derived from production, or a test-side one).
            let writes = |index: &std::collections::HashMap<&str, usize>,
                          side: &std::collections::BTreeSet<String>| {
                let mut v: Vec<(usize, String)> = index
                    .get("set_resident")
                    .map(|at| (*at, "set_resident".to_owned()))
                    .into_iter()
                    .collect();
                for n in writers.iter().chain(side.iter()) {
                    if let Some(at) = index.get(n.as_str()) {
                        v.push((*at, n.clone()));
                    }
                }
                v.sort();
                v
            };
            // Test-side writers: a non-test fn that writes the slot without
            // taking the guard first (login_alice style) writes it for
            // whoever calls it. A fn that takes the guard itself, then writes,
            // is serialized for the whole of its body and is not one.
            let mut side = std::collections::BTreeSet::new();
            loop {
                let mut grew = false;
                for (name, start, w, index) in &fns {
                    if side.contains(name) || has_test_attr(code, *start) {
                        continue;
                    }
                    let Some((first, _)) = writes(index, &side).into_iter().next() else {
                        continue;
                    };
                    if !guard_pos(w).is_some_and(|g| g <= first) {
                        side.insert(name.clone());
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
            for (name, start, w, index) in &fns {
                let is_test = has_test_attr(code, *start);
                // A test also writes through a writer it calls; any other fn
                // only counts when it calls set_resident itself.
                let mut calls = writes(index, &side);
                calls.retain(|(_, n)| is_test || n == "set_resident");
                let Some((first, via)) = calls.into_iter().next() else {
                    continue;
                };
                if guard_pos(w).is_some_and(|g| g <= first) {
                    if rel.starts_with("tests/") {
                        guarded_integration += 1;
                    } else {
                        guarded_lib += 1;
                    }
                    continue;
                }
                let how = if via == "set_resident" {
                    "directly, through set_resident".to_owned()
                } else {
                    format!("through {via}")
                };
                violations.push(format!("{rel}::{name} ({how})"));
            }
        }
        assert!(
            violations.is_empty(),
            "these tests write the process-wide resident-epoch slot without holding a guard \
             taken before the write, so they race every other test that does: {violations:#?}\n\
             Fix: in a lib test bind the shared mutex for the whole test, \
             `let _lock = crate::test_support::ENV_MUTEX.lock().await;` (`.blocking_lock()` in a \
             sync test), under a real name: `let _ =` drops it on that line. In an integration \
             test (tests/*.rs, its own process) copy hold_resident_slot() from \
             tests/subkeys_resident_epoch.rs and bind it. A test that goes through a helper \
             needs a row in SLOT_GUARD_HELPERS (the scan checks the helper's body). Writers \
             derived from production code: {writers:?}. See items.id=796 and items.id=797."
        );

        // The scan must have seen guarded tests of both kinds, or its
        // detection is broken and it passes vacuously.
        assert!(guarded_lib > 0, "saw no guarded lib test writing the slot");
        assert!(
            guarded_integration > 0,
            "saw no guarded integration test writing the slot"
        );

        // Every helper row must really be a guard helper, and still be used.
        let mut bad_rows = Vec::new();
        for (file, fn_name, call, _why) in SLOT_GUARD_HELPERS {
            let Some((_, code)) = src.test.iter().find(|(rel, _)| rel == file) else {
                bad_rows.push(format!("{file}: no such test source"));
                continue;
            };
            let fns = fn_windows(code);
            let locks = fns.iter().any(|(n, _, w)| {
                let signature = w.split('{').next().unwrap_or("");
                n == fn_name
                    && signature.contains("->")
                    && if file.starts_with("tests/") {
                        w.contains(".lock(")
                    } else {
                        env_mutex_guard_pos(w).is_some()
                    }
            });
            if !locks {
                bad_rows.push(format!(
                    "{file}::{fn_name}: not found, takes no lock, or returns nothing"
                ));
            }
            if !fns
                .iter()
                .any(|(n, _, w)| n != fn_name && helper_guard_pos(w, call).is_some())
            {
                bad_rows.push(format!("{file}::{fn_name}: no test calls it any more"));
            }
        }
        assert!(
            bad_rows.is_empty(),
            "SLOT_GUARD_HELPERS rows that are not live guard helpers: {bad_rows:?}"
        );

        // One shared ENV_MUTEX. A second definition would make the name match
        // above lie: that was the three-copies bug of items.id=205.
        let dup_re = regex::Regex::new(r"\bstatic\s+(?:mut\s+)?ENV_MUTEX\b").unwrap();
        let dups: Vec<&String> = src
            .prod
            .iter()
            .chain(src.test.iter())
            .filter(|(rel, code)| rel != "test_support.rs" && dup_re.is_match(code))
            .map(|(rel, _)| rel)
            .collect();
        assert!(
            dups.is_empty(),
            "ENV_MUTEX is defined outside test_support.rs in {dups:?}: import the shared one"
        );
    }

    // -- the drain policy (Jason's stuck-writer ruling, items.id=778) ------

    fn quick() -> DrainPolicy {
        DrainPolicy::default()
    }

    #[tokio::test]
    async fn ask_reports_what_runs_and_changes_nothing() {
        let _serial = DRAIN_SERIAL.lock().await;
        let u = "drain-ask";
        let _reply = acquire(u, Kind::Reply).unwrap();
        let _run = acquire(u, Kind::FocusRun).unwrap();
        let err = drain_for_rotation(u, Choice::Ask, quick())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            DrainError::WritersActive {
                replies: 1,
                focus_runs: 1
            }
        );
        assert_eq!(phase(u), Phase::Open, "the gate is open again");
        assert!(!_reply.stop_token().is_cancelled(), "nothing was stopped");
    }

    #[tokio::test]
    async fn with_nothing_running_the_gate_closes_whatever_the_choice() {
        let _serial = DRAIN_SERIAL.lock().await;
        for (i, choice) in [Choice::Ask, Choice::FinishFirst, Choice::StopAndSave]
            .into_iter()
            .enumerate()
        {
            let u = format!("drain-idle-{i}");
            let guard = drain_for_rotation(&u, choice, quick()).await.unwrap();
            assert_eq!(phase(&u), Phase::Closed);
            drop(guard);
            assert_eq!(phase(&u), Phase::Open);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn finish_first_waits_for_the_reply_and_refuses_new_work_meanwhile() {
        let _serial = DRAIN_SERIAL.lock().await;
        let u = "drain-finish";
        let reply = acquire(u, Kind::Reply).unwrap();
        let waiting = tokio::spawn(async move {
            drain_for_rotation(u, Choice::FinishFirst, DrainPolicy::default()).await
        });
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!waiting.is_finished());
        // While the gate waits no new reply may start.
        assert_eq!(acquire(u, Kind::Reply).unwrap_err(), GateError::Rotating);
        drop(reply);
        let guard = waiting.await.unwrap().unwrap();
        assert_eq!(phase(u), Phase::Closed);
        drop(guard);
    }

    #[tokio::test(start_paused = true)]
    async fn finish_first_ends_at_the_ceiling_and_keep_waiting_starts_it_again() {
        let _serial = DRAIN_SERIAL.lock().await;
        let u = "drain-ceiling";
        let reply = acquire(u, Kind::Reply).unwrap();
        let err = drain_for_rotation(u, Choice::FinishFirst, DrainPolicy::default())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            DrainError::StillRunning {
                replies: 1,
                focus_runs: 0
            }
        );
        assert_eq!(
            phase(u),
            Phase::Open,
            "nothing changed and the gate is open"
        );
        // 'Keep waiting' is the same call again, with a fresh ceiling.
        let again = tokio::spawn(async move {
            drain_for_rotation(u, Choice::FinishFirst, DrainPolicy::default()).await
        });
        tokio::time::sleep(FINISH_FIRST_CEILING - Duration::from_secs(1)).await;
        assert!(!again.is_finished(), "the ceiling restarted");
        drop(reply);
        let _guard = again.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_during_the_wait_changes_nothing_and_reopens_the_gate() {
        let _serial = DRAIN_SERIAL.lock().await;
        let u = "drain-cancel";
        let reply = acquire(u, Kind::FocusRun).unwrap();
        let waiting = tokio::spawn(async move {
            drain_for_rotation(u, Choice::FinishFirst, DrainPolicy::default()).await
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(phase(u), Phase::Draining);
        assert!(cancel_wait(u));
        assert_eq!(waiting.await.unwrap().unwrap_err(), DrainError::Cancelled);
        assert_eq!(phase(u), Phase::Open);
        assert!(
            !reply.stop_token().is_cancelled(),
            "the work was not stopped"
        );
        assert!(!cancel_wait(u), "no wait is left to cancel");
    }

    #[tokio::test(start_paused = true)]
    async fn stop_and_save_trips_the_stop_and_proceeds_when_the_work_ends() {
        let _serial = DRAIN_SERIAL.lock().await;
        let u = "drain-stop";
        let reply = acquire(u, Kind::Reply).unwrap();
        let stop = reply.stop_token().clone();
        // A cooperative writer: ends when told to stop.
        let writer = tokio::spawn(async move {
            stop.cancelled().await;
            drop(reply);
        });
        let guard = drain_for_rotation(u, Choice::StopAndSave, DrainPolicy::default())
            .await
            .unwrap();
        writer.await.unwrap();
        assert_eq!(phase(u), Phase::Closed);
        drop(guard);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_and_save_reports_work_that_will_not_stop_and_never_force_kills() {
        let _serial = DRAIN_SERIAL.lock().await;
        let u = "drain-not-stopping";
        let reply = acquire(u, Kind::Reply).unwrap();
        let err = drain_for_rotation(u, Choice::StopAndSave, DrainPolicy::default())
            .await
            .unwrap_err();
        assert_eq!(err, DrainError::NotStopping);
        assert_eq!(phase(u), Phase::Open);
        assert!(reply.stop_token().is_cancelled(), "it was asked to stop");
        // Still held: nothing killed it.
        assert_eq!(counts(u).replies, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stuck_command_ends_the_wait_with_a_message_after_the_command_ceiling() {
        let _serial = DRAIN_SERIAL.lock().await;
        let u = "drain-straggler";
        let _cmd = acquire(u, Kind::Command).unwrap();
        let err = drain_for_rotation(u, Choice::FinishFirst, DrainPolicy::default())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            DrainError::StillRunning {
                replies: 0,
                focus_runs: 0
            }
        );
        assert_eq!(phase(u), Phase::Open);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_any_wait_needs_no_session() {
        let _serial = DRAIN_SERIAL.lock().await;
        let u = "drain-sessionless";
        let _reply = acquire(u, Kind::Reply).unwrap();
        let waiting = tokio::spawn(async move {
            drain_for_rotation(u, Choice::FinishFirst, DrainPolicy::default()).await
        });
        tokio::time::sleep(Duration::from_secs(1)).await;
        // Nobody signed in: no user to name, the running wait is cancelled.
        assert!(cancel_any_wait(None));
        assert_eq!(waiting.await.unwrap().unwrap_err(), DrainError::Cancelled);
        assert!(!cancel_any_wait(None), "nothing left to cancel");
    }

    #[tokio::test]
    async fn a_closed_gate_never_blocks_the_exempt_operations() {
        let _serial = DRAIN_SERIAL.lock().await;
        // The exempt commands never take a lease, so a Closed gate cannot
        // refuse them: cancel_wait and the status read only look at the gate.
        let u = "drain-exempt";
        let guard = drain_for_rotation(u, Choice::Ask, quick()).await.unwrap();
        assert_eq!(phase(u), Phase::Closed);
        assert!(!cancel_wait(u));
        assert_eq!(counts(u).total(), 0);
        drop(guard);
    }

    #[tokio::test]
    async fn a_command_lease_needs_a_session() {
        let registry = KeyRegistry::default();
        assert_eq!(
            command(&registry).await.unwrap_err(),
            GateError::NotLoggedIn
        );
    }
}

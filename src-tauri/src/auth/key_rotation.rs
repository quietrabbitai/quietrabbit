// src-tauri/src/auth/key_rotation.rs
//
// Data-key (DEK) rotation engine (items.id=630 stage C, decisions.id=893,
// Architecture/AUTH_MULTIUSER_ARCHITECTURE.md Section 4.0 and Architecture 8.6).
// Every password change, restore from the recovery phrase and new recovery
// phrase generates a NEW random DEK, re-keys every database and blob under the
// subkeys of the new DEK, and only then makes the new wraps live.
//
// ORDER OF A ROTATION (run_rotation):
//   1. Preflight, changing nothing: the account is at the epoch the caller
//      thinks, no rotation is pending, the process-wide rotating flag is taken
//      (epoch_guard::begin_rotation: every store open for the account is now
//      refused and the periodic sweeps stand down), the writer gate is closed
//      and empty (auth::writer_gate: the caller drained every writer first,
//      items.id=778), the account directory is inventoried (an encrypted file this code does
//      not recognise stops the rotation), every database opens under the OLD
//      key and passes cipher_integrity_check, and there is room for the
//      largest file.
//   2. Stage (user_store::stage_rotation, one transaction): the new DEK, the
//      new password wrap, recovery seal and sharing public key, the old DEK
//      wrapped under the new one, and one item row per file. From here a
//      crash is resumable.
//   3. Re-key each item in a fixed order (key_rotation_io::inventory). The
//      file itself is the truth, not the item row: it is probed under the NEW
//      key then the OLD key, so a crash between a re-key and its row is
//      recognised on resume.
//   4. Promotion step 1 (user_store::promote_rotation): the staged wraps become
//      the live wrapper, the epoch advances, the sharing public key is
//      replaced, state `verifying`. Refused unless every item is committed.
//   5. Read-back unlock: the live wrapper is read back and unwrapped with the
//      new credential and must give the new DEK.
//   6. Promotion step 2 (user_store::finish_rotation): staging and the wrapped
//      old DEK are cleared, `committed`.
//   7. The sharing hook runs (after_sharing_key_rotation), THEN the caller's
//      key registry is cleared completely while the rotating flag is still
//      held (items.id=778, decisions.id=896): the session ends and the user
//      signs in again, because the data key changed and any cached key would
//      be stale. The hook takes its keys from the RotationOutcome, never from
//      the registry, which still holds the OLD key while it runs.
//
// RETRY (items.id=778, decisions.id=896 point 7): a TRANSIENT failure of one
// file (a busy database, a leftover sidecar that cannot be removed yet) is
// retried inside the rotation, REKEY_ATTEMPTS times in all with REKEY_BACKOFF
// between them, before it counts as a failure. Each retry first re-checks that
// the rotation is still ours, and move_item is idempotent (it probes which key
// the file is under), so a retry can never double-apply. Hard errors are not
// retried. The rollback and the resume paths use the same bounded retry.
//
// FAILURE: an error while the process is alive, before promotion, rolls the
// rotation back automatically (every file back to the old key, state
// `rolled_back`, nothing changed for the user). After promotion nothing is
// rolled back: the data is on the new key, so the rotation is finished by
// resume. A process death at any point is resumed FORWARD at the next unlock
// (resume_rotation); explicit cancel (cancel_rotation) rolls back instead.
//
// WHAT RESUME NEEDS: the NEW DEK, reachable only through the staged password
// wrap (new password) or the staged recovery seal (recovery phrase), see
// new_dek_from_staged_password / new_dek_from_staged_phrase. The old DEK comes
// out of the pending row. An unlock with the OLD password alone cannot resume
// (decided with the plan); login reports an interrupted change instead.
//
// THE ENGINE CHECKS THE KEY EPOCH ITSELF: it uses raw connections and explicit
// keys, never the store openers, so it must not rely on their epoch guard
// (key_rotation_io.rs is exempt from the guard on that basis). It reads the
// wrapper before staging, again before every item (epoch and the pending flag
// must still be ours), and promotion is compare-and-set on the epoch.
//
// STALE FILES: `-wal`, `-journal` and `.tmp` files are deleted before and
// after each re-key, but only when that cannot discard live data: a database's
// sidecars are purged inside rekey_db_file once it has proved it holds the file
// alone, a database that will not open because of sidecars goes through
// settle_stale_sidecars (quarantine, probe, delete or restore), and a blob's
// `.tmp` (old-key ciphertext of an earlier interrupted write) is deleted before
// and after its rewrite.
//
// NOTHING HERE TOUCHES GROUP KEYS: they live inside each persona's personal.db
// (re-keyed with it) and group.db is keyed by the group key itself.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use thiserror::Error;
use x25519_dalek::StaticSecret;
use zeroize::Zeroizing;

use crate::auth::data_key::{self, DataKeyError, SecretBytes};
use crate::auth::epoch_guard;
use crate::auth::kdf;
use crate::auth::key_rotation_io::{self as io, DbKeyState, InventoryItem, ItemTarget};
use crate::auth::registry::KeyRegistry;
use crate::auth::sharing_keypair;
use crate::auth::user_store::{
    self, PendingRotation, RotationDirection, RotationItem, RotationKind, RotationState,
    StagedRotation, UserStoreError,
};
use crate::auth::writer_gate::{self, GateCheckError};
use crate::persistence::ingest_blob::{self, BlobRekey};

/// Attempts per file for a transient failure (the first try included).
pub const REKEY_ATTEMPTS: usize = 5;
/// Sleep before attempts 2 to 5: about 3.75 s of waiting per file at worst.
pub const REKEY_BACKOFF: [std::time::Duration; REKEY_ATTEMPTS - 1] = [
    std::time::Duration::from_millis(250),
    std::time::Duration::from_millis(500),
    std::time::Duration::from_millis(1000),
    std::time::Duration::from_millis(2000),
];

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Preparing,
    Rekeying,
    Verifying,
    Finishing,
    Resuming,
    RollingBack,
    Complete,
    RolledBack,
    Failed,
    /// The rotation was refused or failed after its first report (`Preparing`,
    /// `Resuming` or `RollingBack`) without reaching an outcome: before
    /// anything was staged, or, for a resume or cancel, before any item moved
    /// (or, for a promoted rotation, with its state left as it was). There is
    /// nothing to announce: the caller's own error says why. It only closes the
    /// report that the first event opened (items.id=780, items.id=790).
    NotStarted,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Preparing => "preparing",
            Phase::Rekeying => "rekeying",
            Phase::Verifying => "verifying",
            Phase::Finishing => "finishing",
            Phase::Resuming => "resuming",
            Phase::RollingBack => "rolling_back",
            Phase::Complete => "complete",
            Phase::RolledBack => "rolled_back",
            Phase::Failed => "failed",
            Phase::NotStarted => "not_started",
        }
    }
}

/// One progress report. `current` and `total` are BYTES of items done and to
/// do, or `None` when no honest byte count exists (before the account has been
/// inventoried, and during a single opaque step such as one large database's
/// `PRAGMA rekey`; items.id=780). `message` is a display label, never a path
/// or an id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressEvent {
    pub rotation_id: String,
    pub phase: Phase,
    pub current: Option<u64>,
    pub total: Option<u64>,
    pub message: Option<String>,
}

pub trait ProgressSink: Send + Sync {
    fn emit(&self, event: &ProgressEvent);
}

pub struct NoProgress;

impl ProgressSink for NoProgress {
    fn emit(&self, _event: &ProgressEvent) {}
}

/// The process-wide sink every rotation reports to unless its environment
/// names another. main.rs installs the Tauri event emitter at startup
/// (commands::key_rotation::TauriRotationSink); without one (tests) events
/// are dropped. It also remembers the LAST event so a webview that reloads or
/// mounts late can ask where a rotation stands (get_rotation_status).
pub struct GlobalProgress {
    sink: Mutex<Option<Arc<dyn ProgressSink>>>,
    last: Mutex<Option<ProgressEvent>>,
}

impl GlobalProgress {
    const fn new() -> Self {
        Self {
            sink: Mutex::new(None),
            last: Mutex::new(None),
        }
    }
}

impl ProgressSink for GlobalProgress {
    fn emit(&self, event: &ProgressEvent) {
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(event.clone());
        let sink = self.sink.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(sink) = sink {
            sink.emit(event);
        }
    }
}

static GLOBAL_PROGRESS: GlobalProgress = GlobalProgress::new();

/// Install (or remove) the process-wide progress sink.
pub fn set_global_progress_sink(sink: Option<Arc<dyn ProgressSink>>) {
    *GLOBAL_PROGRESS
        .sink
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = sink;
}

/// The most recent progress event of any rotation in this process.
pub fn last_progress() -> Option<ProgressEvent> {
    GLOBAL_PROGRESS
        .last
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// Forget the remembered event (a new session or a finished report).
pub fn clear_last_progress() {
    *GLOBAL_PROGRESS
        .last
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = None;
}

// ---------------------------------------------------------------------------
// Requests, environment, outcome
// ---------------------------------------------------------------------------

/// Argon2id cost for the NEW password key. Defaults are the account defaults;
/// tests lower them.
#[derive(Debug, Clone, Copy)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            memory_kib: kdf::DEFAULT_ARGON2_MEMORY_KIB,
            iterations: kdf::DEFAULT_ARGON2_ITERATIONS,
            parallelism: kdf::DEFAULT_ARGON2_PARALLELISM,
        }
    }
}

/// What the rotation is for, with the credential material it needs. The
/// caller has already authenticated: the old password for a change, the typed
/// phrase for a restore.
pub enum RotationRequest {
    /// New password; the recovery phrase is unchanged.
    PasswordChange { new_password: Zeroizing<Vec<u8>> },
    /// New password after the phrase unsealed the old DEK.
    Restore { new_password: Zeroizing<Vec<u8>> },
    /// New phrase: its entropy (already shown to and confirmed by the user)
    /// and the account password (unchanged, re-wrapped under a new salt).
    NewPhrase {
        password: Zeroizing<Vec<u8>>,
        recovery_entropy: SecretBytes,
    },
}

impl RotationRequest {
    pub fn kind(&self) -> RotationKind {
        match self {
            RotationRequest::PasswordChange { .. } => RotationKind::PasswordChange,
            RotationRequest::Restore { .. } => RotationKind::Restore,
            RotationRequest::NewPhrase { .. } => RotationKind::NewPhrase,
        }
    }
    fn password(&self) -> &[u8] {
        match self {
            RotationRequest::PasswordChange { new_password }
            | RotationRequest::Restore { new_password } => new_password,
            RotationRequest::NewPhrase { password, .. } => password,
        }
    }
}

/// Test-only crash/fault injection points. A `SimulatedCrash` leaves every
/// piece of state exactly as a dead process would, with no cleanup.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultPoint {
    AfterStage,
    /// After item `n` was re-keyed on disk, before its row was updated.
    AfterItemRekeyBeforeRow(usize),
    /// After item `n`'s row was marked committed.
    AfterItemCommitted(usize),
    AfterAllItems,
    AfterPromote,
    AfterFinishBeforeInstall,
    /// During a rollback, after item `n` was undone.
    RollbackAfterItem(usize),
    /// A recoverable ERROR (not a crash) after item `n` was committed: the
    /// process is alive, so the rotation rolls back.
    FailAfterItemCommitted(usize),
    /// A recoverable error right after promotion step 1.
    FailAfterPromote,
}

pub struct RotationEnv<'a> {
    pub pool: &'a sqlx::SqlitePool,
    pub user_id: &'a str,
    /// `<data root>/users/<user_id>`.
    pub user_dir: PathBuf,
    /// Require the writer gate to be closed and empty before anything starts
    /// (items.id=778). The commands that start a rotation drain and close the
    /// gate first and set this; tests and the unlock-time resume and cancel
    /// paths, which have no writers to drain, leave it off.
    pub require_closed_gate: bool,
    /// The session to swap to the new key at the end. `None` for a rotation
    /// performed on behalf of another process (and in tests).
    pub registry: Option<&'a KeyRegistry>,
    pub progress: &'a dyn ProgressSink,
    pub kdf: KdfParams,
    /// Free bytes on the filesystem holding a path. Injectable.
    pub free_space: fn(&Path) -> Option<u64>,
    /// Sleeps between the attempts at a transiently failing file. Injectable.
    pub retry_backoff: [std::time::Duration; REKEY_ATTEMPTS - 1],
    #[cfg(test)]
    pub faults: Vec<FaultPoint>,
    /// Test only: make item `.0` fail transiently `.1` more times.
    #[cfg(test)]
    pub transient_failures: Option<(usize, std::sync::atomic::AtomicUsize)>,
}

impl<'a> RotationEnv<'a> {
    pub fn new(pool: &'a sqlx::SqlitePool, user_id: &'a str, user_dir: PathBuf) -> Self {
        Self {
            pool,
            user_id,
            user_dir,
            require_closed_gate: false,
            registry: None,
            progress: &GLOBAL_PROGRESS,
            kdf: KdfParams::default(),
            free_space: io::available_space,
            retry_backoff: REKEY_BACKOFF,
            #[cfg(test)]
            faults: Vec::new(),
            #[cfg(test)]
            transient_failures: None,
        }
    }

    #[cfg(test)]
    fn fault(&self, point: FaultPoint) -> Result<(), RotationError> {
        if !self.faults.contains(&point) {
            return Ok(());
        }
        match point {
            FaultPoint::FailAfterItemCommitted(_) | FaultPoint::FailAfterPromote => {
                Err(RotationError::Item {
                    rel_path: "(injected)".into(),
                    detail: "injected failure".into(),
                })
            }
            _ => Err(RotationError::SimulatedCrash),
        }
    }
}

/// The result of a completed rotation.
pub struct RotationOutcome {
    pub rotation_id: String,
    pub kind: RotationKind,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub new_dek: SecretBytes,
    pub old_sharing_public_key: Option<[u8; 32]>,
    pub new_sharing_public_key: [u8; 32],
}

impl std::fmt::Debug for RotationOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RotationOutcome")
            .field("rotation_id", &self.rotation_id)
            .field("kind", &self.kind)
            .field("from_epoch", &self.from_epoch)
            .field("to_epoch", &self.to_epoch)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum ResumeOutcome {
    /// The rotation was finished.
    Completed(RotationOutcome),
    /// The rotation was undone (an interrupted rollback finished, or an
    /// explicit cancel): every file is back on the old key.
    RolledBack,
}

#[derive(Debug, Error)]
pub enum RotationError {
    #[error("no key wrapper exists for this account")]
    NoWrapper,
    #[error("a key rotation is already in progress for this account")]
    RotationInProgress,
    #[error("the account's key was changed elsewhere")]
    EpochChanged,
    #[error("{0} task(s) are still running, wait for them or cancel them")]
    ActiveRuns(usize),
    /// A start path asked for a closed writer gate and the caller had not
    /// drained and closed it. A programming error, never a user condition.
    #[error("the writer gate was not closed before the rotation started")]
    GateNotClosed,
    #[error("unrecognised encrypted file in the account directory: {0}")]
    UnknownEncryptedFile(String),
    #[error("the account has no integration_keys.db to prove the recovered key against")]
    NoProofDatabase,
    #[error("a database cannot be opened with the current key: {rel_path}: {detail}")]
    DatabaseUnreadable { rel_path: String, detail: String },
    #[error("not enough free disk space: need about {needed} bytes, {available} available")]
    InsufficientSpace { needed: u64, available: u64 },
    #[error("could not determine the free disk space")]
    FreeSpaceUnknown,
    /// Another connection holds the file, so nothing was done to it. Retry
    /// once the other use ends.
    #[error("a database is in use by something else: {rel_path}")]
    Busy { rel_path: String },
    #[error("re-keying {rel_path} failed: {detail}")]
    Item { rel_path: String, detail: String },
    /// A journal or WAL file is still beside the database; transient, retried.
    #[error("a leftover journal or WAL file could not be cleared beside {rel_path}")]
    LeftoverSidecar { rel_path: String },
    #[error("the new wrapper did not verify after it was written: {0}")]
    ReadBackFailed(String),
    #[error("the pending rotation cannot be resumed: {0}")]
    CannotResume(String),
    /// An explicit cancel of a rotation whose new wraps are already live: the
    /// data is on the new key and the rotation can only be finished.
    #[error("the change is already complete on disk and can only be finished")]
    AlreadyPromoted,
    #[error("the recovery phrase does not belong to this rotation")]
    WrongPhrase,
    /// An error before promotion; every file is back on the old key and the
    /// rotation is cleared. Nothing changed for the user.
    #[error("the change could not be completed and was undone: {cause}")]
    RolledBack { cause: String },
    /// An error that left the rotation unfinished (either it failed and could
    /// not be undone, or it failed after promotion). It resumes at the next
    /// unlock with the new password or the recovery phrase.
    #[error("the change did not finish and will be completed at the next unlock: {cause}")]
    NeedsResume { cause: String },
    #[error(transparent)]
    Store(#[from] UserStoreError),
    #[error(transparent)]
    Crypto(#[from] DataKeyError),
    #[error(transparent)]
    Kdf(#[from] kdf::AuthError),
    #[error(transparent)]
    Subkey(#[from] crate::auth::subkeys::SubkeyError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("key epoch check failed: {0}")]
    Epoch(String),
    #[cfg(test)]
    #[error("simulated crash")]
    SimulatedCrash,
}

impl RotationError {
    /// Whether a bounded retry can help: another connection still holds the
    /// file, or a leftover sidecar could not be cleared yet. Never a key or a
    /// data problem.
    pub fn is_transient(&self) -> bool {
        use std::io::ErrorKind::{Interrupted, ResourceBusy, TimedOut, WouldBlock};
        match self {
            RotationError::Busy { .. } | RotationError::LeftoverSidecar { .. } => true,
            RotationError::Io(e) => {
                matches!(e.kind(), Interrupted | ResourceBusy | TimedOut | WouldBlock)
            }
            _ => false,
        }
    }

    fn is_crash(&self) -> bool {
        #[cfg(test)]
        {
            matches!(self, RotationError::SimulatedCrash)
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

impl From<io::InventoryError> for RotationError {
    fn from(e: io::InventoryError) -> Self {
        match e {
            io::InventoryError::UnknownEncryptedFile(p) => RotationError::UnknownEncryptedFile(p),
            io::InventoryError::Io(e) => RotationError::Io(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Credentials used for the read-back unlock
// ---------------------------------------------------------------------------

/// How the new wrapper is read back: by the password key, or by the typed
/// recovery phrase's entropy.
pub enum Credential<'a> {
    Password(&'a [u8; kdf::MASTER_KEY_LEN]),
    Phrase(&'a [u8; kdf::MASTER_KEY_LEN]),
}

// ---------------------------------------------------------------------------
// Shared state of one run
// ---------------------------------------------------------------------------

struct Run<'a> {
    env: &'a RotationEnv<'a>,
    rotation_id: String,
    kind: RotationKind,
    from_epoch: u64,
    to_epoch: u64,
    old_dek: SecretBytes,
    new_dek: SecretBytes,
    items: Vec<RotationItem>,
    total_bytes: u64,
}

impl Run<'_> {
    fn emit(&self, phase: Phase, current: u64, message: Option<&str>) {
        self.env.progress.emit(&ProgressEvent {
            rotation_id: self.rotation_id.clone(),
            phase,
            current: Some(current),
            total: Some(self.total_bytes),
            message: message.map(str::to_owned),
        });
    }

    /// Bytes of the items in `state`: what a run has done before it starts,
    /// whether it is a fresh run or a resume (`Committed` for a forward run,
    /// `RolledBack` for a rollback).
    fn bytes_in_state(&self, state: RotationState) -> u64 {
        self.items
            .iter()
            .filter(|i| i.state == state)
            .map(|i| i.bytes)
            .sum()
    }

    /// Report that `item` is about to be re-keyed, `done` bytes of items being
    /// finished. An item that is one opaque step (see `is_opaque_step`) is
    /// reported WITHOUT byte counts: any percentage shown across it would be a
    /// guess, because the step has no progress hook and its speed depends on
    /// the machine.
    fn emit_item(&self, phase: Phase, done: u64, item: &RotationItem) {
        let label = Some(label_of(item));
        if is_opaque_step(item) {
            self.env.progress.emit(&ProgressEvent {
                rotation_id: self.rotation_id.clone(),
                phase,
                current: None,
                total: None,
                message: label.map(str::to_owned),
            });
        } else {
            self.emit(phase, done, label);
        }
    }

    /// The engine's own epoch check: the wrapper must still be at the epoch
    /// this rotation started from, with this rotation as the pending one
    /// (before promotion), or at the target epoch (after).
    async fn check_still_ours(&self, promoted: bool) -> Result<(), RotationError> {
        let state = user_store::get_key_wrapper_state(self.env.pool, self.env.user_id)
            .await?
            .ok_or(RotationError::NoWrapper)?;
        let expected_epoch = if promoted {
            self.to_epoch
        } else {
            self.from_epoch
        };
        if state.wrapper.key_epoch != expected_epoch
            || state.pending_rotation.as_deref() != Some(self.rotation_id.as_str())
        {
            return Err(RotationError::EpochChanged);
        }
        Ok(())
    }

    fn abs(&self, item: &RotationItem) -> PathBuf {
        self.env.user_dir.join(&item.rel_path)
    }
}

/// A RekeyError on one file, as the rotation's error: busy stays busy.
fn file_err(rel_path: &str, e: io::RekeyError) -> RotationError {
    match e {
        io::RekeyError::Busy => RotationError::Busy {
            rel_path: rel_path.to_owned(),
        },
        io::RekeyError::LeftoverSidecar(_) => RotationError::LeftoverSidecar {
            rel_path: rel_path.to_owned(),
        },
        other => RotationError::Item {
            rel_path: rel_path.to_owned(),
            detail: other.to_string(),
        },
    }
}

fn display_label(target: &ItemTarget) -> &'static str {
    use crate::auth::subkeys::DbPurpose::*;
    match target {
        ItemTarget::Blob(_) => "Documents",
        ItemTarget::Db(d) => match d.purpose {
            Personal => "Personal information",
            Outputs => "Saved results",
            Messages => "Conversations",
            PlanState | DomainContext => "Topics",
            ViewCache => "Shared views",
            IntegrationKeys | CloudCookies => "Connections",
            IngestBlob => "Documents",
        },
    }
}

// ---------------------------------------------------------------------------
// Re-keying one item, in either direction
// ---------------------------------------------------------------------------

/// Move one item from `src` (dek, epoch) to `dst`. Probes the file first, so
/// an item already on the destination key is recognised and left alone.
async fn move_item(
    run: &Run<'_>,
    item: &RotationItem,
    src: (&[u8; kdf::MASTER_KEY_LEN], u64),
    dst: (&[u8; kdf::MASTER_KEY_LEN], u64),
) -> Result<(), RotationError> {
    let item_err = |detail: String| RotationError::Item {
        rel_path: item.rel_path.clone(),
        detail,
    };
    let target = io::target_for_rel_path(&item.rel_path)
        .ok_or_else(|| item_err("not a recognised account file".into()))?;
    let abs = run.abs(item);
    if !abs.is_file() {
        return Err(item_err("the file is missing".into()));
    }
    let user = run.env.user_id;

    match target {
        ItemTarget::Db(t) => {
            let src_hex = io::db_key_for(&t, src.0, user, src.1)?;
            let dst_hex = io::db_key_for(&t, dst.0, user, dst.1)?;
            let mut state = io::db_key_state(&abs, &src_hex, &dst_hex)
                .await
                .map_err(|e| file_err(&item.rel_path, e))?;
            if state == DbKeyState::Neither {
                // Both keys said "not a database". That can be stale sidecars
                // rather than the key: settle them (only acts on that failure;
                // busy or any other error aborts untouched) and look again.
                io::settle_stale_sidecars(&abs, &[&src_hex, &dst_hex])
                    .await
                    .map_err(|e| file_err(&item.rel_path, e))?;
                state = io::db_key_state(&abs, &src_hex, &dst_hex)
                    .await
                    .map_err(|e| file_err(&item.rel_path, e))?;
            }
            match state {
                DbKeyState::New => {
                    io::verify_db_key(&abs, &dst_hex)
                        .await
                        .map_err(|e| file_err(&item.rel_path, e))?;
                }
                DbKeyState::Old => {
                    io::rekey_db_file(&abs, &src_hex, &dst_hex)
                        .await
                        .map_err(|e| file_err(&item.rel_path, e))?;
                }
                DbKeyState::Neither => {
                    return Err(item_err(
                        "the file opens under neither the old nor the new key".into(),
                    ));
                }
            }
            io::remove_leftover_sidecars(&abs)?;
        }
        ItemTarget::Blob(b) => {
            let tmp = ingest_blob::stale_tmp_path(&abs);
            io::remove_if_exists(&tmp)?;
            ingest_blob::reencrypt_blob(
                &abs,
                user,
                &b.persona_id,
                &b.output_id,
                b.version,
                src.0,
                src.1,
                dst.0,
                dst.1,
            )
            .await
            .map(|_: BlobRekey| ())
            .map_err(|e| item_err(e.to_string()))?;
            let opens = ingest_blob::blob_opens_under(
                &abs,
                user,
                &b.persona_id,
                &b.output_id,
                b.version,
                dst.0,
                dst.1,
            )
            .await
            .map_err(|e| item_err(e.to_string()))?;
            if !opens {
                return Err(item_err("the rewritten blob does not open".into()));
            }
            io::remove_if_exists(&tmp)?;
        }
    }
    Ok(())
}

/// Databases at least this big are re-keyed by one `PRAGMA rekey`, which
/// reports no progress while it runs, so the bar goes indeterminate for them
/// (items.id=780, decisions.id=899). A size cut-off, not a speed estimate:
/// smaller databases finish quickly enough that switching the bar's mode for
/// each of them would only flicker.
pub const OPAQUE_STEP_MIN_BYTES: u64 = 64 * 1024 * 1024;

/// Whether re-keying `item` is one step with no progress inside it. Blobs are
/// whole files that each report on their own, so only a large database counts.
fn is_opaque_step(item: &RotationItem) -> bool {
    !item.is_blob && item.bytes >= OPAQUE_STEP_MIN_BYTES
}

fn label_of(item: &RotationItem) -> &'static str {
    io::target_for_rel_path(&item.rel_path)
        .map(|t| display_label(&t))
        .unwrap_or("Files")
}

/// `move_item` with the bounded retry for transient failures (see the module
/// header). Between attempts the rotation must still be ours; a hard error, or
/// the last transient one, is returned.
async fn move_item_with_retry(
    run: &Run<'_>,
    n: usize,
    item: &RotationItem,
    src: (&[u8; kdf::MASTER_KEY_LEN], u64),
    dst: (&[u8; kdf::MASTER_KEY_LEN], u64),
    promoted: bool,
) -> Result<(), RotationError> {
    let mut attempt = 0;
    loop {
        #[cfg(test)]
        let injected = match &run.env.transient_failures {
            Some((at, left))
                if *at == n
                    && left
                        .fetch_update(
                            std::sync::atomic::Ordering::SeqCst,
                            std::sync::atomic::Ordering::SeqCst,
                            |v| v.checked_sub(1),
                        )
                        .is_ok() =>
            {
                Err(RotationError::Busy {
                    rel_path: item.rel_path.clone(),
                })
            }
            _ => Ok(()),
        };
        #[cfg(not(test))]
        let injected: Result<(), RotationError> = {
            let _ = n;
            Ok(())
        };
        let result = match injected {
            Ok(()) => move_item(run, item, src, dst).await,
            Err(e) => Err(e),
        };
        match result {
            Err(e) if e.is_transient() && attempt + 1 < REKEY_ATTEMPTS => {
                log::warn!(
                    "key_rotation: transient failure on attempt {} of {REKEY_ATTEMPTS}, retrying: {e}",
                    attempt + 1
                );
                tokio::time::sleep(run.env.retry_backoff[attempt]).await;
                run.check_still_ours(promoted).await?;
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Re-key every item not yet committed, in order, recording each.
async fn drive_forward(run: &Run<'_>) -> Result<(), RotationError> {
    let pool = run.env.pool;
    user_store::set_rotation_progress(
        pool,
        &run.rotation_id,
        RotationState::InProgress,
        RotationDirection::Forward,
        None,
    )
    .await?;

    let mut done = run.bytes_in_state(RotationState::Committed);
    for (n, item) in run.items.iter().enumerate() {
        if item.state == RotationState::Committed {
            continue;
        }
        run.check_still_ours(false).await?;
        run.emit_item(Phase::Rekeying, done, item);
        user_store::set_rotation_item_state(
            pool,
            &run.rotation_id,
            &item.rel_path,
            RotationState::InProgress,
            None,
        )
        .await?;

        let moved = move_item_with_retry(
            run,
            n,
            item,
            (&run.old_dek, run.from_epoch),
            (&run.new_dek, run.to_epoch),
            false,
        )
        .await;
        if let Err(e) = moved {
            let _ = user_store::set_rotation_item_state(
                pool,
                &run.rotation_id,
                &item.rel_path,
                RotationState::Failed,
                Some(&e.to_string()),
            )
            .await;
            return Err(e);
        }
        #[cfg(test)]
        run.env.fault(FaultPoint::AfterItemRekeyBeforeRow(n))?;
        user_store::set_rotation_item_state(
            pool,
            &run.rotation_id,
            &item.rel_path,
            RotationState::Committed,
            None,
        )
        .await?;
        done += item.bytes;
        #[cfg(test)]
        {
            run.env.fault(FaultPoint::AfterItemCommitted(n))?;
            run.env.fault(FaultPoint::FailAfterItemCommitted(n))?;
        }
        #[cfg(not(test))]
        let _ = n;
    }
    // Nothing may have appeared since the inventory: a file written by
    // something else during the rotation would be left on the old key by a
    // promotion that does not know about it.
    recheck_inventory(run).await?;
    #[cfg(test)]
    run.env.fault(FaultPoint::AfterAllItems)?;
    Ok(())
}

/// Inventory the account again after the last item and require that every
/// file found is in the staged set. Anything else fails into the rollback
/// path (before promotion), so the new wraps never go live over a file that
/// was not re-keyed.
async fn recheck_inventory(run: &Run<'_>) -> Result<(), RotationError> {
    let inv = io::inventory(&run.env.user_dir).await?;
    let staged: std::collections::HashSet<&str> =
        run.items.iter().map(|i| i.rel_path.as_str()).collect();
    if let Some(extra) = inv
        .items
        .iter()
        .find(|i| !staged.contains(i.rel_path.as_str()))
    {
        return Err(RotationError::Item {
            rel_path: extra.rel_path.clone(),
            detail: "a file appeared during the rotation that was not part of it".into(),
        });
    }
    if let Some(tmp) = inv.stale_tmp.first() {
        return Err(RotationError::Item {
            rel_path: tmp
                .strip_prefix(&run.env.user_dir)
                .unwrap_or(tmp)
                .to_string_lossy()
                .into_owned(),
            detail: "a blob was being written during the rotation".into(),
        });
    }
    Ok(())
}

/// Undo every item back to the old key, then clear the rotation.
async fn roll_back(run: &Run<'_>, cause: &str) -> Result<(), RotationError> {
    let pool = run.env.pool;
    user_store::set_rotation_progress(
        pool,
        &run.rotation_id,
        RotationState::InProgress,
        RotationDirection::Rollback,
        Some(cause),
    )
    .await?;

    // Items a previous (interrupted) rollback already undid are skipped and
    // counted as done up front, so a resumed bar starts where that one stopped.
    // `RolledBack` is only written after the move succeeded; any other state,
    // `Failed` included, is walked again.
    let mut done = run.bytes_in_state(RotationState::RolledBack);
    // Newest first: the reverse of the order they were moved.
    for (n, item) in run.items.iter().enumerate().rev() {
        if item.state == RotationState::RolledBack {
            continue;
        }
        run.emit_item(Phase::RollingBack, done, item);
        let moved = move_item_with_retry(
            run,
            n,
            item,
            (&run.new_dek, run.to_epoch),
            (&run.old_dek, run.from_epoch),
            false,
        )
        .await;
        match moved {
            Ok(()) => {
                user_store::set_rotation_item_state(
                    pool,
                    &run.rotation_id,
                    &item.rel_path,
                    RotationState::RolledBack,
                    None,
                )
                .await?;
            }
            Err(e) => {
                let _ = user_store::set_rotation_item_state(
                    pool,
                    &run.rotation_id,
                    &item.rel_path,
                    RotationState::Failed,
                    Some(&e.to_string()),
                )
                .await;
                return Err(e);
            }
        }
        done += item.bytes;
        #[cfg(test)]
        run.env.fault(FaultPoint::RollbackAfterItem(n))?;
        #[cfg(not(test))]
        let _ = n;
    }
    user_store::abort_rotation(
        pool,
        &run.rotation_id,
        RotationState::RolledBack,
        Some(cause),
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Promotion, read-back and the end of a rotation
// ---------------------------------------------------------------------------

/// Promotion step 1 → read-back unlock → promotion step 2.
async fn promote_and_finish(
    run: &Run<'_>,
    credential: Credential<'_>,
) -> Result<(), RotationError> {
    let pool = run.env.pool;
    let user = run.env.user_id;
    run.emit(Phase::Verifying, run.total_bytes, None);

    user_store::promote_rotation(pool, &run.rotation_id, run.from_epoch).await?;
    #[cfg(test)]
    {
        run.env.fault(FaultPoint::AfterPromote)?;
        run.env.fault(FaultPoint::FailAfterPromote)?;
    }

    run.emit(Phase::Finishing, run.total_bytes, None);
    run.check_still_ours(true).await?;
    read_back(run, user, &credential).await?;
    user_store::finish_rotation(pool, &run.rotation_id).await?;
    scrub_shared_db(pool).await;
    Ok(())
}

/// The "new wrap verifies by an unlock" check: read the live wrapper back
/// from shared.db and open it with the new credential.
async fn read_back(
    run: &Run<'_>,
    user: &str,
    credential: &Credential<'_>,
) -> Result<(), RotationError> {
    let state = user_store::get_key_wrapper_state(run.env.pool, user)
        .await?
        .ok_or(RotationError::NoWrapper)?;
    let w = &state.wrapper;
    if w.key_epoch != run.to_epoch {
        return Err(RotationError::ReadBackFailed(
            "the stored epoch is not the new one".into(),
        ));
    }
    let opened = match credential {
        Credential::Password(pk) => {
            data_key::unwrap_dek(pk, &w.password_wrap, user, w.key_epoch, w.format_version)
        }
        Credential::Phrase(entropy) => {
            let (secret, public) = data_key::derive_recovery_keypair(entropy, user);
            if public.as_bytes() != &w.recovery_public_key {
                return Err(RotationError::ReadBackFailed(
                    "the phrase does not match the stored recovery key".into(),
                ));
            }
            data_key::unseal_dek_with_recovery_key(
                &secret,
                &w.recovery_seal,
                user,
                w.key_epoch,
                w.format_version,
            )
        }
    }
    .map_err(|e| RotationError::ReadBackFailed(e.to_string()))?;
    if *opened != *run.new_dek {
        return Err(RotationError::ReadBackFailed(
            "the stored wrapper opens to a different key".into(),
        ));
    }
    Ok(())
}

/// Best effort: fold shared.db's WAL back so the cleared wraps do not linger
/// in it.
async fn scrub_shared_db(pool: &sqlx::SqlitePool) {
    if let Ok(mut conn) = pool.acquire().await {
        let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut *conn)
            .await;
    }
}

/// Clear the caller's key registry completely (key, resident epoch, pending
/// phrase and recovery slots) after a rotation (items.id=778). Done while the
/// rotating flag is still held, so no sweep or idle check sees the epoch change
/// while the old key is still resident. The command layer then ends the rest of
/// the session (group keys, connect sessions, session rows) and tells the
/// webview. A rotation run on behalf of another process has no registry.
async fn end_session(env: &RotationEnv<'_>) {
    if let Some(registry) = env.registry {
        // Also clears the process-wide resident epoch of the account.
        registry.clear().await;
    }
}

/// The sharing-key hook (items.id=630, for items.id=659). Called once per
/// completed rotation, after the session swap, best effort: it must never
/// fail a rotation that has already committed.
///
/// WHAT STOPS WORKING AFTER A ROTATION, until items.id=659 (group re-wrap)
/// lands. All of these are envelopes encrypted to the OLD sharing public key,
/// which the new private key cannot open:
///   1. shared.db `pending_group_invitations` rows still `pending` for this
///      user's personas: accept_invitation fails. Accepted rows are never
///      decrypted again.
///   2. shared.db `pending_group_key_rotations` rows still `pending` (the new
///      group key after a member departure, not yet applied): this member
///      cannot obtain the new key and is cut off from that group's later
///      content until it is re-sent. The serious one.
///   3. shared.db `pending_persona_shares.encrypted_payload` rows still
///      `pending` (SYNCED and VIEW-ONLY): accept fails. Accepted rows are not
///      re-read.
///   4. The shared-folder `<share_id>.qrshare` files of ongoing SYNCED and
///      VIEW-ONLY persona shares where this user is the RECIPIENT: the pull
///      fails until the owner pushes again, and the owner's push is gated by a
///      content hash only, so it will NOT push again by itself unless the
///      shared content changes. A silent stale share. The fix is for the push
///      to also compare the recipient's `user_sharing_keys.key_epoch`.
///   5. Envelopes another user encrypted to the old key in the moment between
///      the promotion commit and their next key lookup.
///
/// NOT affected: group keys already held (they live in the re-keyed
/// personal.db and in GroupKeyRegistry), every group.db and .qrsync file
/// (symmetric group key), envelopes this user SENT to others, the recovery seal.
///
///
/// RECONCILE RULE for 659: `user_sharing_keys.key_epoch` is durable, so groups
/// and shares can detect "last wrapped to an older epoch" by themselves after a
/// crash between promotion and this call; no separate marker table exists.
pub async fn after_sharing_key_rotation(
    _pool: &sqlx::SqlitePool,
    user_id: &str,
    from_epoch: u64,
    to_epoch: u64,
    old_public_key: Option<[u8; 32]>,
    new_public_key: [u8; 32],
) {
    log::info!(
        "key_rotation: sharing key rotated for user={user_id} epoch {from_epoch} -> {to_epoch} \
         (old key published: {}, new key {} bytes) -- group and share re-wrap is items.id=659",
        old_public_key.is_some(),
        new_public_key.len()
    );
}

// ---------------------------------------------------------------------------
// run_rotation
// ---------------------------------------------------------------------------

fn items_from_inventory(inv: &io::Inventory) -> Vec<RotationItem> {
    inv.items
        .iter()
        .map(|i: &InventoryItem| RotationItem {
            rel_path: i.rel_path.clone(),
            is_blob: matches!(i.target, ItemTarget::Blob(_)),
            state: RotationState::Pending,
            bytes: i.bytes,
            error: None,
        })
        .collect()
}

/// Everything that must hold before a rotation changes anything.
async fn preflight(
    env: &RotationEnv<'_>,
    old_dek: &[u8; kdf::MASTER_KEY_LEN],
    from_epoch: u64,
    kind: RotationKind,
) -> Result<(user_store::WrapperState, io::Inventory), RotationError> {
    let state = user_store::get_key_wrapper_state(env.pool, env.user_id)
        .await?
        .ok_or(RotationError::NoWrapper)?;
    if state.pending_rotation.is_some() {
        return Err(RotationError::RotationInProgress);
    }
    if state.wrapper.key_epoch != from_epoch {
        return Err(RotationError::EpochChanged);
    }

    if env.require_closed_gate {
        writer_gate::ensure_closed_and_empty(env.user_id).map_err(|e| match e {
            GateCheckError::Busy(n) => RotationError::ActiveRuns(n),
            GateCheckError::NotClosed => RotationError::GateNotClosed,
        })?;
    }

    let inv = io::inventory(&env.user_dir).await?;
    if kind == RotationKind::Restore
        && !inv
            .items
            .iter()
            .any(|i| i.rel_path == "integration_keys.db")
    {
        return Err(RotationError::NoProofDatabase);
    }

    // Every database must open under the OLD key and be sound. For a restore
    // this is also what proves the unsealed key is the account's real key: a
    // forged seal cannot open a real database.
    for item in &inv.items {
        if let ItemTarget::Db(t) = &item.target {
            let key = io::db_key_for(t, old_dek, env.user_id, from_epoch)?;
            let unreadable = |detail: String| RotationError::DatabaseUnreadable {
                rel_path: item.rel_path.clone(),
                detail,
            };
            // A busy file is retried a bounded number of times before the
            // rotation refuses to start (nothing has changed yet).
            let mut probe = io::probe_open(&item.abs_path, &key).await;
            for pause in env.retry_backoff {
                if !matches!(probe, io::OpenProbe::Busy) {
                    break;
                }
                tokio::time::sleep(pause).await;
                probe = io::probe_open(&item.abs_path, &key).await;
            }
            match probe {
                io::OpenProbe::Opens => {}
                io::OpenProbe::Busy => {
                    return Err(RotationError::Busy {
                        rel_path: item.rel_path.clone(),
                    })
                }
                io::OpenProbe::Failed(detail) => return Err(unreadable(detail)),
                io::OpenProbe::NotADatabase => {
                    // Stale sidecars may be the only problem. Settling acts
                    // only on this failure; busy aborts with nothing moved.
                    io::settle_stale_sidecars(&item.abs_path, &[&key])
                        .await
                        .map_err(|e| match e {
                            io::RekeyError::Busy => RotationError::Busy {
                                rel_path: item.rel_path.clone(),
                            },
                            other => unreadable(other.to_string()),
                        })?;
                }
            }
            io::verify_db_key(&item.abs_path, &key)
                .await
                .map_err(|e| unreadable(e.to_string()))?;
        }
    }

    let largest = inv.required_free_space();
    if largest > 0 {
        let needed = largest + largest / 10 + 1024 * 1024;
        // Blocking pool, like inventory(): a statvfs stuck on an unreachable
        // hard mount then ties up one blocking thread, not an async worker. A
        // join failure is "unknown" like any other failure to measure.
        let free_space = env.free_space;
        let user_dir = env.user_dir.clone();
        let available = tokio::task::spawn_blocking(move || free_space(&user_dir))
            .await
            .ok()
            .flatten()
            .ok_or(RotationError::FreeSpaceUnknown)?;
        if available < needed {
            return Err(RotationError::InsufficientSpace { needed, available });
        }
    }
    Ok((state, inv))
}

/// The engine's half of the acquire/begin_rotation ordering (items.id=778; see
/// writer_gate::acquire): with the rotating flag set, count the live leases. A
/// start path that drained and closed the gate first has none. The unlock-time
/// resume and cancel do not drain, so a lease that slipped in just before the
/// flag is found here and the rotation refuses instead of running under a writer.
fn check_no_writers(env: &RotationEnv<'_>) -> Result<(), RotationError> {
    if env.require_closed_gate {
        return Ok(()); // preflight asserts a closed, empty gate
    }
    match writer_gate::live_leases(env.user_id) {
        0 => Ok(()),
        n => Err(RotationError::ActiveRuns(n)),
    }
}

/// Report `phase` with no byte counts, for the moments before the account's
/// items are known (items.id=780, items.id=790). Needs no `Run`: the resume and
/// cancel paths call it before `load_run` has produced one.
fn emit_uncounted(env: &RotationEnv<'_>, rotation_id: &str, phase: Phase) {
    env.progress.emit(&ProgressEvent {
        rotation_id: rotation_id.to_owned(),
        phase,
        current: None,
        total: None,
        message: None,
    });
}

/// What the work before the first item leaves behind for the run.
struct Prepared {
    to_epoch: u64,
    new_dek: SecretBytes,
    password_key: Zeroizing<[u8; kdf::MASTER_KEY_LEN]>,
    items: Vec<RotationItem>,
    total_bytes: u64,
    old_sharing_public: Option<[u8; 32]>,
    new_sharing_public: [u8; 32],
}

/// Preflight and staging: everything from "nothing has changed" to "the
/// rotation is staged and resumable". Split out so `run_rotation` can close
/// the `Preparing` report when this refuses (items.id=780).
async fn prepare_rotation(
    env: &RotationEnv<'_>,
    old_dek: &[u8; kdf::MASTER_KEY_LEN],
    from_epoch: u64,
    request: &RotationRequest,
    rotation_id: &str,
) -> Result<Prepared, RotationError> {
    let user = env.user_id;
    let kind = request.kind();
    let (state, inv) = preflight(env, old_dek, from_epoch, kind).await?;

    // Stage.
    let to_epoch = from_epoch + 1;
    let new_dek = data_key::generate_dek()?;
    let salt = kdf::generate_salt()?;
    let password_key = kdf::derive_password_key_blocking(
        request.password(),
        &salt,
        env.kdf.memory_kib,
        env.kdf.iterations,
        env.kdf.parallelism,
    )
    .await?;

    let (recovery_public_key, new_secret): ([u8; 32], Option<StaticSecret>) = match &request {
        RotationRequest::NewPhrase {
            recovery_entropy, ..
        } => {
            let (secret, public) = data_key::derive_recovery_keypair(recovery_entropy, user);
            (*public.as_bytes(), Some(secret))
        }
        _ => (state.wrapper.recovery_public_key, None),
    };
    let (password_wrap, recovery_seal) = data_key::build_wrapper_parts(
        user,
        &new_dek,
        &password_key,
        &recovery_public_key,
        to_epoch,
    )?;

    // Self-verify the staged wraps before anything depends on them.
    let by_password = data_key::unwrap_dek(
        &password_key,
        &password_wrap,
        user,
        to_epoch,
        data_key::WRAP_FORMAT_VERSION,
    )?;
    if *by_password != *new_dek {
        return Err(RotationError::Crypto(DataKeyError::Crypto(
            "staged password wrap does not open to the new key".into(),
        )));
    }
    if let Some(secret) = &new_secret {
        let by_phrase = data_key::unseal_dek_with_recovery_key(
            secret,
            &recovery_seal,
            user,
            to_epoch,
            data_key::WRAP_FORMAT_VERSION,
        )?;
        if *by_phrase != *new_dek {
            return Err(RotationError::Crypto(DataKeyError::Crypto(
                "staged recovery seal does not open to the new key".into(),
            )));
        }
    }

    let (_, new_sharing_public) = sharing_keypair::derive_sharing_keypair(&new_dek, user, to_epoch);
    let old_sharing_public = sharing_keypair::get_public_key(env.pool, user)
        .await
        .ok()
        .flatten();
    let items = items_from_inventory(&inv);
    let total_bytes = inv.total_bytes();

    let staged = StagedRotation {
        rotation_id: rotation_id.to_owned(),
        user_id: user.to_owned(),
        kind,
        from_epoch,
        to_epoch,
        old_dek_under_new: data_key::wrap_old_dek_under_new(
            &new_dek,
            old_dek,
            user,
            from_epoch,
            to_epoch,
            rotation_id,
        )?,
        salt: salt.to_vec(),
        kdf_memory_kib: env.kdf.memory_kib,
        kdf_iterations: env.kdf.iterations,
        kdf_parallelism: env.kdf.parallelism,
        format_version: data_key::WRAP_FORMAT_VERSION,
        password_wrap,
        recovery_public_key,
        recovery_seal,
        sharing_public_key: *new_sharing_public.as_bytes(),
        items: items.clone(),
    };
    user_store::stage_rotation(env.pool, &staged)
        .await
        .map_err(|e| match e {
            UserStoreError::RotationInProgress => RotationError::RotationInProgress,
            UserStoreError::EpochChanged => RotationError::EpochChanged,
            other => other.into(),
        })?;
    Ok(Prepared {
        to_epoch,
        new_dek,
        password_key,
        items,
        total_bytes,
        old_sharing_public,
        new_sharing_public: *new_sharing_public.as_bytes(),
    })
}

/// Rotate the account's data key. `old_dek` is the resident (or recovered)
/// key at `from_epoch`; the caller has authenticated for `request`. See the
/// module header for the order of work and the failure policy.
pub async fn run_rotation(
    env: &RotationEnv<'_>,
    old_dek: &[u8; kdf::MASTER_KEY_LEN],
    from_epoch: u64,
    request: RotationRequest,
) -> Result<RotationOutcome, RotationError> {
    // The engine's own epoch check, before taking the flag (the flag makes
    // the guard refuse everyone, this process included).
    match epoch_guard::ensure_current_with_pool(env.pool, env.user_id).await {
        Ok(()) => {}
        Err(epoch_guard::KeyEpochError::Changed { .. }) => return Err(RotationError::EpochChanged),
        Err(epoch_guard::KeyEpochError::Rotating) => return Err(RotationError::RotationInProgress),
        Err(e) => return Err(RotationError::Epoch(e.to_string())),
    }
    let _guard =
        epoch_guard::begin_rotation(env.user_id).map_err(|_| RotationError::RotationInProgress)?;
    check_no_writers(env)?;

    let kind = request.kind();
    // Reported before any work, with no byte counts (there is no inventory
    // yet), so the overlay is up for the whole of preflight and staging
    // (items.id=780). The flag is already held, so a status read agrees that a
    // rotation is running.
    let rotation_id = uuid::Uuid::new_v4().to_string();
    emit_uncounted(env, &rotation_id, Phase::Preparing);
    let prepared = match prepare_rotation(env, old_dek, from_epoch, &request, &rotation_id).await {
        Ok(p) => p,
        Err(e) => {
            // Nothing was staged. A simulated crash reports nothing, like the
            // process death it stands for.
            if !e.is_crash() {
                emit_uncounted(env, &rotation_id, Phase::NotStarted);
            }
            return Err(e);
        }
    };
    let Prepared {
        to_epoch,
        new_dek,
        password_key,
        items,
        total_bytes,
        old_sharing_public,
        new_sharing_public,
    } = prepared;
    #[cfg(test)]
    env.fault(FaultPoint::AfterStage)?;

    let run = Run {
        env,
        rotation_id,
        kind,
        from_epoch,
        to_epoch,
        old_dek: Zeroizing::new(*old_dek),
        new_dek,
        items,
        total_bytes,
    };

    let result = match drive_forward(&run).await {
        Ok(()) => promote_and_finish(&run, Credential::Password(&password_key)).await,
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        return Err(handle_failure(&run, e).await);
    }

    #[cfg(test)]
    env.fault(FaultPoint::AfterFinishBeforeInstall)?;
    Ok(complete(&run, old_sharing_public, new_sharing_public).await)
}

/// Decide what a failure means. Before promotion it is rolled back; after it,
/// the rotation is left for resume.
async fn handle_failure(run: &Run<'_>, e: RotationError) -> RotationError {
    if e.is_crash() {
        return e;
    }
    let cause = e.to_string();
    let promoted = matches!(
        user_store::get_rotation(run.env.pool, &run.rotation_id).await,
        Ok(Some(r)) if r.state == RotationState::Verifying
    );
    if promoted {
        // The data is on the new key; the registry's key is stale. End the
        // session now, while the flag is still held (items.id=778).
        end_session(run.env).await;
        run.emit(Phase::Failed, run.total_bytes, None);
        return RotationError::NeedsResume { cause };
    }
    match roll_back(run, &cause).await {
        Ok(()) => {
            run.emit(Phase::RolledBack, 0, None);
            RotationError::RolledBack { cause }
        }
        Err(rb) if rb.is_crash() => rb,
        Err(rb) => {
            let _ = user_store::abort_rotation(
                run.env.pool,
                &run.rotation_id,
                RotationState::Failed,
                Some(&format!("{cause}; rollback failed: {rb}")),
            )
            .await;
            // Files may be on either key and the unlock now needs the staged
            // credential: the resident key cannot be trusted (items.id=778).
            end_session(run.env).await;
            run.emit(Phase::Failed, 0, None);
            RotationError::NeedsResume {
                cause: format!("{cause}; rollback failed: {rb}"),
            }
        }
    }
}

/// The last steps of a successful rotation: run the sharing hook, end the
/// session (clear the registry), report completion.
async fn complete(
    run: &Run<'_>,
    old_sharing_public: Option<[u8; 32]>,
    new_sharing_public: [u8; 32],
) -> RotationOutcome {
    let outcome = RotationOutcome {
        rotation_id: run.rotation_id.clone(),
        kind: run.kind,
        from_epoch: run.from_epoch,
        to_epoch: run.to_epoch,
        new_dek: run.new_dek.clone(),
        old_sharing_public_key: old_sharing_public,
        new_sharing_public_key: new_sharing_public,
    };
    #[cfg(test)]
    record_hook_call(run.env).await;
    after_sharing_key_rotation(
        run.env.pool,
        run.env.user_id,
        outcome.from_epoch,
        outcome.to_epoch,
        outcome.old_sharing_public_key,
        outcome.new_sharing_public_key,
    )
    .await;
    end_session(run.env).await;
    run.emit(Phase::Complete, run.total_bytes, None);
    outcome
}

// ---------------------------------------------------------------------------
// Resume and cancel
// ---------------------------------------------------------------------------

/// The new DEK from the staged password wrap, given the password key derived
/// with the STAGED salt (pending.salt) and KDF parameters.
pub fn new_dek_from_staged_password(
    pending: &PendingRotation,
    user_id: &str,
    password_key: &[u8; kdf::MASTER_KEY_LEN],
) -> Result<SecretBytes, RotationError> {
    let (Some(wrap), Some(fmt)) = (pending.password_wrap.as_ref(), pending.format_version) else {
        return Err(RotationError::CannotResume(
            "the staged password wrap is gone".into(),
        ));
    };
    Ok(data_key::unwrap_dek(
        password_key,
        wrap,
        user_id,
        pending.to_epoch,
        fmt,
    )?)
}

/// The new DEK from the staged recovery seal, given the typed phrase's
/// entropy. The phrase must derive the staged recovery public key.
pub fn new_dek_from_staged_phrase(
    pending: &PendingRotation,
    user_id: &str,
    entropy: &[u8; kdf::MASTER_KEY_LEN],
) -> Result<SecretBytes, RotationError> {
    let (Some(seal), Some(fmt), Some(staged_public)) = (
        pending.recovery_seal.as_ref(),
        pending.format_version,
        pending.recovery_public_key.as_ref(),
    ) else {
        return Err(RotationError::CannotResume(
            "the staged recovery seal is gone".into(),
        ));
    };
    let (secret, public) = data_key::derive_recovery_keypair(entropy, user_id);
    if public.as_bytes() != staged_public {
        return Err(RotationError::WrongPhrase);
    }
    Ok(data_key::unseal_dek_with_recovery_key(
        &secret,
        seal,
        user_id,
        pending.to_epoch,
        fmt,
    )?)
}

/// What the user typed to reach the staged new key of an interrupted rotation.
pub enum StagedCredential<'a> {
    /// The new password, tried against the staged wrap with the STAGED salt
    /// and cost parameters.
    Password(&'a [u8]),
    /// The recovery phrase's entropy, tried against the staged recovery seal.
    Phrase(&'a [u8; kdf::MASTER_KEY_LEN]),
}

/// The new DEK of an interrupted rotation, plus the key that proves the
/// read-back unlock when the rotation is resumed.
pub struct StagedUnlock {
    pub new_dek: SecretBytes,
    readback_key: Zeroizing<[u8; kdf::MASTER_KEY_LEN]>,
    via_phrase: bool,
}

impl StagedUnlock {
    /// The credential `resume_rotation` reads the live wrapper back with.
    pub fn credential(&self) -> Credential<'_> {
        if self.via_phrase {
            Credential::Phrase(&self.readback_key)
        } else {
            Credential::Password(&self.readback_key)
        }
    }
}

#[derive(Debug, Error)]
pub enum StagedUnlockError {
    /// The staged salt or cost parameters are gone, so a password cannot be
    /// tried at all.
    #[error("the staged key derivation parameters are gone")]
    Unusable,
    #[error(transparent)]
    Kdf(kdf::AuthError),
    /// The credential does not open the staged new key.
    #[error("the credential does not open the staged key")]
    WrongCredential,
    /// Anything else (the staged material is malformed or missing).
    #[error(transparent)]
    Failed(RotationError),
}

/// Turn what the user typed into the staged new DEK. Shared by login (new
/// password), restore (phrase) and cancel. A wrong credential is reported as
/// `WrongCredential`: for a password only an authentication failure counts as
/// wrong, for a phrase a wrong phrase or any failed unseal does.
pub async fn unlock_staged(
    pending: &PendingRotation,
    user_id: &str,
    credential: StagedCredential<'_>,
) -> Result<StagedUnlock, StagedUnlockError> {
    match credential {
        StagedCredential::Password(password) => {
            let (Some(salt), Some(mem), Some(iter), Some(par)) = (
                pending.salt.as_ref(),
                pending.kdf_memory_kib,
                pending.kdf_iterations,
                pending.kdf_parallelism,
            ) else {
                return Err(StagedUnlockError::Unusable);
            };
            let staged_pk = kdf::derive_password_key_blocking(password, salt, mem, iter, par)
                .await
                .map_err(StagedUnlockError::Kdf)?;
            match new_dek_from_staged_password(pending, user_id, &staged_pk) {
                Ok(new_dek) => Ok(StagedUnlock {
                    new_dek,
                    readback_key: staged_pk,
                    via_phrase: false,
                }),
                Err(RotationError::Crypto(DataKeyError::WrongPassword)) => {
                    Err(StagedUnlockError::WrongCredential)
                }
                Err(e) => Err(StagedUnlockError::Failed(e)),
            }
        }
        StagedCredential::Phrase(entropy) => {
            match new_dek_from_staged_phrase(pending, user_id, entropy) {
                Ok(new_dek) => Ok(StagedUnlock {
                    new_dek,
                    readback_key: Zeroizing::new(*entropy),
                    via_phrase: true,
                }),
                Err(RotationError::WrongPhrase) | Err(RotationError::Crypto(_)) => {
                    Err(StagedUnlockError::WrongCredential)
                }
                Err(e) => Err(StagedUnlockError::Failed(e)),
            }
        }
    }
}

async fn load_run<'a>(
    env: &'a RotationEnv<'a>,
    pending: &PendingRotation,
    new_dek: SecretBytes,
) -> Result<Run<'a>, RotationError> {
    let Some(wrapped_old) = pending.old_dek_under_new.as_ref() else {
        return Err(RotationError::CannotResume(
            "the old key is no longer recoverable".into(),
        ));
    };
    let old_dek = data_key::unwrap_old_dek_with_new(
        &new_dek,
        wrapped_old,
        env.user_id,
        pending.from_epoch,
        pending.to_epoch,
        &pending.rotation_id,
    )
    .map_err(|_| RotationError::CannotResume("the new key does not match this rotation".into()))?;
    let items = user_store::list_rotation_items(env.pool, &pending.rotation_id).await?;
    let total_bytes = items.iter().map(|i| i.bytes).sum();
    Ok(Run {
        env,
        rotation_id: pending.rotation_id.clone(),
        kind: pending.kind,
        from_epoch: pending.from_epoch,
        to_epoch: pending.to_epoch,
        old_dek,
        new_dek,
        items,
        total_bytes,
    })
}

/// `load_run`, preceded by the first report and followed, on a refusal, by the
/// silent closing one (items.id=790; the resume and cancel counterpart of
/// `run_rotation`'s `Preparing` / `NotStarted`). Called with the rotating flag
/// held, so a status read agrees a rotation is running. No byte counts: the
/// items are not read yet. A refusal before this point (flag taken, writers
/// present) emits nothing: it is instant, and a `NotStarted` for a flag some
/// other rotation holds would close ITS report. A simulated crash reports
/// nothing, like the process death it stands for.
async fn load_run_reported<'a>(
    env: &'a RotationEnv<'a>,
    pending: &PendingRotation,
    new_dek: SecretBytes,
    first: Phase,
) -> Result<Run<'a>, RotationError> {
    emit_uncounted(env, &pending.rotation_id, first);
    match load_run(env, pending, new_dek).await {
        Ok(run) => Ok(run),
        Err(e) => {
            if !e.is_crash() {
                emit_uncounted(env, &pending.rotation_id, Phase::NotStarted);
            }
            Err(e)
        }
    }
}

/// The tail of resuming a PROMOTED rotation: confirm it is still ours, read the
/// live wrapper back with the credential, finish.
async fn finish_promoted(run: &Run<'_>, credential: &Credential<'_>) -> Result<(), RotationError> {
    run.check_still_ours(true).await?;
    run.emit(Phase::Finishing, run.total_bytes, None);
    read_back(run, run.env.user_id, credential).await?;
    user_store::finish_rotation(run.env.pool, &run.rotation_id)
        .await
        .map_err(RotationError::from)?;
    scrub_shared_db(run.env.pool).await;
    Ok(())
}

/// Finish an interrupted rotation FORWARD. `new_dek` comes from
/// new_dek_from_staged_password or new_dek_from_staged_phrase; `credential`
/// is what proves the read-back (the staged/new password key, or the phrase's
/// entropy). A failure marks the rotation `failed` and keeps it resumable; it
/// does not roll back by itself (cancel_rotation does that on request). A
/// PROMOTED rotation is the exception: it is not marked failed, so it stays
/// `Verifying` and cancel keeps refusing it (items.id=790).
pub async fn resume_rotation(
    env: &RotationEnv<'_>,
    pending: &PendingRotation,
    new_dek: SecretBytes,
    credential: Credential<'_>,
) -> Result<ResumeOutcome, RotationError> {
    let _guard =
        epoch_guard::begin_rotation(env.user_id).map_err(|_| RotationError::RotationInProgress)?;
    check_no_writers(env)?;
    let run = load_run_reported(env, pending, new_dek, Phase::Resuming).await?;
    // A resume starts where the run it continues will (a promoted forward one
    // included: every item is committed), so the bar does not show 0% and then
    // jump. A rollback starts at the bytes it already undid.
    let resumed_done = if pending.direction == RotationDirection::Rollback {
        run.bytes_in_state(RotationState::RolledBack)
    } else {
        run.bytes_in_state(RotationState::Committed)
    };
    run.emit(Phase::Resuming, resumed_done, None);

    if pending.direction == RotationDirection::Rollback {
        return finish_rollback(&run, pending.last_error.as_deref().unwrap_or("cancelled")).await;
    }
    let result = if pending.state == RotationState::Verifying {
        // Promoted: the rotation's state must stay `Verifying` (cancel refuses
        // on it), so a failure here is NOT marked failed. It closes the report
        // without an outcome and leaves the rotation resumable (items.id=790).
        if let Err(e) = finish_promoted(&run, &credential).await {
            if !e.is_crash() {
                emit_uncounted(env, &run.rotation_id, Phase::NotStarted);
            }
            return Err(e);
        }
        Ok(())
    } else {
        match drive_forward(&run).await {
            Ok(()) => promote_and_finish(&run, credential).await,
            Err(e) => Err(e),
        }
    };
    if let Err(e) = result {
        if e.is_crash() {
            return Err(e);
        }
        let cause = e.to_string();
        let _ = user_store::abort_rotation(
            env.pool,
            &run.rotation_id,
            RotationState::Failed,
            Some(&cause),
        )
        .await;
        run.emit(Phase::Failed, 0, None);
        return Err(RotationError::NeedsResume { cause });
    }

    #[cfg(test)]
    env.fault(FaultPoint::AfterFinishBeforeInstall)?;
    let new_public =
        sharing_keypair::derive_sharing_keypair(&run.new_dek, env.user_id, run.to_epoch)
            .1
            .to_bytes();
    // The old key was published before promotion; a resumed rotation no longer
    // knows it, which the hook documents as optional.
    Ok(ResumeOutcome::Completed(
        complete(&run, None, new_public).await,
    ))
}

async fn finish_rollback(run: &Run<'_>, cause: &str) -> Result<ResumeOutcome, RotationError> {
    match roll_back(run, cause).await {
        Ok(()) => {
            run.emit(Phase::RolledBack, 0, None);
            Ok(ResumeOutcome::RolledBack)
        }
        Err(e) if e.is_crash() => Err(e),
        Err(e) => {
            let _ = user_store::abort_rotation(
                run.env.pool,
                &run.rotation_id,
                RotationState::Failed,
                Some(&e.to_string()),
            )
            .await;
            run.emit(Phase::Failed, 0, None);
            Err(RotationError::NeedsResume {
                cause: e.to_string(),
            })
        }
    }
}

/// Undo an interrupted rotation instead of finishing it (the user asked).
/// Refused once promoted: the new wraps are live and the data is on the new
/// key. Needs the same new-key material as resume_rotation.
pub async fn cancel_rotation(
    env: &RotationEnv<'_>,
    pending: &PendingRotation,
    new_dek: SecretBytes,
) -> Result<ResumeOutcome, RotationError> {
    if pending.state == RotationState::Verifying {
        return Err(RotationError::AlreadyPromoted);
    }
    let _guard =
        epoch_guard::begin_rotation(env.user_id).map_err(|_| RotationError::RotationInProgress)?;
    check_no_writers(env)?;
    let run = load_run_reported(env, pending, new_dek, Phase::RollingBack).await?;
    finish_rollback(&run, "cancelled").await
}

/// Test seam: what the sharing hook's call site saw (items.id=778: the hook
/// runs while the rotating flag is held and BEFORE the registry is cleared).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HookObservation {
    pub rotating: bool,
    pub registry_occupied: bool,
}

#[cfg(test)]
pub(crate) static HOOK_OBSERVATION: Mutex<Option<HookObservation>> = Mutex::new(None);

#[cfg(test)]
async fn record_hook_call(env: &RotationEnv<'_>) {
    let registry_occupied = match env.registry {
        Some(r) => r.is_occupied().await,
        None => false,
    };
    *HOOK_OBSERVATION.lock().unwrap() = Some(HookObservation {
        rotating: epoch_guard::is_rotating_for(env.user_id),
        registry_occupied,
    });
}

#[cfg(test)]
mod tests;

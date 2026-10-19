// src-tauri/src/commands/key_rotation.rs
//
// Group 11 -- Auth: the three credential changes, each a data-key rotation
// (items.id=630 stage D, decisions.id=893, Architecture/
// AUTH_MULTIUSER_ARCHITECTURE.md Section 4.0). The work is auth::key_rotation;
// this file is the IPC surface around it.
//
//   change_password(old, new)                  -- session required
//   begin_new_recovery_phrase(password)        -- session required, shows the
//   confirm_new_recovery_phrase(password, ..)     words once, then rotates
//   cancel_new_recovery_phrase()
//   restore_with_recovery_phrase(name, words, new password)
//                                              -- no session (forgot password)
//   cancel_interrupted_rotation_with_password(name, new password)
//   cancel_interrupted_rotation_with_phrase(name, words)
//                                              -- no session (resume screen)
//   get_rotation_status()
//
// NEW-PASSWORD POLICY (items.id=777, decisions.id=858 and 900): a password
// being CHOSEN (change_password, restore_with_recovery_phrase) is checked by
// auth::password_policy before any credential is verified or any work starts:
// empty, shorter than 15 characters, or on the common/breached list each have
// their own code. change_password also refuses a new password equal to the
// current one, but only after the old password is verified (earlier it would
// be an oracle). Cancelling an interrupted change and signing in never apply
// the policy: they type a password that already exists, which may predate it.
//
// ERRORS ARE CODED AND CARRY NOTHING TO IDENTIFY ANYTHING. Every command here
// returns KeyRotationError: a small closed set of codes, a few with a count or
// a size in megabytes. No file name, path, user id, persona id or rotation id
// ever crosses this boundary; the detail goes to the log only. Wrong password
// and wrong phrase are the same code (InvalidCredentials), and so is an
// unknown account.
//
// FAILED ATTEMPTS: a wrong old password or wrong phrase is recorded in
// auth_failures exactly as login records one (record_failed_attempt), behind
// the same is_locked_out check, and nothing is cleared on success. Whether
// recovery-phrase failures should share the password counter is for items.id=632.
//
// CANCEL (items.id=771, decisions.id=895 point 5): an interrupted rotation that
// is not yet promoted can be undone from the login screen with the NEW password
// or the recovery phrase (the credentials that reach the staged new key). Unknown
// account, an account with nothing pending, and a wrong credential all answer
// InvalidCredentials; the last two each record one failed attempt. A promoted
// rotation (state verifying) answers RotationAlreadyComplete, and only after the
// credential proved right. Cancel never signs in: the account is back on the OLD
// key and the user signs in with the old password. The old password typed here is
// simply wrong (login's uncounted "old password" answer would make cancel an
// oracle for it). Equal timing across these cases is items.id=639's.
//
// NEW-PHRASE ORDER (decided with the plan, differs from AUTH 4.0's listing):
// the new words are generated and SHOWN first (begin), the user re-enters
// positions the SERVER chose (confirm), and only then does the rotation run
// and revoke the old words. A crash before the rotation commits changes
// nothing and the old words still work; after it the user already holds and
// has confirmed the new ones. The entropy lives only in KeyRegistry's
// pending_new_phrase slot (memory, zeroized, cleared by any lock) -- never on
// disk and never in an error. The password is asked for again at confirm
// rather than held in memory between the two calls.
//
// WRITERS (items.id=778, decisions.id=896): the three commands that start a
// rotation first drain the account's writers through auth::writer_gate
// (drain_writers below): no new reply, Focus run, import or sweep starts, and
// the rotation runs only once every lease has ended. They hold no lease
// themselves and are listed as exempt in the gate's source scan.
//
// PROGRESS: events on the existing `task-progress` channel with kind
// `key_rotation` (TauriRotationSink, installed in main.rs), plus
// get_rotation_status for a webview that reloads mid-rotation.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State;
use zeroize::Zeroizing;

use crate::auth::data_key::{self, DataKeyError};
use crate::auth::epoch_guard;
use crate::auth::idle_timeout::{emit_session_locked, lock_session, LockReason};
use crate::auth::kdf;
use crate::auth::key_rotation::{
    self, ProgressEvent, ProgressSink, ResumeOutcome, RotationEnv, RotationError, RotationRequest,
    StagedCredential, StagedUnlockError,
};
use crate::auth::password_policy::{self, PasswordPolicyError};
use crate::auth::registry::{GroupKeyRegistry, KeyRegistry, NewPhraseSlot};
use crate::auth::user_store;
use crate::auth::writer_gate::{self, DrainError, DrainGuard, GateError};
use crate::commands::auth::{is_locked_out, record_failed_attempt};
use crate::provider_credentials::session::ConnectSessions;
use crate::task_progress::{emit_progress, TaskProgressPayload};

// ---------------------------------------------------------------------------
// IPC types
// ---------------------------------------------------------------------------

/// Why a credential change did not happen (or did not finish). Frontend-safe:
/// no path, id or secret in any variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum KeyRotationError {
    /// No account is unlocked in this process.
    NotLoggedIn,
    /// Wrong old password, wrong recovery phrase, or an unknown account. One
    /// code for all three.
    InvalidCredentials,
    AccountLocked,
    NewPasswordEmpty,
    /// Fewer than 15 characters (decisions.id=858; counted in Unicode code
    /// points after NFKC, see auth::password_policy).
    NewPasswordTooShort,
    /// On the common / breached password list.
    NewPasswordBlocked,
    /// change_password only: the new password is the current one, which would
    /// start a data-key rotation that changes nothing for the user.
    NewPasswordSameAsCurrent,
    /// The typed words are not a valid 24-word recovery phrase.
    PhraseInvalid,
    /// A rotation is already running, or one is pending that must finish first.
    RotationInProgress,
    /// Tasks are still running; wait for them or cancel them. The engine's own
    /// last-moment check; the drain normally answers with the codes below.
    ActiveRuns {
        count: u32,
    },
    /// A chat reply or Focus run is running and no choice was made yet
    /// (items.id=778). Nothing was changed. The caller asks the user whether
    /// to finish first or stop and save, and calls again with the answer.
    WritersActive {
        replies: u32,
        focus_runs: u32,
    },
    /// The wait for running work reached its ceiling (10 minutes for 'finish
    /// first', 30 seconds for short operations). Nothing was changed. The
    /// counts say what is still running.
    WritersStillRunning {
        replies: u32,
        focus_runs: u32,
    },
    /// 'Stop and save' was chosen and the work had not stopped after 60
    /// seconds. Nothing was changed and nothing was killed.
    WriterNotStopping,
    /// The user cancelled the wait. Nothing was changed.
    DrainCancelled,
    /// Another part of the app (or another window) is using the data right now.
    Busy,
    InsufficientSpace {
        needed_mb: u32,
        available_mb: u32,
    },
    FreeSpaceUnknown,
    /// A file this app does not recognise is in the account's data.
    UnrecognisedFiles,
    /// A database could not be opened with the current key.
    DatabaseUnreadable,
    /// The account's key was changed in another session.
    EpochChanged,
    /// Cancel refused: the change is already complete on disk (the new wraps
    /// are live) and can only be finished, with the new password or the
    /// recovery phrase.
    RotationAlreadyComplete,
    /// The change was started and then undone. Nothing changed for the user.
    RolledBack,
    /// The change did not finish and will be completed the next time the
    /// account is unlocked with the new password or the recovery phrase.
    NeedsResume,
    /// confirm_new_recovery_phrase without a phrase to confirm.
    NoPendingPhrase,
    /// The re-entered words were not the ones asked for.
    ConfirmationFailed,
    /// Anything else. The detail is in the log, not here.
    Internal,
}

/// The new recovery phrase, shown once, and which word positions (1-based)
/// the user must re-enter to confirm it.
#[derive(Debug, Serialize, Deserialize, Type)]
pub struct NewRecoveryPhrase {
    pub mnemonic: String,
    pub confirm_positions: Vec<u32>,
}

/// What the user chose when a chat reply or Focus run was running
/// (items.id=778, decisions.id=896). `ask` is the first call: it changes
/// nothing and reports what is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum WritersChoice {
    Ask,
    FinishFirst,
    StopAndSave,
}

impl From<WritersChoice> for writer_gate::Choice {
    fn from(c: WritersChoice) -> Self {
        match c {
            WritersChoice::Ask => writer_gate::Choice::Ask,
            WritersChoice::FinishFirst => writer_gate::Choice::FinishFirst,
            WritersChoice::StopAndSave => writer_gate::Choice::StopAndSave,
        }
    }
}

/// One re-entered word.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct WordAnswer {
    pub position: u32,
    pub word: String,
}

/// How the last rotation this process ran ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum LastRotationOutcome {
    /// None ran in this process, one is running, or the last one stopped
    /// without a final report (an interruption).
    None,
    Complete,
    RolledBack,
    /// It did not finish and waits for the next unlock with the new password
    /// or the recovery phrase (or a cancel, if it was not yet promoted).
    Failed,
}

/// Where a rotation stands, for a webview that reloaded or mounted late.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RotationStatus {
    /// A rotation is running in this process right now.
    pub running: bool,
    /// The last reported phase while running (`rekeying`, `verifying`, ...).
    pub phase: Option<String>,
    /// 0 to 100, by bytes, while running.
    pub percent: u8,
    /// A rotation was interrupted and waits for the next unlock with the new
    /// password or the recovery phrase.
    pub interrupted: bool,
    /// `password_change`, `restore` or `new_phrase` while one is pending.
    pub pending_kind: Option<String>,
    /// How the last rotation this process ran ended. `none` while one runs.
    pub last_outcome: LastRotationOutcome,
}

// ---------------------------------------------------------------------------
// Progress events
// ---------------------------------------------------------------------------

/// Sends rotation progress to the webview on the shared `task-progress`
/// event (kind `key_rotation`). Bytes done and bytes in total; the message is
/// a display label, never a path.
pub struct TauriRotationSink {
    handle: tauri::AppHandle,
}

impl ProgressSink for TauriRotationSink {
    fn emit(&self, e: &ProgressEvent) {
        emit_progress(
            &self.handle,
            &TaskProgressPayload {
                task_id: e.rotation_id.clone(),
                kind: "key_rotation".to_owned(),
                phase: e.phase.as_str().to_owned(),
                current: Some(e.current),
                total: Some(e.total),
                message: e.message.clone(),
            },
        );
    }
}

/// Install the Tauri progress emitter for every rotation in this process.
/// Called once from main.rs's setup.
pub fn install_progress_sink(handle: tauri::AppHandle) {
    key_rotation::set_global_progress_sink(Some(Arc::new(TauriRotationSink { handle })));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn user_dir(user_id: &str) -> std::path::PathBuf {
    crate::persistence::migrations::get_data_root()
        .join("users")
        .join(user_id)
}

fn mb(bytes: u64) -> u32 {
    u32::try_from(bytes.div_ceil(1024 * 1024)).unwrap_or(u32::MAX)
}

/// Translate an engine error. Logs the full detail, returns only a code.
fn code_for(e: &RotationError) -> KeyRotationError {
    log::warn!("key_rotation: {e}");
    match e {
        RotationError::RotationInProgress => KeyRotationError::RotationInProgress,
        RotationError::EpochChanged => KeyRotationError::EpochChanged,
        RotationError::ActiveRuns(n) => KeyRotationError::ActiveRuns {
            count: u32::try_from(*n).unwrap_or(u32::MAX),
        },
        RotationError::Busy { .. } => KeyRotationError::Busy,
        RotationError::UnknownEncryptedFile(_) => KeyRotationError::UnrecognisedFiles,
        RotationError::NoProofDatabase | RotationError::DatabaseUnreadable { .. } => {
            KeyRotationError::DatabaseUnreadable
        }
        RotationError::InsufficientSpace { needed, available } => {
            KeyRotationError::InsufficientSpace {
                needed_mb: mb(*needed),
                available_mb: u32::try_from(available / (1024 * 1024)).unwrap_or(u32::MAX),
            }
        }
        RotationError::FreeSpaceUnknown => KeyRotationError::FreeSpaceUnknown,
        RotationError::RolledBack { .. } => KeyRotationError::RolledBack,
        RotationError::NeedsResume { .. } => KeyRotationError::NeedsResume,
        RotationError::WrongPhrase => KeyRotationError::InvalidCredentials,
        RotationError::AlreadyPromoted => KeyRotationError::RotationAlreadyComplete,
        _ => KeyRotationError::Internal,
    }
}

/// Translate a failure to reach the staged new key. A wrong credential is the
/// caller's to count, so it maps to the same code as everywhere else.
fn code_for_unlock(e: &StagedUnlockError) -> KeyRotationError {
    match e {
        StagedUnlockError::WrongCredential => KeyRotationError::InvalidCredentials,
        StagedUnlockError::Failed(e) => code_for(e),
        other => internal("staged unlock", other),
    }
}

struct Resident {
    user_id: String,
    data_key: [u8; kdf::MASTER_KEY_LEN],
    key_epoch: u64,
}

async fn resident(key_registry: &KeyRegistry) -> Result<Resident, KeyRotationError> {
    key_registry
        .with_key(|k| Resident {
            user_id: k.user_id.clone(),
            data_key: k.data_key,
            key_epoch: k.key_epoch,
        })
        .await
        .ok_or(KeyRotationError::NotLoggedIn)
}

fn internal(context: &str, e: impl std::fmt::Display) -> KeyRotationError {
    log::warn!("key_rotation: {context}: {e}");
    KeyRotationError::Internal
}

/// decisions.id=858: judge a password that is about to be chosen, BEFORE any
/// credential is checked or any work is done. The verdict depends only on what
/// the caller typed, so it says nothing about any account and is not a failed
/// attempt. Never used for a password that already exists (sign-in, resume,
/// cancel).
fn check_new_password(password: &str) -> Result<(), KeyRotationError> {
    password_policy::validate_new_password(password).map_err(|e| match e {
        PasswordPolicyError::Empty => KeyRotationError::NewPasswordEmpty,
        PasswordPolicyError::TooShort => KeyRotationError::NewPasswordTooShort,
        PasswordPolicyError::Blocked => KeyRotationError::NewPasswordBlocked,
    })
}

/// Re-verify a password against the account's LIVE wrapper and return the
/// DEK it opens, recording a wrong password as a failed attempt. A resident
/// session alone is not enough authority to rotate.
async fn verify_password(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    display_name: &str,
    password: &str,
) -> Result<data_key::SecretBytes, KeyRotationError> {
    if is_locked_out(pool, display_name)
        .await
        .map_err(|e| internal("lockout check", e))?
    {
        return Err(KeyRotationError::AccountLocked);
    }
    let state = user_store::get_key_wrapper_state(pool, user_id)
        .await
        .map_err(|e| internal("wrapper", e))?
        .ok_or_else(|| internal("wrapper", "missing"))?;
    if state.pending_rotation.is_some() {
        return Err(KeyRotationError::RotationInProgress);
    }
    let (salt, mem, iter, par) = user_store::get_salt_params(pool, user_id)
        .await
        .map_err(|e| internal("salt", e))?
        .ok_or_else(|| internal("salt", "missing"))?;
    let pk = kdf::derive_password_key_blocking(password.as_bytes(), &salt, mem, iter, par)
        .await
        .map_err(|e| internal("kdf", e))?;
    match data_key::unwrap_dek(
        &pk,
        &state.wrapper.password_wrap,
        user_id,
        state.wrapper.key_epoch,
        state.wrapper.format_version,
    ) {
        Ok(dek) => Ok(dek),
        Err(DataKeyError::WrongPassword) => {
            record_failed_attempt(pool, display_name)
                .await
                .map_err(|e| internal("failure bookkeeping", e))?;
            Err(KeyRotationError::InvalidCredentials)
        }
        Err(e) => Err(internal("unwrap", e)),
    }
}

/// The resident session plus a verified password: the common start of a
/// change that needs both. Returns (resident, display_name).
async fn authorise_session_change(
    key_registry: &KeyRegistry,
    pool: &sqlx::SqlitePool,
    password: &str,
) -> Result<(Resident, String), KeyRotationError> {
    let resident = resident(key_registry).await?;
    let user = user_store::find_user_by_id(pool, &resident.user_id)
        .await
        .map_err(|e| internal("user", e))?
        .ok_or_else(|| internal("user", "resident account has no row"))?;
    let dek = verify_password(pool, &resident.user_id, &user.display_name, password).await?;
    if *dek != resident.data_key {
        // The password opens a different key than this session holds: the
        // account was rotated elsewhere.
        return Err(KeyRotationError::EpochChanged);
    }
    Ok((resident, user.display_name))
}

fn env_for<'a>(
    pool: &'a sqlx::SqlitePool,
    user_id: &'a str,
    require_closed_gate: bool,
    registry: Option<&'a KeyRegistry>,
) -> RotationEnv<'a> {
    let mut env = RotationEnv::new(pool, user_id, user_dir(user_id));
    env.require_closed_gate = require_closed_gate;
    env.registry = registry;
    env
}

/// Drain every writer of `user_id` and close the writer gate before a rotation
/// starts (items.id=778, decisions.id=896), following the user's choice. The
/// returned guard keeps the gate closed for the whole rotation; dropping it, on
/// success, error or panic, reopens it. Nothing is changed when this refuses.
async fn drain_writers(
    user_id: &str,
    choice: WritersChoice,
) -> Result<DrainGuard, KeyRotationError> {
    drain_writers_with(user_id, choice, writer_gate::DrainPolicy::default()).await
}

/// `drain_writers` with the waits spelled out (tests shorten them).
async fn drain_writers_with(
    user_id: &str,
    choice: WritersChoice,
    policy: writer_gate::DrainPolicy,
) -> Result<DrainGuard, KeyRotationError> {
    let to_u32 = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    writer_gate::drain_for_rotation(user_id, choice.into(), policy)
        .await
        .map_err(|e| match e {
            DrainError::Gate(GateError::NotLoggedIn) => KeyRotationError::NotLoggedIn,
            DrainError::Gate(GateError::DrainInProgress | GateError::Rotating) => {
                KeyRotationError::RotationInProgress
            }
            DrainError::Gate(other) => internal("drain", other),
            DrainError::WritersActive {
                replies,
                focus_runs,
            } => KeyRotationError::WritersActive {
                replies: to_u32(replies),
                focus_runs: to_u32(focus_runs),
            },
            DrainError::StillRunning {
                replies,
                focus_runs,
            } => KeyRotationError::WritersStillRunning {
                replies: to_u32(replies),
                focus_runs: to_u32(focus_runs),
            },
            DrainError::NotStopping => KeyRotationError::WriterNotStopping,
            DrainError::Cancelled => KeyRotationError::DrainCancelled,
        })
}

/// What a rotation command found out about the session while it ran: whether
/// the session must end, for whom and why (items.id=778, decisions.id=896).
/// Every COMPLETED rotation ends the session, and so does one that failed
/// after its new wraps were live (`NeedsResume`); a rollback or a refusal does
/// not. The engine has already cleared the key registry; the command then
/// finishes the lock (group keys, connect sessions, session rows) and tells
/// the webview.
#[derive(Default)]
struct SessionEnd {
    pending: Option<(String, LockReason)>,
}

impl SessionEnd {
    /// Record the end implied by `outcome`: `ok_reason` when it completed,
    /// `ChangeUnfinished` when it needs resuming, nothing otherwise.
    fn note<T>(
        &mut self,
        user_id: &str,
        outcome: &Result<T, RotationError>,
        ok_reason: LockReason,
    ) {
        match outcome {
            Ok(_) => self.pending = Some((user_id.to_owned(), ok_reason)),
            Err(e) => self.note_error(user_id, e),
        }
    }

    /// `note` for a failure: only `NeedsResume` ends the session.
    fn note_error(&mut self, user_id: &str, error: &RotationError) {
        if matches!(error, RotationError::NeedsResume { .. }) {
            self.pending = Some((user_id.to_owned(), LockReason::ChangeUnfinished));
        }
    }

    /// End the session if one was recorded: `lock_session` is the one clear
    /// path every lock goes through. Returns the reason to announce.
    async fn finish(
        self,
        pool: &sqlx::SqlitePool,
        key_registry: &KeyRegistry,
        group_key_registry: &GroupKeyRegistry,
        connect_sessions: &ConnectSessions,
    ) -> Option<LockReason> {
        let (user_id, reason) = self.pending?;
        if let Err(e) = lock_session(
            pool,
            key_registry,
            group_key_registry,
            connect_sessions,
            Some(&user_id),
            reason,
        )
        .await
        {
            // The key is already gone; only the session-row soft-expire failed.
            log::warn!("key_rotation: could not soft-expire the sessions after the change: {e}");
        }
        Some(reason)
    }
}

/// Cancel the wait of a rotation that is waiting for running work (the Cancel
/// button on the waiting screen). Nothing is changed: the waiting call returns
/// `drain_cancelled`, the gate reopens and the running work carries on. A
/// no-op when no wait is under way. Exempt from the drain (it never takes a
/// lease), so a closed gate cannot block it.
///
/// SESSIONLESS: the restore screen waits with nobody signed in, so this needs
/// no session. It cancels the signed-in account's wait if there is one, and
/// otherwise the wait that is running. It carries no data and reveals nothing.
#[tauri::command]
#[specta::specta]
pub async fn cancel_writer_wait(
    key_registry: State<'_, KeyRegistry>,
) -> Result<(), KeyRotationError> {
    let user = key_registry.user_id().await;
    writer_gate::cancel_any_wait(user.as_deref());
    Ok(())
}

// ---------------------------------------------------------------------------
// change_password
// ---------------------------------------------------------------------------

/// Change the account password. Re-verifies `old_password`, then rotates the
/// data key: everything is re-locked under a new key and the new password.
/// The recovery phrase is unchanged and stays valid. Returns when the change is
/// complete (or has failed); progress arrives as `task-progress` events. A
/// completed change ENDS THE SESSION (`session-locked`, reason
/// `password_changed`): the user signs in again with the new password.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn change_password(
    app_handle: tauri::AppHandle,
    old_password: String,
    new_password: String,
    writers: WritersChoice,
    key_registry: State<'_, KeyRegistry>,
    group_key_registry: State<'_, GroupKeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    connect_sessions: State<'_, ConnectSessions>,
) -> Result<(), KeyRotationError> {
    let (result, lock) = change_password_core(
        old_password,
        new_password,
        writers,
        key_registry,
        group_key_registry,
        pool,
        connect_sessions,
    )
    .await;
    if let Some(reason) = lock {
        emit_session_locked(&app_handle, reason);
    }
    result
}

/// The body of `change_password`, split out so tests can call it without an
/// `AppHandle` (same split as `logout`/`logout_core`). Returns the result and
/// the reason the session ended, if it did.
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub(crate) async fn change_password_core(
    old_password: String,
    new_password: String,
    writers: WritersChoice,
    key_registry: State<'_, KeyRegistry>,
    group_key_registry: State<'_, GroupKeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    connect_sessions: State<'_, ConnectSessions>,
) -> (Result<(), KeyRotationError>, Option<LockReason>) {
    let mut end = SessionEnd::default();
    let result = change_password_inner(
        old_password,
        new_password,
        writers,
        &key_registry,
        &pool,
        &mut end,
    )
    .await;
    let lock = end
        .finish(&pool, &key_registry, &group_key_registry, &connect_sessions)
        .await;
    (result, lock)
}

async fn change_password_inner(
    old_password: String,
    new_password: String,
    writers: WritersChoice,
    key_registry: &KeyRegistry,
    pool: &sqlx::SqlitePool,
    end: &mut SessionEnd,
) -> Result<(), KeyRotationError> {
    let old_password = Zeroizing::new(old_password);
    let new_password = Zeroizing::new(new_password);
    check_new_password(&new_password)?;
    let (resident, _) = authorise_session_change(key_registry, pool, &old_password).await?;
    // Only now, with the old password proven right, is "same as the current
    // password" a safe thing to say (before that it would be an oracle).
    if password_policy::same_password(&old_password, &new_password) {
        return Err(KeyRotationError::NewPasswordSameAsCurrent);
    }

    let _drain = drain_writers(&resident.user_id, writers).await?;
    let env = env_for(pool, &resident.user_id, true, Some(key_registry));
    let outcome = key_rotation::run_rotation(
        &env,
        &resident.data_key,
        resident.key_epoch,
        RotationRequest::PasswordChange {
            new_password: Zeroizing::new(new_password.as_bytes().to_vec()),
        },
    )
    .await;
    end.note(&resident.user_id, &outcome, LockReason::PasswordChanged);
    outcome.map(drop).map_err(|e| code_for(&e))
}

// ---------------------------------------------------------------------------
// New recovery phrase
// ---------------------------------------------------------------------------

const CONFIRM_WORDS: usize = 3;
const MAX_FAILED_CONFIRMATIONS: u8 = 5;

/// `CONFIRM_WORDS` distinct 1-based positions in 1..=24, chosen at random.
fn random_positions() -> Result<Vec<u32>, KeyRotationError> {
    let mut chosen: Vec<u32> = Vec::new();
    while chosen.len() < CONFIRM_WORDS {
        let mut byte = [0u8; 1];
        getrandom::fill(&mut byte).map_err(|e| internal("random", e))?;
        // 24 divides 240: reject 240..=255 so every position is equally likely.
        if byte[0] >= 240 {
            continue;
        }
        let position = u32::from(byte[0] % 24) + 1;
        if !chosen.contains(&position) {
            chosen.push(position);
        }
    }
    chosen.sort_unstable();
    Ok(chosen)
}

/// Start creating a new recovery phrase: re-verifies the password, generates
/// fresh recovery entropy, keeps it in memory only, and returns the 24 words
/// to show ONCE together with the positions to confirm. Nothing is rotated
/// yet, and the old words still work.
#[tauri::command]
#[specta::specta]
pub async fn begin_new_recovery_phrase(
    password: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<NewRecoveryPhrase, KeyRotationError> {
    let password = Zeroizing::new(password);
    authorise_session_change(&key_registry, &pool, &password).await?;

    let entropy = data_key::generate_recovery_entropy().map_err(|e| internal("entropy", e))?;
    let mnemonic =
        data_key::mnemonic_from_entropy(&entropy).map_err(|e| internal("mnemonic", e))?;
    let confirm_positions = random_positions()?;
    key_registry
        .set_pending_new_phrase(NewPhraseSlot {
            entropy,
            confirm_positions: confirm_positions.clone(),
            failed_confirmations: 0,
        })
        .await;
    Ok(NewRecoveryPhrase {
        mnemonic: mnemonic.to_string(),
        confirm_positions,
    })
}

/// Abandon a new phrase that was shown but not confirmed. A no-op when there
/// is none.
#[tauri::command]
#[specta::specta]
pub async fn cancel_new_recovery_phrase(
    key_registry: State<'_, KeyRegistry>,
) -> Result<(), KeyRotationError> {
    drop(key_registry.take_pending_new_phrase().await);
    Ok(())
}

fn normalise_word(w: &str) -> String {
    w.trim().to_lowercase()
}

/// Confirm the new phrase by re-entering the asked-for words, then rotate: the
/// data key changes, the password stays, and the old words stop working. Asks
/// for the password again (it is not held in memory in between). A completed
/// change ENDS THE SESSION (`session-locked`, reason `recovery_phrase_changed`):
/// the user signs in again with the same password.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn confirm_new_recovery_phrase(
    app_handle: tauri::AppHandle,
    password: String,
    answers: Vec<WordAnswer>,
    writers: WritersChoice,
    key_registry: State<'_, KeyRegistry>,
    group_key_registry: State<'_, GroupKeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    connect_sessions: State<'_, ConnectSessions>,
) -> Result<(), KeyRotationError> {
    let (result, lock) = confirm_new_recovery_phrase_core(
        password,
        answers,
        writers,
        key_registry,
        group_key_registry,
        pool,
        connect_sessions,
    )
    .await;
    if let Some(reason) = lock {
        emit_session_locked(&app_handle, reason);
    }
    result
}

/// The body of `confirm_new_recovery_phrase`, split out for tests (no `AppHandle`).
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub(crate) async fn confirm_new_recovery_phrase_core(
    password: String,
    answers: Vec<WordAnswer>,
    writers: WritersChoice,
    key_registry: State<'_, KeyRegistry>,
    group_key_registry: State<'_, GroupKeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    connect_sessions: State<'_, ConnectSessions>,
) -> (Result<(), KeyRotationError>, Option<LockReason>) {
    let mut end = SessionEnd::default();
    let result = confirm_new_recovery_phrase_inner(
        password,
        answers,
        writers,
        &key_registry,
        &pool,
        &mut end,
    )
    .await;
    let lock = end
        .finish(&pool, &key_registry, &group_key_registry, &connect_sessions)
        .await;
    (result, lock)
}

async fn confirm_new_recovery_phrase_inner(
    password: String,
    answers: Vec<WordAnswer>,
    writers: WritersChoice,
    key_registry: &KeyRegistry,
    pool: &sqlx::SqlitePool,
    end: &mut SessionEnd,
) -> Result<(), KeyRotationError> {
    let password = Zeroizing::new(password);
    let resident = resident(key_registry).await?;

    // Check the words before anything else is spent.
    let verdict = key_registry
        .with_pending_new_phrase(|slot| {
            let words: Vec<String> = data_key::mnemonic_from_entropy(&slot.entropy)
                .map(|m| m.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default();
            let correct = slot.confirm_positions.iter().all(|pos| {
                let want = words.get(*pos as usize - 1);
                let got = answers.iter().find(|a| a.position == *pos);
                matches!((want, got), (Some(w), Some(a)) if normalise_word(&a.word) == *w)
            });
            if !correct {
                slot.failed_confirmations += 1;
            }
            (correct, slot.failed_confirmations)
        })
        .await;
    match verdict {
        None => return Err(KeyRotationError::NoPendingPhrase),
        Some((false, failures)) => {
            if failures >= MAX_FAILED_CONFIRMATIONS {
                drop(key_registry.take_pending_new_phrase().await);
            }
            return Err(KeyRotationError::ConfirmationFailed);
        }
        Some((true, _)) => {}
    }

    authorise_session_change(key_registry, pool, &password).await?;
    let entropy = key_registry
        .with_pending_new_phrase(|slot| slot.entropy.clone())
        .await
        .ok_or(KeyRotationError::NoPendingPhrase)?;

    let _drain = drain_writers(&resident.user_id, writers).await?;
    let env = env_for(pool, &resident.user_id, true, Some(key_registry));
    let outcome = key_rotation::run_rotation(
        &env,
        &resident.data_key,
        resident.key_epoch,
        RotationRequest::NewPhrase {
            password: Zeroizing::new(password.as_bytes().to_vec()),
            recovery_entropy: entropy,
        },
    )
    .await;
    end.note(
        &resident.user_id,
        &outcome,
        LockReason::RecoveryPhraseChanged,
    );
    outcome.map_err(|e| code_for(&e))?;
    // Adopted: the held words have done their job (the engine has already
    // cleared the registry, pending slots included).
    drop(key_registry.take_pending_new_phrase().await);
    Ok(())
}

// ---------------------------------------------------------------------------
// Restore from the recovery phrase
// ---------------------------------------------------------------------------

/// Trim, lowercase and collapse whitespace so pasted or hand-typed words
/// compare equal.
fn normalise_phrase(phrase: &str) -> Zeroizing<String> {
    Zeroizing::new(
        phrase
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Forgot the password: set a new password with the 24 recovery words. The
/// words unseal the data key, the data key is rotated exactly as for a password
/// change, and the user then signs in with the new password: no session is
/// opened here (items.id=778; decisions.id=896 ends every rotation at the login
/// screen). The words themselves do not change. If a change was interrupted
/// earlier, this is also the way to finish it with the words.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub async fn restore_with_recovery_phrase(
    app_handle: tauri::AppHandle,
    display_name: String,
    phrase: String,
    new_password: String,
    writers: WritersChoice,
    key_registry: State<'_, KeyRegistry>,
    group_key_registry: State<'_, GroupKeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    connect_sessions: State<'_, ConnectSessions>,
) -> Result<(), KeyRotationError> {
    let (result, lock) = restore_with_recovery_phrase_core(
        display_name,
        phrase,
        new_password,
        writers,
        key_registry,
        group_key_registry,
        pool,
        connect_sessions,
    )
    .await;
    if let Some(reason) = lock {
        emit_session_locked(&app_handle, reason);
    }
    result
}

/// The body of `restore_with_recovery_phrase`, split out for tests (no `AppHandle`).
#[allow(clippy::too_many_arguments)] // Explicit architecture boundary; see D6-342/D6-346.
pub(crate) async fn restore_with_recovery_phrase_core(
    display_name: String,
    phrase: String,
    new_password: String,
    writers: WritersChoice,
    key_registry: State<'_, KeyRegistry>,
    group_key_registry: State<'_, GroupKeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    connect_sessions: State<'_, ConnectSessions>,
) -> (Result<(), KeyRotationError>, Option<LockReason>) {
    let mut end = SessionEnd::default();
    let result = restore_inner(display_name, phrase, new_password, writers, &pool, &mut end).await;
    let lock = end
        .finish(&pool, &key_registry, &group_key_registry, &connect_sessions)
        .await;
    (result, lock)
}

async fn restore_inner(
    display_name: String,
    phrase: String,
    new_password: String,
    writers: WritersChoice,
    pool: &sqlx::SqlitePool,
    end: &mut SessionEnd,
) -> Result<(), KeyRotationError> {
    let new_password = Zeroizing::new(new_password);
    check_new_password(&new_password)?;
    // The words' shape is checked first and says nothing about any account,
    // so an unknown account cannot be told apart by it.
    let entropy = data_key::entropy_from_mnemonic(&normalise_phrase(&phrase))
        .map_err(|_| KeyRotationError::PhraseInvalid)?;
    // Unknown account and wrong words must look the same.
    let user = user_store::find_user_by_display_name(pool, &display_name)
        .await
        .map_err(|e| internal("user", e))?
        .ok_or(KeyRotationError::InvalidCredentials)?;
    if is_locked_out(pool, &display_name)
        .await
        .map_err(|e| internal("lockout check", e))?
    {
        return Err(KeyRotationError::AccountLocked);
    }
    let state = user_store::get_key_wrapper_state(pool, &user.id)
        .await
        .map_err(|e| internal("wrapper", e))?
        .ok_or_else(|| internal("wrapper", "missing"))?;

    let wrong = || async {
        record_failed_attempt(pool, &display_name)
            .await
            .map_err(|e| internal("failure bookkeeping", e))?;
        Err::<(), _>(KeyRotationError::InvalidCredentials)
    };

    // An interrupted change: finish it with the words.
    if state.pending_rotation.is_some() {
        let pending = user_store::get_pending_rotation(pool, &user.id)
            .await
            .map_err(|e| internal("pending rotation", e))?
            .ok_or_else(|| internal("pending rotation", "flag without a row"))?;
        let unlock = match key_rotation::unlock_staged(
            &pending,
            &user.id,
            StagedCredential::Phrase(&entropy),
        )
        .await
        {
            Ok(u) => u,
            Err(StagedUnlockError::WrongCredential) => return wrong().await.map(|_| ()),
            Err(e) => return Err(code_for_unlock(&e)),
        };
        let _drain = drain_writers(&user.id, writers).await?;
        let env = env_for(pool, &user.id, true, None);
        let finished = match key_rotation::resume_rotation(
            &env,
            &pending,
            unlock.new_dek.clone(),
            unlock.credential(),
        )
        .await
        {
            Ok(ResumeOutcome::Completed(outcome)) => outcome,
            Ok(ResumeOutcome::RolledBack) => return Err(KeyRotationError::RolledBack),
            Err(e) => {
                end.note_error(&user.id, &e);
                return Err(code_for(&e));
            }
        };
        // The interrupted change is done, on whatever password it had chosen.
        // The person here forgot their password and typed a new one: run the
        // restore on top so the account ENDS on the typed password. (If this
        // second step fails the account stays on the finished change's
        // password and the failure is reported.) No session is opened: the
        // user signs in with the new password (items.id=778).
        let outcome = key_rotation::run_rotation(
            &env,
            &finished.new_dek,
            finished.to_epoch,
            RotationRequest::Restore {
                new_password: Zeroizing::new(new_password.as_bytes().to_vec()),
            },
        )
        .await;
        end.note(&user.id, &outcome, LockReason::PasswordChanged);
        return outcome.map(drop).map_err(|e| code_for(&e));
    }

    // The ordinary case: the words must derive the stored recovery key, then
    // unseal the live seal.
    let (secret, public) = data_key::derive_recovery_keypair(&entropy, &user.id);
    if public.as_bytes() != &state.wrapper.recovery_public_key {
        return wrong().await.map(|_| ());
    }
    let old_dek = match data_key::unseal_dek_with_recovery_key(
        &secret,
        &state.wrapper.recovery_seal,
        &user.id,
        state.wrapper.key_epoch,
        state.wrapper.format_version,
    ) {
        Ok(d) => d,
        Err(_) => return wrong().await.map(|_| ()),
    };

    let _drain = drain_writers(&user.id, writers).await?;
    let env = env_for(pool, &user.id, true, None);
    let outcome = key_rotation::run_rotation(
        &env,
        &old_dek,
        state.wrapper.key_epoch,
        RotationRequest::Restore {
            new_password: Zeroizing::new(new_password.as_bytes().to_vec()),
        },
    )
    .await;
    end.note(&user.id, &outcome, LockReason::PasswordChanged);
    match outcome {
        Ok(_) => Ok(()),
        // A forged seal unseals to a key that opens nothing real. ONLY a
        // failure to open integration_keys.db (the database every account has
        // and the first one tried) says the recovered key is not the account's:
        // that is indistinguishable from wrong words and counted the same way.
        // Any other unreadable database means the key was right and something
        // else is wrong with the data: reported as such, never as a failed
        // attempt.
        Err(RotationError::DatabaseUnreadable { rel_path, .. })
            if rel_path == "integration_keys.db" =>
        {
            log::warn!("key_rotation: the recovered key does not open the account's keys database");
            wrong().await.map(|_| ())
        }
        Err(e) => Err(code_for(&e)),
    }
}

// ---------------------------------------------------------------------------
// Cancel an interrupted rotation
// ---------------------------------------------------------------------------

/// What the user typed to prove they may cancel.
enum CancelWith<'a> {
    Password(&'a str),
    Phrase(&'a [u8; kdf::MASTER_KEY_LEN]),
}

/// Undo an interrupted, not yet promoted rotation. No session exists: the
/// account is identified by `display_name` and the credential is the one that
/// reaches the staged NEW key. See the module header for the answers.
async fn cancel_pending(
    pool: &sqlx::SqlitePool,
    display_name: &str,
    with: CancelWith<'_>,
) -> Result<(), KeyRotationError> {
    // Unknown account and wrong credential must look the same.
    let user = user_store::find_user_by_display_name(pool, display_name)
        .await
        .map_err(|e| internal("user", e))?
        .ok_or(KeyRotationError::InvalidCredentials)?;
    if is_locked_out(pool, display_name)
        .await
        .map_err(|e| internal("lockout check", e))?
    {
        return Err(KeyRotationError::AccountLocked);
    }
    let state = user_store::get_key_wrapper_state(pool, &user.id)
        .await
        .map_err(|e| internal("wrapper", e))?
        .ok_or_else(|| internal("wrapper", "missing"))?;

    let wrong = || async {
        record_failed_attempt(pool, display_name)
            .await
            .map_err(|e| internal("failure bookkeeping", e))?;
        Err::<(), _>(KeyRotationError::InvalidCredentials)
    };

    // Nothing to cancel is answered as a wrong credential, and counted as one.
    if state.pending_rotation.is_none() {
        return wrong().await;
    }
    let pending = user_store::get_pending_rotation(pool, &user.id)
        .await
        .map_err(|e| internal("pending rotation", e))?
        .ok_or_else(|| internal("pending rotation", "flag without a row"))?;

    let credential = match &with {
        CancelWith::Password(password) => StagedCredential::Password(password.as_bytes()),
        CancelWith::Phrase(entropy) => StagedCredential::Phrase(entropy),
    };
    let unlock = match key_rotation::unlock_staged(&pending, &user.id, credential).await {
        Ok(u) => u,
        Err(StagedUnlockError::WrongCredential) => return wrong().await,
        Err(e) => return Err(code_for_unlock(&e)),
    };

    // No session and no drain: cancel is an unlock-time path with no writers
    // (items.id=778 leaves it alone).
    let env = env_for(pool, &user.id, false, None);
    match key_rotation::cancel_rotation(&env, &pending, unlock.new_dek.clone()).await {
        Ok(ResumeOutcome::RolledBack) => Ok(()),
        Ok(ResumeOutcome::Completed(_)) => Err(internal("cancel", "reported a completed rotation")),
        Err(e) => Err(code_for(&e)),
    }
}

/// Cancel an interrupted password change, restore or new-phrase rotation by
/// typing the NEW password (for a new-phrase rotation, the account password).
/// On success the account is back on its old key with nothing pending; this does
/// not sign in, the user signs in with the OLD password. Refused with
/// `rotation_already_complete` once the change is promoted.
#[tauri::command]
#[specta::specta]
pub async fn cancel_interrupted_rotation_with_password(
    display_name: String,
    new_password: String,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<(), KeyRotationError> {
    let new_password = Zeroizing::new(new_password);
    cancel_pending(&pool, &display_name, CancelWith::Password(&new_password)).await
}

/// Cancel an interrupted rotation by typing the 24 recovery words (for a
/// new-phrase rotation, the NEW words). Same outcome as the password form.
#[tauri::command]
#[specta::specta]
pub async fn cancel_interrupted_rotation_with_phrase(
    display_name: String,
    phrase: String,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<(), KeyRotationError> {
    // The words' shape is checked first and says nothing about any account.
    let entropy = data_key::entropy_from_mnemonic(&normalise_phrase(&phrase))
        .map_err(|_| KeyRotationError::PhraseInvalid)?;
    cancel_pending(&pool, &display_name, CancelWith::Phrase(&entropy)).await
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Map the last remembered progress event to how the last rotation ended. Only
/// a final report counts: anything else (a crash left `rekeying`, a refused
/// resume left `resuming`) is `none`, and so is any moment a rotation runs,
/// because the remembered event may belong to the previous one.
fn last_outcome_of(running: bool, last: Option<&ProgressEvent>) -> LastRotationOutcome {
    if running {
        return LastRotationOutcome::None;
    }
    match last.map(|e| e.phase) {
        Some(key_rotation::Phase::Complete) => LastRotationOutcome::Complete,
        Some(key_rotation::Phase::RolledBack) => LastRotationOutcome::RolledBack,
        Some(key_rotation::Phase::Failed) => LastRotationOutcome::Failed,
        _ => LastRotationOutcome::None,
    }
}

/// Where a rotation stands. Safe to poll; reads no secret. Without a session
/// there is nothing to report beyond whether this process is rotating and how
/// the last rotation it ran ended.
#[tauri::command]
#[specta::specta]
pub async fn get_rotation_status(
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<RotationStatus, KeyRotationError> {
    let running = epoch_guard::is_rotating();
    let last = key_rotation::last_progress();
    let (phase, percent) = match last.as_ref().filter(|_| running) {
        Some(e) => {
            let pct = e
                .current
                .saturating_mul(100)
                .checked_div(e.total)
                .unwrap_or(0)
                .min(100) as u8;
            (Some(e.phase.as_str().to_owned()), pct)
        }
        None => (None, 0),
    };
    let user_id = key_registry.with_key(|k| k.user_id.clone()).await;
    let pending = match (&user_id, running) {
        (Some(user_id), false) => user_store::get_pending_rotation(&pool, user_id)
            .await
            .map_err(|e| internal("pending rotation", e))?,
        _ => None,
    };
    Ok(RotationStatus {
        running,
        phase,
        percent,
        interrupted: pending.is_some(),
        pending_kind: pending.map(|p| p.kind.as_str().to_owned()),
        last_outcome: last_outcome_of(running, last.as_ref()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::Manager;

    use crate::auth::key_rotation::{FaultPoint, Phase};
    use crate::commands::auth::{
        get_recovery_key_display, login, logout_core, LOGIN_ROTATION_INTERRUPTED,
    };
    use crate::persistence::{message_store, personal_store};
    use crate::test_support::ENV_MUTEX;

    const PASSWORD: &str = "password123";
    const NEW_PASSWORD: &str = "a different password";

    struct Harness {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        _tempdir: tempfile::TempDir,
        saved_root: Option<String>,
        app: tauri::App<tauri::test::MockRuntime>,
        /// The reasons the session ended with, in order (items.id=778).
        locks: std::sync::Mutex<Vec<LockReason>>,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            key_rotation::set_global_progress_sink(None);
            key_rotation::clear_last_progress();
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    impl Harness {
        async fn new() -> Harness {
            let lock = ENV_MUTEX.lock().await;
            let saved_root = std::env::var("QR_DATA_ROOT").ok();
            let tempdir = tempfile::tempdir().unwrap();
            std::env::set_var("QR_DATA_ROOT", tempdir.path());
            crate::persistence::migrations::migrate_shared_db()
                .await
                .unwrap();
            let pool = sqlx::SqlitePool::connect_with(
                crate::providers::utils::connect_options_unencrypted(
                    &crate::providers::utils::db_path_shared(),
                ),
            )
            .await
            .unwrap();
            let app = tauri::test::mock_app();
            app.manage(KeyRegistry::default());
            app.manage(GroupKeyRegistry::default());
            app.manage(crate::provider_credentials::session::ConnectSessions::default());
            app.manage(pool);
            Harness {
                _lock: lock,
                _tempdir: tempdir,
                saved_root,
                app,
                locks: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn registry(&self) -> State<'_, KeyRegistry> {
            self.app.state::<KeyRegistry>()
        }
        fn groups(&self) -> State<'_, GroupKeyRegistry> {
            self.app.state::<GroupKeyRegistry>()
        }
        fn pool(&self) -> State<'_, sqlx::SqlitePool> {
            self.app.state::<sqlx::SqlitePool>()
        }

        async fn login(&self, password: &str) -> Result<(), String> {
            login(
                "Alice".to_owned(),
                password.to_owned(),
                self.registry(),
                self.groups(),
                self.pool(),
            )
            .await
        }

        /// Create the account (bootstrap login), note the recovery words, and
        /// put some real data under the data key.
        async fn account(&self) -> String {
            self.login(PASSWORD).await.unwrap();
            let words = get_recovery_key_display(self.registry())
                .await
                .unwrap()
                .mnemonic;
            let (user_id, key) = self
                .registry()
                .with_key(|k| {
                    (
                        k.user_id.clone(),
                        crate::auth::registry::key_hex(&k.data_key),
                    )
                })
                .await
                .unwrap();
            personal_store::open_personal_db(&user_id, "persona-1", &key)
                .await
                .unwrap();
            words
        }

        async fn user_id(&self) -> String {
            user_store::find_user_by_display_name(&self.pool(), "Alice")
                .await
                .unwrap()
                .unwrap()
                .id
        }

        async fn failures(&self) -> i64 {
            sqlx::query_scalar("SELECT COUNT(*) FROM auth_failures")
                .fetch_one(&*self.pool())
                .await
                .unwrap()
        }

        async fn change_password(&self, old: &str, new: &str) -> Result<(), KeyRotationError> {
            self.change_password_choosing(old, new, WritersChoice::Ask)
                .await
        }

        async fn change_password_choosing(
            &self,
            old: &str,
            new: &str,
            writers: WritersChoice,
        ) -> Result<(), KeyRotationError> {
            let (result, lock) = change_password_core(
                old.to_owned(),
                new.to_owned(),
                writers,
                self.registry(),
                self.groups(),
                self.pool(),
                self.app.state(),
            )
            .await;
            self.locks.lock().unwrap().extend(lock);
            result
        }

        /// The reason the session last ended with, if it ended.
        fn last_lock(&self) -> Option<LockReason> {
            self.locks.lock().unwrap().last().copied()
        }

        /// The account's data key in hex, as a store opener wants it.
        async fn key_hex(&self) -> String {
            self.registry()
                .with_key(|k| crate::auth::registry::key_hex(&k.data_key))
                .await
                .unwrap()
        }

        async fn restore(&self, words: &str, new: &str) -> Result<(), KeyRotationError> {
            let (result, lock) = restore_with_recovery_phrase_core(
                "Alice".to_owned(),
                words.to_owned(),
                new.to_owned(),
                WritersChoice::Ask,
                self.registry(),
                self.groups(),
                self.pool(),
                self.app.state(),
            )
            .await;
            self.locks.lock().unwrap().extend(lock);
            result
        }

        /// Assert that nothing is resident and nothing is open: the registry,
        /// the resident epoch, the pending slots and the gate are all empty,
        /// and the OLD data key opens no database (items.id=778).
        async fn assert_nothing_resident(&self, user_id: &str, old_key: &str) {
            assert!(self.registry().user_id().await.is_none(), "registry key");
            assert!(
                self.registry()
                    .with_pending_new_phrase(|_| ())
                    .await
                    .is_none(),
                "pending phrase slot"
            );
            assert_eq!(epoch_guard::resident_epoch(user_id), None, "resident epoch");
            assert!(writer_gate::is_idle(user_id), "gate idle, zero leases");
            assert!(
                personal_store::open_personal_db(user_id, "persona-1", old_key)
                    .await
                    .is_err(),
                "the old key opens no store"
            );
        }

        /// Leave an interrupted rotation behind, as a crash would, and lock
        /// the process (a restart).
        async fn crash_a_password_change(&self, fault: FaultPoint) {
            self.crash_with(vec![fault]).await;
        }

        /// Run a password change that dies at the last of `faults`.
        async fn crash_with(&self, faults: Vec<FaultPoint>) {
            let err = self
                .run_password_change_with(faults, NEW_PASSWORD)
                .await
                .unwrap_err();
            assert!(matches!(err, RotationError::SimulatedCrash), "{err:?}");
        }

        /// Run a password change from the resident session with faults armed,
        /// returning whatever the engine returned.
        async fn run_password_change_with(
            &self,
            faults: Vec<FaultPoint>,
            new_password: &str,
        ) -> Result<key_rotation::RotationOutcome, RotationError> {
            self.run_with(
                faults,
                RotationRequest::PasswordChange {
                    new_password: Zeroizing::new(new_password.as_bytes().to_vec()),
                },
            )
            .await
        }

        async fn run_with(
            &self,
            faults: Vec<FaultPoint>,
            request: RotationRequest,
        ) -> Result<key_rotation::RotationOutcome, RotationError> {
            let (user_id, dek, epoch) = self
                .registry()
                .with_key(|k| (k.user_id.clone(), k.data_key, k.key_epoch))
                .await
                .unwrap();
            let pool = self.pool();
            let mut env = RotationEnv::new(&pool, &user_id, user_dir(&user_id));
            env.faults = faults;
            key_rotation::run_rotation(&env, &dek, epoch, request).await
        }

        async fn cancel_password(
            &self,
            display_name: &str,
            password: &str,
        ) -> Result<(), KeyRotationError> {
            cancel_interrupted_rotation_with_password(
                display_name.to_owned(),
                password.to_owned(),
                self.pool(),
            )
            .await
        }

        async fn cancel_phrase(
            &self,
            display_name: &str,
            words: &str,
        ) -> Result<(), KeyRotationError> {
            cancel_interrupted_rotation_with_phrase(
                display_name.to_owned(),
                words.to_owned(),
                self.pool(),
            )
            .await
        }

        async fn status(&self) -> RotationStatus {
            get_rotation_status(self.registry(), self.pool())
                .await
                .unwrap()
        }

        async fn restart(&self) {
            let user_id = self.user_id().await;
            self.registry().clear().await;
            epoch_guard::clear_resident_for(&user_id);
        }
    }

    // -- change_password ----------------------------------------------------

    #[tokio::test]
    async fn change_password_rotates_and_ends_the_session() {
        let h = Harness::new().await;
        h.account().await;
        let user_id = h.user_id().await;
        let old_key = h.key_hex().await;

        h.change_password(PASSWORD, NEW_PASSWORD).await.unwrap();

        // The session ended: nothing is resident, nothing is open, the webview
        // is told why (items.id=778).
        assert_eq!(h.last_lock(), Some(LockReason::PasswordChanged));
        h.assert_nothing_resident(&user_id, &old_key).await;
        assert_eq!(
            h.change_password(NEW_PASSWORD, "another valid password")
                .await
                .unwrap_err(),
            KeyRotationError::NotLoggedIn
        );

        // Old password is dead, new password signs in at the next epoch.
        assert_eq!(h.login(PASSWORD).await.unwrap_err(), "invalid credentials");
        h.login(NEW_PASSWORD).await.unwrap();
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(2));
        let new_key = h.key_hex().await;
        assert_ne!(new_key, old_key);
        assert!(
            personal_store::open_personal_db(&user_id, "persona-1", &new_key)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn change_password_refuses_without_a_session_with_a_bad_password_or_an_empty_new_one() {
        let h = Harness::new().await;
        assert_eq!(
            h.change_password(PASSWORD, NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::NotLoggedIn
        );
        h.account().await;
        assert_eq!(
            h.change_password(PASSWORD, "").await.unwrap_err(),
            KeyRotationError::NewPasswordEmpty
        );
        assert_eq!(h.failures().await, 0);

        assert_eq!(
            h.change_password("not the password", NEW_PASSWORD)
                .await
                .unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        // Recorded exactly as login records a wrong password.
        assert_eq!(h.failures().await, 1);
        // Nothing changed: the old password still signs in.
        h.registry().clear().await;
        epoch_guard::clear_resident_for(&h.user_id().await);
        h.login(PASSWORD).await.unwrap();
    }

    #[test]
    fn only_a_completed_or_unfinished_rotation_ends_the_session() {
        // items.id=778: complete ends it with the kind's reason, a failure after
        // promotion ends it as unfinished, a rollback or refusal does not.
        let ok: Result<(), RotationError> = Ok(());
        let needs: Result<(), RotationError> =
            Err(RotationError::NeedsResume { cause: "x".into() });
        let rolled: Result<(), RotationError> =
            Err(RotationError::RolledBack { cause: "x".into() });
        let busy: Result<(), RotationError> = Err(RotationError::ActiveRuns(1));

        let mut end = SessionEnd::default();
        end.note("u", &ok, LockReason::RecoveryPhraseChanged);
        assert_eq!(
            end.pending,
            Some(("u".into(), LockReason::RecoveryPhraseChanged))
        );

        let mut end = SessionEnd::default();
        end.note("u", &needs, LockReason::PasswordChanged);
        assert_eq!(
            end.pending,
            Some(("u".into(), LockReason::ChangeUnfinished))
        );

        for no in [&rolled, &busy] {
            let mut end = SessionEnd::default();
            end.note("u", no, LockReason::PasswordChanged);
            assert_eq!(end.pending, None);
        }
    }

    #[tokio::test]
    async fn finishing_a_session_end_clears_everything_and_reports_the_reason() {
        let h = Harness::new().await;
        h.account().await;
        let user_id = h.user_id().await;
        let mut end = SessionEnd::default();
        end.note::<()>(&user_id, &Ok(()), LockReason::PasswordChanged);
        let reason = end
            .finish(&h.pool(), &h.registry(), &h.groups(), &h.app.state())
            .await;
        assert_eq!(reason, Some(LockReason::PasswordChanged));
        assert!(h.registry().user_id().await.is_none());
        assert_eq!(epoch_guard::resident_epoch(&user_id), None);
        // Nothing recorded, nothing ended.
        let none = SessionEnd::default()
            .finish(&h.pool(), &h.registry(), &h.groups(), &h.app.state())
            .await;
        assert_eq!(none, None);
    }

    #[tokio::test]
    async fn a_running_reply_is_reported_first_and_nothing_is_changed() {
        let h = Harness::new().await;
        h.account().await;
        let user_id = h.user_id().await;
        // A chat reply in flight holds a lease (items.id=778): the first call
        // reports it, changes nothing and leaves the gate open.
        let reply = writer_gate::acquire(&user_id, writer_gate::Kind::Reply).unwrap();
        assert_eq!(
            h.change_password(PASSWORD, NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::WritersActive {
                replies: 1,
                focus_runs: 0
            }
        );
        assert_eq!(writer_gate::phase(&user_id), writer_gate::Phase::Open);
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(1));
        assert!(!reply.stop_token().is_cancelled(), "nothing was stopped");
        drop(reply);
        h.change_password(PASSWORD, NEW_PASSWORD).await.unwrap();
        assert!(writer_gate::is_idle(&user_id));
    }

    /// Wait (bounded) until the gate of `user_id` reaches `want`: the rotation
    /// command verifies the password first, which takes seconds, before it
    /// drains.
    async fn wait_for_phase(user_id: &str, want: writer_gate::Phase) {
        for _ in 0..600 {
            if writer_gate::phase(user_id) == want {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("the gate never reached {want:?}");
    }

    /// Sign in again with `password` after a rotation (the session may or may
    /// not have ended) and return the data key in hex.
    async fn sign_in_again(h: &Harness, password: &str) -> String {
        h.registry().clear().await;
        epoch_guard::clear_resident_for(&h.user_id().await);
        h.login(password).await.unwrap();
        h.key_hex().await
    }

    #[tokio::test]
    async fn finish_first_holds_the_rotation_until_the_reply_ends() {
        let h = Harness::new().await;
        h.account().await;
        let user_id = h.user_id().await;
        let old_key = h.key_hex().await;
        let reply = writer_gate::acquire(&user_id, writer_gate::Kind::Reply).unwrap();

        let rotate = h.change_password_choosing(PASSWORD, NEW_PASSWORD, WritersChoice::FinishFirst);
        let writer = async {
            wait_for_phase(&user_id, writer_gate::Phase::Draining).await;
            // Held: the rotation has not started, the key is still the old
            // one, the gate refuses new replies, and the reply can still save.
            assert_eq!(writer_gate::phase(&user_id), writer_gate::Phase::Draining);
            assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(1));
            assert!(writer_gate::acquire(&user_id, writer_gate::Kind::Reply).is_err());
            message_store::save_message(
                &user_id,
                "persona-1",
                &old_key,
                "ctx-held",
                "assistant",
                "the whole reply",
                None,
            )
            .await
            .unwrap();
            drop(reply);
        };
        let (rotated, ()) = tokio::join!(rotate, writer);
        rotated.unwrap();

        // The reply written while the rotation waited is under the new key.
        let new_key = sign_in_again(&h, NEW_PASSWORD).await;
        let saved = message_store::list_messages(&user_id, "persona-1", &new_key, "ctx-held")
            .await
            .unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].content, "the whole reply");
    }

    #[tokio::test]
    async fn stop_and_save_stops_the_reply_then_rotates() {
        let h = Harness::new().await;
        h.account().await;
        let user_id = h.user_id().await;
        let old_key = h.key_hex().await;
        let reply = writer_gate::acquire(&user_id, writer_gate::Kind::Reply).unwrap();
        let stop = reply.stop_token().clone();

        let rotate = h.change_password_choosing(PASSWORD, NEW_PASSWORD, WritersChoice::StopAndSave);
        // A cooperative writer: told to stop, it saves what it has and ends.
        let writer = async {
            stop.cancelled().await;
            message_store::save_message(
                &user_id,
                "persona-1",
                &old_key,
                "ctx-stopped",
                "assistant",
                "half an answ",
                None,
            )
            .await
            .unwrap();
            drop(reply);
        };
        let (rotated, ()) = tokio::join!(rotate, writer);
        rotated.unwrap();

        let new_key = sign_in_again(&h, NEW_PASSWORD).await;
        let saved = message_store::list_messages(&user_id, "persona-1", &new_key, "ctx-stopped")
            .await
            .unwrap();
        assert_eq!(saved[0].content, "half an answ");
    }

    #[tokio::test]
    async fn a_reply_that_never_stops_is_reported_and_nothing_is_changed() {
        let h = Harness::new().await;
        h.account().await;
        let user_id = h.user_id().await;
        let reply = writer_gate::acquire(&user_id, writer_gate::Kind::FocusRun).unwrap();
        // The grace is the real 60 s constant; a paused clock would need the
        // whole harness on a paused runtime, so the policy itself is tested
        // with paused time in auth::writer_gate and this checks the mapping.
        let err = super::drain_writers_with(
            &user_id,
            WritersChoice::StopAndSave,
            writer_gate::DrainPolicy {
                finish_first: std::time::Duration::from_millis(50),
                stop_grace: std::time::Duration::from_millis(50),
                commands: std::time::Duration::from_millis(50),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err, KeyRotationError::WriterNotStopping);
        assert!(reply.stop_token().is_cancelled());
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(1));
        assert_eq!(writer_gate::phase(&user_id), writer_gate::Phase::Open);
        // Cancel mapping and the still-running message.
        let err = super::drain_writers_with(
            &user_id,
            WritersChoice::FinishFirst,
            writer_gate::DrainPolicy {
                finish_first: std::time::Duration::from_millis(50),
                stop_grace: std::time::Duration::from_millis(50),
                commands: std::time::Duration::from_millis(50),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            err,
            KeyRotationError::WritersStillRunning {
                replies: 0,
                focus_runs: 1
            }
        );
    }

    #[tokio::test]
    async fn cancel_writer_wait_ends_a_waiting_rotation_and_changes_nothing() {
        let h = Harness::new().await;
        h.account().await;
        let user_id = h.user_id().await;
        let reply = writer_gate::acquire(&user_id, writer_gate::Kind::Reply).unwrap();
        let rotate = h.change_password_choosing(PASSWORD, NEW_PASSWORD, WritersChoice::FinishFirst);
        let cancel = async {
            wait_for_phase(&user_id, writer_gate::Phase::Draining).await;
            cancel_writer_wait(h.registry()).await.unwrap();
        };
        let (rotated, ()) = tokio::join!(rotate, cancel);
        assert_eq!(rotated.unwrap_err(), KeyRotationError::DrainCancelled);
        assert_eq!(writer_gate::phase(&user_id), writer_gate::Phase::Open);
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(1));
        assert!(!reply.stop_token().is_cancelled(), "the reply carries on");
        drop(reply);
    }

    // -- new-password policy (items.id=777, decisions.id=858) -----------

    const FIFTEEN: &str = "fifteen chars!!"; // exactly 15, not listed
    const FOURTEEN: &str = "fourteen chars"; // one short of the minimum
    const LISTED_LONG: &str = "123456789daniel"; // on the pinned blocklist

    #[tokio::test]
    async fn change_password_enforces_the_policy_with_distinct_codes() {
        let h = Harness::new().await;
        h.account().await;
        assert_eq!(FIFTEEN.chars().count(), 15);
        assert_eq!(FOURTEEN.chars().count(), 14);

        assert_eq!(
            h.change_password(PASSWORD, FOURTEEN).await.unwrap_err(),
            KeyRotationError::NewPasswordTooShort
        );
        assert_eq!(
            h.change_password(PASSWORD, LISTED_LONG).await.unwrap_err(),
            KeyRotationError::NewPasswordBlocked
        );
        assert_eq!(
            h.change_password(PASSWORD, &LISTED_LONG.to_uppercase())
                .await
                .unwrap_err(),
            KeyRotationError::NewPasswordBlocked
        );
        // A verdict on the NEW password says nothing about the old one: it is
        // given before the old password is checked and is not a failed attempt.
        assert_eq!(
            h.change_password("not the password", "short")
                .await
                .unwrap_err(),
            KeyRotationError::NewPasswordTooShort
        );
        assert_eq!(h.failures().await, 0);
        // Nothing started: no pending change, the old password still opens.
        assert!(!h.status().await.interrupted);
        h.registry().clear().await;
        epoch_guard::clear_resident_for(&h.user_id().await);
        h.login(PASSWORD).await.unwrap();

        // Exactly 15 is accepted.
        h.change_password(PASSWORD, FIFTEEN).await.unwrap();
        h.registry().clear().await;
        epoch_guard::clear_resident_for(&h.user_id().await);
        h.login(FIFTEEN).await.unwrap();
    }

    #[tokio::test]
    async fn change_password_length_is_counted_in_characters_after_normalisation() {
        let h = Harness::new().await;
        h.account().await;
        // 8 CJK characters are 24 bytes but 8 characters.
        let cjk = "\u{65e5}\u{672c}\u{8a9e}\u{6f22}\u{5b57}\u{4eba}\u{6c11}\u{6c34}";
        assert!(cjk.len() >= 15);
        assert_eq!(
            h.change_password(PASSWORD, cjk).await.unwrap_err(),
            KeyRotationError::NewPasswordTooShort
        );
        // 14 decomposed accented letters are 14 characters (28 bytes).
        assert_eq!(
            h.change_password(PASSWORD, &"e\u{301}".repeat(14))
                .await
                .unwrap_err(),
            KeyRotationError::NewPasswordTooShort
        );
        // 15 emoji are 15 characters.
        h.change_password(PASSWORD, &"\u{1f600}".repeat(15))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn change_password_refuses_the_current_password_only_after_verifying_it() {
        let h = Harness::new().await;
        h.account().await;
        // Move to a non-ASCII password so the test covers normalisation too.
        let current = "Cr\u{e8}me br\u{fb}l\u{e9}e is fifteen+";
        h.change_password(PASSWORD, current).await.unwrap();
        h.login(current).await.unwrap();

        assert_eq!(
            h.change_password(current, current).await.unwrap_err(),
            KeyRotationError::NewPasswordSameAsCurrent
        );
        // The same text typed in decomposed form is the same password (same
        // key), so it is refused too.
        let decomposed = "Cre\u{300}me bru\u{302}le\u{301}e is fifteen+";
        assert_ne!(current, decomposed);
        assert_eq!(
            h.change_password(current, decomposed).await.unwrap_err(),
            KeyRotationError::NewPasswordSameAsCurrent
        );
        assert_eq!(h.failures().await, 0);
        assert!(!h.status().await.interrupted);

        // A WRONG old password that happens to equal the new one must not be
        // told apart from any other wrong password: no "same as current".
        let wrong = "this is not the password";
        assert_eq!(
            h.change_password(wrong, wrong).await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(h.failures().await, 1);

        // And no password history is kept: the previous password is allowed
        // again.
        h.change_password(current, NEW_PASSWORD).await.unwrap();
        h.login(NEW_PASSWORD).await.unwrap();
        h.change_password(NEW_PASSWORD, current).await.unwrap();
    }

    #[tokio::test]
    async fn restore_enforces_the_policy_before_touching_the_account() {
        let h = Harness::new().await;
        let words = h.account().await;
        h.registry().clear().await;
        epoch_guard::clear_resident_for(&h.user_id().await);

        assert_eq!(
            h.restore(&words, "short").await.unwrap_err(),
            KeyRotationError::NewPasswordTooShort
        );
        assert_eq!(
            h.restore(&words, LISTED_LONG).await.unwrap_err(),
            KeyRotationError::NewPasswordBlocked
        );
        // Judged before the words: wrong words with a bad password still get
        // the password verdict, and nothing is counted or changed.
        assert_eq!(
            h.restore("one two three", "short").await.unwrap_err(),
            KeyRotationError::NewPasswordTooShort
        );
        assert_eq!(h.failures().await, 0);
        h.login(PASSWORD).await.unwrap();
        h.registry().clear().await;
        epoch_guard::clear_resident_for(&h.user_id().await);

        h.restore(&words, FIFTEEN).await.unwrap();
        h.registry().clear().await;
        epoch_guard::clear_resident_for(&h.user_id().await);
        h.login(FIFTEEN).await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_and_resuming_never_apply_the_policy() {
        // A change interrupted with a password that predates the policy (here a
        // 4-character one, started below the command layer) must still be
        // cancellable with that password, and a sign-in with it must still
        // finish the change.
        for resume in [false, true] {
            let h = Harness::new().await;
            h.account().await;
            let err = h
                .run_password_change_with(vec![FaultPoint::AfterItemCommitted(0)], "weak")
                .await
                .unwrap_err();
            assert!(matches!(err, RotationError::SimulatedCrash), "{err:?}");
            h.restart().await;

            if resume {
                h.login("weak").await.unwrap();
            } else {
                h.cancel_password("Alice", "weak").await.unwrap();
                h.login(PASSWORD).await.unwrap();
            }
        }
    }

    // -- interrupted change, through login ------------------------------

    #[tokio::test]
    async fn the_old_password_after_an_interruption_gets_the_interrupted_result() {
        let h = Harness::new().await;
        h.account().await;
        h.crash_a_password_change(FaultPoint::AfterItemCommitted(0))
            .await;
        h.restart().await;

        // Old password: not a session, not a failed attempt, a named result.
        assert_eq!(
            h.login(PASSWORD).await.unwrap_err(),
            LOGIN_ROTATION_INTERRUPTED
        );
        assert!(!h.registry().is_occupied().await);
        assert_eq!(h.failures().await, 0);

        // A wrong password is just wrong, and counted.
        assert_eq!(
            h.login("something else").await.unwrap_err(),
            "invalid credentials"
        );
        assert_eq!(h.failures().await, 1);
        assert!(!h.registry().is_occupied().await);
    }

    #[tokio::test]
    async fn the_new_password_after_an_interruption_finishes_it_and_signs_in() {
        let h = Harness::new().await;
        h.account().await;
        let user_id = h.user_id().await;
        h.crash_a_password_change(FaultPoint::AfterItemCommitted(0))
            .await;
        h.restart().await;

        h.login(NEW_PASSWORD).await.unwrap();
        let (key, epoch) = h
            .registry()
            .with_key(|k| (crate::auth::registry::key_hex(&k.data_key), k.key_epoch))
            .await
            .unwrap();
        assert_eq!(epoch, 2);
        assert!(
            personal_store::open_personal_db(&user_id, "persona-1", &key)
                .await
                .is_ok()
        );
        assert!(user_store::get_pending_rotation(&h.pool(), &user_id)
            .await
            .unwrap()
            .is_none());

        // And from now on it is an ordinary account.
        h.registry().clear().await;
        epoch_guard::clear_resident_for(&user_id);
        assert_eq!(h.login(PASSWORD).await.unwrap_err(), "invalid credentials");
        h.login(NEW_PASSWORD).await.unwrap();
    }

    #[tokio::test]
    async fn the_recovery_phrase_finishes_an_interrupted_change() {
        let h = Harness::new().await;
        let words = h.account().await;
        let user_id = h.user_id().await;
        h.crash_a_password_change(FaultPoint::AfterAllItems).await;
        h.restart().await;

        h.restore(&words, "the one typed at restore").await.unwrap();
        // No session is opened by a restore: the user signs in (items.id=778).
        assert!(!h.registry().is_occupied().await);
        assert_eq!(h.last_lock(), Some(LockReason::PasswordChanged));
        assert!(user_store::get_pending_rotation(&h.pool(), &user_id)
            .await
            .unwrap()
            .is_none());
        // Two rotations ran: the interrupted change (epoch 2) and then the
        // restore on top (epoch 3), so the account ENDS on the typed password.
        assert_eq!(h.login(PASSWORD).await.unwrap_err(), "invalid credentials");
        assert_eq!(
            h.login(NEW_PASSWORD).await.unwrap_err(),
            "invalid credentials",
            "the interrupted change's password was superseded"
        );
        h.login("the one typed at restore").await.unwrap();
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(3));
    }

    // -- restore --------------------------------------------------------

    #[tokio::test]
    async fn cancel_ends_a_waiting_restore_with_nobody_signed_in_and_changes_nothing() {
        // Cancelling with no session cancels whichever wait is running, so
        // keep the gate's own drain tests from overlapping this one.
        let _serial = writer_gate::DRAIN_SERIAL.lock().await;
        let h = Harness::new().await;
        let words = h.account().await;
        let user_id = h.user_id().await;
        // A reply from the earlier session is still running after the lock.
        let reply = writer_gate::acquire(&user_id, writer_gate::Kind::Reply).unwrap();
        logout_core(h.registry(), h.groups(), h.pool(), h.app.state())
            .await
            .unwrap();
        epoch_guard::clear_resident_for(&user_id);
        assert!(h.registry().user_id().await.is_none(), "no session");

        let restore = restore_with_recovery_phrase_core(
            "Alice".to_owned(),
            words.clone(),
            NEW_PASSWORD.to_owned(),
            WritersChoice::FinishFirst,
            h.registry(),
            h.groups(),
            h.pool(),
            h.app.state(),
        );
        let cancel = async {
            wait_for_phase(&user_id, writer_gate::Phase::Draining).await;
            cancel_writer_wait(h.registry()).await.unwrap();
        };
        let (restored, ()) = tokio::join!(restore, cancel);
        assert_eq!(restored.0.unwrap_err(), KeyRotationError::DrainCancelled);
        assert_eq!(
            restored.1, None,
            "a cancelled change does not end the session"
        );
        assert_eq!(writer_gate::phase(&user_id), writer_gate::Phase::Open);
        assert!(!reply.stop_token().is_cancelled(), "the reply carries on");
        drop(reply);

        // Nothing changed: the old password still signs in, at the old epoch.
        h.login(PASSWORD).await.unwrap();
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(1));
    }

    #[tokio::test]
    async fn restore_with_the_words_sets_a_new_password_and_ends_at_login() {
        let h = Harness::new().await;
        let words = h.account().await;
        let user_id = h.user_id().await;
        let old_key = h.key_hex().await;
        logout_core(h.registry(), h.groups(), h.pool(), h.app.state())
            .await
            .unwrap();
        epoch_guard::clear_resident_for(&user_id);

        h.restore(&words, NEW_PASSWORD).await.unwrap();
        // No session is opened: the user signs in with the new password.
        assert_eq!(h.last_lock(), Some(LockReason::PasswordChanged));
        h.assert_nothing_resident(&user_id, &old_key).await;
        h.login(NEW_PASSWORD).await.unwrap();
        let (key, epoch) = h
            .registry()
            .with_key(|k| (crate::auth::registry::key_hex(&k.data_key), k.key_epoch))
            .await
            .unwrap();
        assert_eq!(epoch, 2);
        assert!(
            personal_store::open_personal_db(&user_id, "persona-1", &key)
                .await
                .is_ok()
        );

        h.registry().clear().await;
        epoch_guard::clear_resident_for(&user_id);
        assert_eq!(h.login(PASSWORD).await.unwrap_err(), "invalid credentials");
        h.login(NEW_PASSWORD).await.unwrap();

        // The SAME words still work after the restore.
        logout_core(h.registry(), h.groups(), h.pool(), h.app.state())
            .await
            .unwrap();
        epoch_guard::clear_resident_for(&user_id);
        h.restore(&words, "yet another password").await.unwrap();
    }

    #[tokio::test]
    async fn restore_gives_the_same_answer_for_wrong_words_and_an_unknown_account() {
        let h = Harness::new().await;
        let words = h.account().await;
        let user_id = h.user_id().await;
        logout_core(h.registry(), h.groups(), h.pool(), h.app.state())
            .await
            .unwrap();
        epoch_guard::clear_resident_for(&user_id);

        // Valid words that are not this account's.
        let stranger =
            data_key::mnemonic_from_entropy(&data_key::generate_recovery_entropy().unwrap())
                .unwrap();
        assert_eq!(
            h.restore(&stranger, NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(h.failures().await, 1, "a wrong phrase is a failed attempt");

        // The right words for a name that does not exist.
        assert_eq!(
            restore_with_recovery_phrase_core(
                "Nobody".to_owned(),
                words.clone(),
                NEW_PASSWORD.to_owned(),
                WritersChoice::Ask,
                h.registry(),
                h.groups(),
                h.pool(),
                h.app.state(),
            )
            .await
            .0
            .unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(
            h.failures().await,
            1,
            "no account, nothing to count against"
        );

        // Words that are not a phrase at all say so, and say nothing about
        // any account.
        assert_eq!(
            h.restore("one two three", NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::PhraseInvalid
        );
        assert_eq!(
            h.restore(&words, "").await.unwrap_err(),
            KeyRotationError::NewPasswordEmpty
        );
        // Messy typing is forgiven.
        let messy = format!("  {}  ", words.to_uppercase().replace(' ', "   "));
        h.restore(&messy, NEW_PASSWORD).await.unwrap();
    }

    #[tokio::test]
    async fn a_forged_seal_counts_as_a_wrong_phrase() {
        let h = Harness::new().await;
        let words = h.account().await;
        let user_id = h.user_id().await;
        logout_core(h.registry(), h.groups(), h.pool(), h.app.state())
            .await
            .unwrap();
        epoch_guard::clear_resident_for(&user_id);

        // Someone who can write shared.db seals a key of their own to the
        // (public) recovery key, with the right user, epoch and version.
        let state = user_store::get_key_wrapper_state(&h.pool(), &user_id)
            .await
            .unwrap()
            .unwrap();
        let forged = data_key::seal_dek_to_recovery_key(
            &state.wrapper.recovery_public_key,
            &data_key::generate_dek().unwrap(),
            &user_id,
            state.wrapper.key_epoch,
            state.wrapper.format_version,
        )
        .unwrap();
        sqlx::query("UPDATE user_key_wrappers SET recovery_seal_hex = ? WHERE user_id = ?")
            .bind(
                forged
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
            )
            .bind(&user_id)
            .execute(&*h.pool())
            .await
            .unwrap();

        assert_eq!(
            h.restore(&words, NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(h.failures().await, 1, "failing the keys database counts");
        assert!(!h.registry().is_occupied().await);
    }

    #[tokio::test]
    async fn another_unreadable_database_is_reported_as_such_and_is_not_a_failed_attempt() {
        let h = Harness::new().await;
        let words = h.account().await;
        let user_id = h.user_id().await;
        logout_core(h.registry(), h.groups(), h.pool(), h.app.state())
            .await
            .unwrap();
        epoch_guard::clear_resident_for(&user_id);

        // The words are right; one persona database is damaged.
        let damaged = crate::providers::utils::db_path_personal(&user_id, "persona-1");
        std::fs::write(&damaged, b"damaged on disk").unwrap();

        assert_eq!(
            h.restore(&words, NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::DatabaseUnreadable
        );
        assert_eq!(h.failures().await, 0, "nothing was guessed wrong");
        assert!(!h.registry().is_occupied().await);
        // And nothing was changed: no rotation is pending.
        assert!(user_store::get_pending_rotation(&h.pool(), &user_id)
            .await
            .unwrap()
            .is_none());
    }

    // -- new recovery phrase --------------------------------------------

    fn answers_for(words: &str, positions: &[u32]) -> Vec<WordAnswer> {
        let list: Vec<&str> = words.split_whitespace().collect();
        positions
            .iter()
            .map(|p| WordAnswer {
                position: *p,
                word: list[*p as usize - 1].to_owned(),
            })
            .collect()
    }

    #[tokio::test]
    async fn a_new_phrase_is_shown_confirmed_then_adopted_and_the_old_one_is_revoked() {
        let h = Harness::new().await;
        let old_words = h.account().await;
        let user_id = h.user_id().await;
        let old_key = h.key_hex().await;

        let shown = begin_new_recovery_phrase(PASSWORD.to_owned(), h.registry(), h.pool())
            .await
            .unwrap();
        assert_eq!(shown.mnemonic.split_whitespace().count(), 24);
        assert_eq!(shown.confirm_positions.len(), 3);
        assert!(shown.confirm_positions.iter().all(|p| (1..=24).contains(p)));
        assert_ne!(shown.mnemonic, old_words);

        // Nothing has changed yet: the epoch is the same.
        assert_eq!(
            h.registry().with_key(|k| k.key_epoch).await,
            Some(1),
            "showing the words rotates nothing"
        );

        // Wrong words, then the right ones.
        let mut wrong = answers_for(&shown.mnemonic, &shown.confirm_positions);
        wrong[0].word = "zoo".to_owned();
        if wrong[0].word == answers_for(&shown.mnemonic, &shown.confirm_positions)[0].word {
            wrong[0].word = "abandon".to_owned();
        }
        assert_eq!(
            confirm_new_recovery_phrase_core(
                PASSWORD.to_owned(),
                wrong,
                WritersChoice::Ask,
                h.registry(),
                h.groups(),
                h.pool(),
                h.app.state(),
            )
            .await
            .0
            .unwrap_err(),
            KeyRotationError::ConfirmationFailed
        );
        // An incomplete answer is wrong too.
        assert_eq!(
            confirm_new_recovery_phrase_core(
                PASSWORD.to_owned(),
                vec![],
                WritersChoice::Ask,
                h.registry(),
                h.groups(),
                h.pool(),
                h.app.state(),
            )
            .await
            .0
            .unwrap_err(),
            KeyRotationError::ConfirmationFailed
        );
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(1));

        let (done, lock) = confirm_new_recovery_phrase_core(
            PASSWORD.to_owned(),
            answers_for(&shown.mnemonic.to_uppercase(), &shown.confirm_positions),
            WritersChoice::Ask,
            h.registry(),
            h.groups(),
            h.pool(),
            h.app.state(),
        )
        .await;
        done.unwrap();
        assert_eq!(lock, Some(LockReason::RecoveryPhraseChanged));
        // The session ended with the change; the password is the same.
        h.assert_nothing_resident(&user_id, &old_key).await;
        h.login(PASSWORD).await.unwrap();
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(2));

        // The password did not change; the old words are dead; the new ones work.
        logout_core(h.registry(), h.groups(), h.pool(), h.app.state())
            .await
            .unwrap();
        epoch_guard::clear_resident_for(&user_id);
        assert_eq!(
            h.restore(&old_words, NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        h.restore(&shown.mnemonic, NEW_PASSWORD).await.unwrap();
        h.login(NEW_PASSWORD).await.unwrap();
        // The slot was emptied by the adoption.
        assert_eq!(
            confirm_new_recovery_phrase_core(
                PASSWORD.to_owned(),
                vec![],
                WritersChoice::Ask,
                h.registry(),
                h.groups(),
                h.pool(),
                h.app.state(),
            )
            .await
            .0
            .unwrap_err(),
            KeyRotationError::NoPendingPhrase
        );
    }

    #[tokio::test]
    async fn a_new_phrase_needs_the_password_and_does_not_outlive_a_lock_or_a_cancel() {
        let h = Harness::new().await;
        h.account().await;
        assert_eq!(
            begin_new_recovery_phrase("wrong".to_owned(), h.registry(), h.pool())
                .await
                .unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(h.failures().await, 1);

        let shown = begin_new_recovery_phrase(PASSWORD.to_owned(), h.registry(), h.pool())
            .await
            .unwrap();
        // The right words but the wrong password at confirm: refused, and the
        // old words and key are untouched.
        assert_eq!(
            confirm_new_recovery_phrase_core(
                "wrong".to_owned(),
                answers_for(&shown.mnemonic, &shown.confirm_positions),
                WritersChoice::Ask,
                h.registry(),
                h.groups(),
                h.pool(),
                h.app.state(),
            )
            .await
            .0
            .unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(1));

        // Cancelling drops the words from memory.
        cancel_new_recovery_phrase(h.registry()).await.unwrap();
        assert!(h.registry().take_pending_new_phrase().await.is_none());

        // So does locking the session.
        begin_new_recovery_phrase(PASSWORD.to_owned(), h.registry(), h.pool())
            .await
            .unwrap();
        h.registry().clear().await;
        assert!(h.registry().take_pending_new_phrase().await.is_none());
    }

    #[tokio::test]
    async fn five_wrong_confirmations_discard_the_shown_phrase() {
        let h = Harness::new().await;
        h.account().await;
        begin_new_recovery_phrase(PASSWORD.to_owned(), h.registry(), h.pool())
            .await
            .unwrap();
        for _ in 0..MAX_FAILED_CONFIRMATIONS {
            assert_eq!(
                confirm_new_recovery_phrase_core(
                    PASSWORD.to_owned(),
                    vec![],
                    WritersChoice::Ask,
                    h.registry(),
                    h.groups(),
                    h.pool(),
                    h.app.state(),
                )
                .await
                .0
                .unwrap_err(),
                KeyRotationError::ConfirmationFailed
            );
        }
        assert_eq!(
            confirm_new_recovery_phrase_core(
                PASSWORD.to_owned(),
                vec![],
                WritersChoice::Ask,
                h.registry(),
                h.groups(),
                h.pool(),
                h.app.state(),
            )
            .await
            .0
            .unwrap_err(),
            KeyRotationError::NoPendingPhrase
        );
    }

    #[test]
    fn random_positions_are_distinct_sorted_and_in_range() {
        for _ in 0..200 {
            let p = random_positions().unwrap();
            assert_eq!(p.len(), CONFIRM_WORDS);
            assert!(p.windows(2).all(|w| w[0] < w[1]));
            assert!(p.iter().all(|n| (1..=24).contains(n)));
        }
    }

    // -- status and events ----------------------------------------------

    #[tokio::test]
    async fn status_reports_idle_running_and_interrupted() {
        let h = Harness::new().await;
        h.account().await;
        let idle = get_rotation_status(h.registry(), h.pool()).await.unwrap();
        assert_eq!(
            idle,
            RotationStatus {
                running: false,
                phase: None,
                percent: 0,
                interrupted: false,
                pending_kind: None,
                last_outcome: LastRotationOutcome::None
            }
        );

        let user_id = h.user_id().await;
        {
            let _flag = epoch_guard::begin_rotation(&user_id).unwrap();
            let running = get_rotation_status(h.registry(), h.pool()).await.unwrap();
            assert!(running.running && !running.interrupted);
        }

        // An interrupted rotation, with the session still held here.
        h.crash_a_password_change(FaultPoint::AfterStage).await;
        let interrupted = get_rotation_status(h.registry(), h.pool()).await.unwrap();
        assert!(!interrupted.running && interrupted.interrupted);
        assert_eq!(interrupted.pending_kind.as_deref(), Some("password_change"));
    }

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<ProgressEvent>>);
    impl ProgressSink for Recorder {
        fn emit(&self, e: &ProgressEvent) {
            self.0.lock().unwrap().push(e.clone());
        }
    }

    #[tokio::test]
    async fn the_global_sink_receives_the_rotation_and_remembers_the_last_event() {
        let h = Harness::new().await;
        h.account().await;
        let recorder = Arc::new(Recorder::default());
        key_rotation::set_global_progress_sink(Some(recorder.clone()));
        key_rotation::clear_last_progress();

        h.change_password(PASSWORD, NEW_PASSWORD).await.unwrap();

        let events = recorder.0.lock().unwrap();
        assert_eq!(events.first().map(|e| e.phase), Some(Phase::Preparing));
        assert_eq!(events.last().map(|e| e.phase), Some(Phase::Complete));
        assert_eq!(
            key_rotation::last_progress().map(|e| e.phase),
            Some(Phase::Complete)
        );
    }

    // -- cancel an interrupted rotation ------------------------------------

    #[derive(Clone, Copy)]
    enum Interrupted {
        Pending,
        InProgress,
        Failed,
        RollbackDirection,
    }

    struct Crashed {
        words: String,
        user_id: String,
        old_dek: [u8; kdf::MASTER_KEY_LEN],
        rotation_id: String,
    }

    /// An account whose password change was interrupted in `case`, after a
    /// restart (locked, nothing resident).
    async fn interrupted_account(h: &Harness, case: Interrupted) -> Crashed {
        let words = h.account().await;
        let user_id = h.user_id().await;
        let old_dek = h.registry().with_key(|k| k.data_key).await.unwrap();
        match case {
            Interrupted::Pending => h.crash_with(vec![FaultPoint::AfterStage]).await,
            Interrupted::InProgress => h.crash_with(vec![FaultPoint::AfterItemCommitted(0)]).await,
            Interrupted::Failed => {
                h.crash_with(vec![FaultPoint::AfterItemCommitted(0)]).await;
                let id = pending_of(h, &user_id).await.rotation_id;
                user_store::abort_rotation(
                    &h.pool(),
                    &id,
                    user_store::RotationState::Failed,
                    Some("test"),
                )
                .await
                .unwrap();
            }
            Interrupted::RollbackDirection => {
                h.crash_with(vec![
                    FaultPoint::FailAfterItemCommitted(0),
                    FaultPoint::RollbackAfterItem(1),
                ])
                .await
            }
        }
        let pending = pending_of(h, &user_id).await;
        match case {
            Interrupted::Pending => {
                assert_eq!(pending.state, user_store::RotationState::Pending)
            }
            Interrupted::InProgress => {
                assert_eq!(pending.state, user_store::RotationState::InProgress)
            }
            Interrupted::Failed => assert_eq!(pending.state, user_store::RotationState::Failed),
            Interrupted::RollbackDirection => {
                assert_eq!(pending.direction, user_store::RotationDirection::Rollback)
            }
        }
        h.restart().await;
        Crashed {
            words,
            user_id,
            old_dek,
            rotation_id: pending.rotation_id,
        }
    }

    async fn pending_of(h: &Harness, user_id: &str) -> user_store::PendingRotation {
        user_store::get_pending_rotation(&h.pool(), user_id)
            .await
            .unwrap()
            .expect("a rotation must be pending")
    }

    /// The account is back on its old key with nothing pending, no failed
    /// attempt was recorded, no session was opened, and the OLD password opens
    /// the OLD data.
    async fn assert_back_on_the_old_key(h: &Harness, c: &Crashed) {
        let state = user_store::get_key_wrapper_state(&h.pool(), &c.user_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.pending_rotation, None);
        assert_eq!(state.wrapper.key_epoch, 1);
        assert!(user_store::get_pending_rotation(&h.pool(), &c.user_id)
            .await
            .unwrap()
            .is_none());
        let row = user_store::get_rotation(&h.pool(), &c.rotation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, user_store::RotationState::RolledBack);
        assert!(!h.registry().is_occupied().await, "cancel signs nothing in");
        assert_eq!(h.failures().await, 0);

        assert_eq!(
            h.login(NEW_PASSWORD).await.unwrap_err(),
            "invalid credentials",
            "the new password is dead after a cancel"
        );
        h.login(PASSWORD).await.unwrap();
        let (dek, epoch) = h
            .registry()
            .with_key(|k| (k.data_key, k.key_epoch))
            .await
            .unwrap();
        assert_eq!(dek, c.old_dek);
        assert_eq!(epoch, 1);
        assert!(
            personal_store::open_personal_db(
                &c.user_id,
                "persona-1",
                &crate::auth::registry::key_hex(&dek)
            )
            .await
            .is_ok(),
            "the old data opens under the old key"
        );
        // And a fresh change can start: nothing is left pending.
        h.change_password(PASSWORD, NEW_PASSWORD).await.unwrap();
    }

    async fn cancel_with_new_password_at(case: Interrupted) {
        let h = Harness::new().await;
        let c = interrupted_account(&h, case).await;
        h.cancel_password("Alice", NEW_PASSWORD).await.unwrap();
        assert_back_on_the_old_key(&h, &c).await;
    }

    #[tokio::test]
    async fn cancel_undoes_a_pending_rotation() {
        cancel_with_new_password_at(Interrupted::Pending).await;
    }

    #[tokio::test]
    async fn cancel_undoes_an_in_progress_rotation() {
        cancel_with_new_password_at(Interrupted::InProgress).await;
    }

    #[tokio::test]
    async fn cancel_undoes_a_failed_rotation() {
        cancel_with_new_password_at(Interrupted::Failed).await;
    }

    #[tokio::test]
    async fn cancel_finishes_a_rollback_that_was_already_under_way() {
        cancel_with_new_password_at(Interrupted::RollbackDirection).await;
    }

    #[tokio::test]
    async fn cancel_works_with_the_recovery_phrase() {
        let h = Harness::new().await;
        let c = interrupted_account(&h, Interrupted::InProgress).await;
        // Messy typing is forgiven, as at restore.
        let messy = format!("  {}  ", c.words.to_uppercase().replace(' ', "   "));
        h.cancel_phrase("Alice", &messy).await.unwrap();
        assert_back_on_the_old_key(&h, &c).await;
    }

    #[tokio::test]
    async fn a_promoted_rotation_cannot_be_cancelled_and_can_still_be_finished() {
        let h = Harness::new().await;
        let words = h.account().await;
        let user_id = h.user_id().await;
        h.crash_a_password_change(FaultPoint::AfterPromote).await;
        let pending = pending_of(&h, &user_id).await;
        assert_eq!(pending.state, user_store::RotationState::Verifying);
        h.restart().await;

        // A wrong credential learns nothing about the state.
        assert_eq!(
            h.cancel_password("Alice", "not it").await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(h.failures().await, 1);

        // The right ones are told it is already complete, and it is not counted.
        assert_eq!(
            h.cancel_password("Alice", NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::RotationAlreadyComplete
        );
        assert_eq!(
            h.cancel_phrase("Alice", &words).await.unwrap_err(),
            KeyRotationError::RotationAlreadyComplete
        );
        assert_eq!(h.failures().await, 1);
        assert!(user_store::get_pending_rotation(&h.pool(), &user_id)
            .await
            .unwrap()
            .is_some());

        // It can only be finished, and the data is on the new key.
        h.login(NEW_PASSWORD).await.unwrap();
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(2));
    }

    #[tokio::test]
    async fn a_wrong_credential_counts_exactly_one_failed_attempt_and_changes_nothing() {
        let h = Harness::new().await;
        let c = interrupted_account(&h, Interrupted::InProgress).await;
        let stranger =
            data_key::mnemonic_from_entropy(&data_key::generate_recovery_entropy().unwrap())
                .unwrap();

        let mut expected = 0;
        for attempt in ["not it", PASSWORD] {
            // The OLD password is just wrong here: login's uncounted
            // "interrupted" answer would make cancel an oracle for it.
            assert_eq!(
                h.cancel_password("Alice", attempt).await.unwrap_err(),
                KeyRotationError::InvalidCredentials
            );
            expected += 1;
            assert_eq!(h.failures().await, expected);
        }
        assert_eq!(
            h.cancel_phrase("Alice", &stranger).await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        expected += 1;
        assert_eq!(h.failures().await, expected);

        // Still pending, still cancellable with the right credential, and the
        // successful cancel clears nothing it should not.
        assert!(user_store::get_pending_rotation(&h.pool(), &c.user_id)
            .await
            .unwrap()
            .is_some());
        h.cancel_password("Alice", NEW_PASSWORD).await.unwrap();
        assert_eq!(h.failures().await, expected);
    }

    #[tokio::test]
    async fn cancelling_a_new_phrase_rotation_needs_the_new_words_or_the_password() {
        let h = Harness::new().await;
        let old_words = h.account().await;
        let user_id = h.user_id().await;
        let new_entropy = data_key::generate_recovery_entropy().unwrap();
        let new_words = data_key::mnemonic_from_entropy(&new_entropy)
            .unwrap()
            .to_string();
        let err = h
            .run_with(
                vec![FaultPoint::AfterItemCommitted(0)],
                RotationRequest::NewPhrase {
                    password: Zeroizing::new(PASSWORD.as_bytes().to_vec()),
                    recovery_entropy: new_entropy,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, RotationError::SimulatedCrash), "{err:?}");
        h.restart().await;

        // The OLD words open nothing of the staged key.
        assert_eq!(
            h.cancel_phrase("Alice", &old_words).await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(h.failures().await, 1);
        h.cancel_phrase("Alice", &new_words).await.unwrap();
        assert!(user_store::get_pending_rotation(&h.pool(), &user_id)
            .await
            .unwrap()
            .is_none());
        // The old words still work: nothing was revoked.
        h.restore(&old_words, NEW_PASSWORD).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_account_nothing_pending_and_wrong_credential_look_the_same() {
        let h = Harness::new().await;
        let words = h.account().await;
        let user_id = h.user_id().await;

        // Nothing pending: answered as a wrong credential and counted as one.
        assert_eq!(
            h.cancel_password("Alice", NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(h.failures().await, 1);
        assert_eq!(
            h.cancel_phrase("Alice", &words).await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(h.failures().await, 2);

        // Unknown account: the same answer, no account to count against.
        assert_eq!(
            h.cancel_password("Nobody", NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(
            h.cancel_phrase("Nobody", &words).await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(h.failures().await, 2);

        // And the same answer an account WITH a pending rotation gives a wrong
        // credential.
        h.crash_a_password_change(FaultPoint::AfterItemCommitted(0))
            .await;
        h.restart().await;
        assert_eq!(
            h.cancel_password("Alice", "not it").await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        assert_eq!(h.failures().await, 3);

        // Words that are not a phrase say so and say nothing about any account.
        for name in ["Alice", "Nobody"] {
            assert_eq!(
                h.cancel_phrase(name, "one two three").await.unwrap_err(),
                KeyRotationError::PhraseInvalid
            );
        }
        assert_eq!(h.failures().await, 3);
        assert!(user_store::get_pending_rotation(&h.pool(), &user_id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn cancel_refuses_while_a_rotation_is_running_and_touches_nothing() {
        let h = Harness::new().await;
        let c = interrupted_account(&h, Interrupted::InProgress).await;
        {
            let _flag = epoch_guard::begin_rotation(&c.user_id).unwrap();
            assert_eq!(
                h.cancel_password("Alice", NEW_PASSWORD).await.unwrap_err(),
                KeyRotationError::RotationInProgress
            );
        }
        assert_eq!(h.failures().await, 0);
        assert!(user_store::get_pending_rotation(&h.pool(), &c.user_id)
            .await
            .unwrap()
            .is_some());
        h.cancel_password("Alice", NEW_PASSWORD).await.unwrap();
        assert_back_on_the_old_key(&h, &c).await;
    }

    // -- last outcome -------------------------------------------------------

    fn event(phase: key_rotation::Phase) -> ProgressEvent {
        ProgressEvent {
            rotation_id: "r".to_owned(),
            phase,
            current: 1,
            total: 2,
            message: None,
        }
    }

    #[test]
    fn only_a_final_report_is_an_outcome_and_never_while_running() {
        use key_rotation::Phase;
        assert_eq!(last_outcome_of(false, None), LastRotationOutcome::None);
        for (phase, want) in [
            (Phase::Complete, LastRotationOutcome::Complete),
            (Phase::RolledBack, LastRotationOutcome::RolledBack),
            (Phase::Failed, LastRotationOutcome::Failed),
        ] {
            assert_eq!(last_outcome_of(false, Some(&event(phase))), want);
            assert_eq!(
                last_outcome_of(true, Some(&event(phase))),
                LastRotationOutcome::None,
                "the event may belong to the previous rotation"
            );
        }
        for phase in [
            Phase::Preparing,
            Phase::Rekeying,
            Phase::Verifying,
            Phase::Finishing,
            Phase::Resuming,
            Phase::RollingBack,
        ] {
            assert_eq!(
                last_outcome_of(false, Some(&event(phase))),
                LastRotationOutcome::None
            );
        }
        assert_eq!(
            serde_json::to_string(&LastRotationOutcome::RolledBack).unwrap(),
            "\"rolled_back\""
        );
        assert_eq!(
            serde_json::to_string(&LastRotationOutcome::None).unwrap(),
            "\"none\""
        );
    }

    #[tokio::test]
    async fn last_outcome_is_none_in_a_process_that_ran_no_rotation() {
        let h = Harness::new().await;
        key_rotation::clear_last_progress();
        h.account().await;
        assert_eq!(h.status().await.last_outcome, LastRotationOutcome::None);
    }

    #[tokio::test]
    async fn last_outcome_complete_is_not_sticky_into_the_next_rotation() {
        let h = Harness::new().await;
        h.account().await;
        key_rotation::clear_last_progress();

        h.change_password(PASSWORD, NEW_PASSWORD).await.unwrap();
        assert_eq!(h.status().await.last_outcome, LastRotationOutcome::Complete);
        // The change ended the session; sign in again for the next rotation.
        h.login(NEW_PASSWORD).await.unwrap();

        // While a rotation holds the flag the previous outcome is not reported.
        let user_id = h.user_id().await;
        {
            let _flag = epoch_guard::begin_rotation(&user_id).unwrap();
            let s = h.status().await;
            assert!(s.running);
            assert_eq!(s.last_outcome, LastRotationOutcome::None);
        }

        // A new rotation that starts and is interrupted replaces it with
        // nothing: a crash is not an outcome.
        h.crash_with(vec![FaultPoint::AfterItemCommitted(0)]).await;
        let s = h.status().await;
        assert!(!s.running && s.interrupted);
        assert_eq!(s.last_outcome, LastRotationOutcome::None);
    }

    #[tokio::test]
    async fn last_outcome_rolled_back_after_an_automatic_rollback_and_after_a_cancel() {
        let h = Harness::new().await;
        h.account().await;
        key_rotation::clear_last_progress();

        let err = h
            .run_password_change_with(vec![FaultPoint::FailAfterItemCommitted(0)], NEW_PASSWORD)
            .await
            .unwrap_err();
        assert!(matches!(err, RotationError::RolledBack { .. }), "{err:?}");
        assert_eq!(
            h.status().await.last_outcome,
            LastRotationOutcome::RolledBack
        );

        // A cancel reports the same way.
        key_rotation::clear_last_progress();
        h.crash_with(vec![FaultPoint::AfterItemCommitted(0)]).await;
        h.restart().await;
        assert_eq!(h.status().await.last_outcome, LastRotationOutcome::None);
        h.cancel_password("Alice", NEW_PASSWORD).await.unwrap();
        assert_eq!(
            h.status().await.last_outcome,
            LastRotationOutcome::RolledBack
        );
    }

    #[tokio::test]
    async fn last_outcome_failed_when_a_promoted_rotation_could_not_finish() {
        let h = Harness::new().await;
        h.account().await;
        key_rotation::clear_last_progress();

        let err = h
            .run_password_change_with(vec![FaultPoint::FailAfterPromote], NEW_PASSWORD)
            .await
            .unwrap_err();
        assert!(matches!(err, RotationError::NeedsResume { .. }), "{err:?}");
        let s = h.status().await;
        assert_eq!(s.last_outcome, LastRotationOutcome::Failed);
        assert!(s.interrupted);
    }

    // -- errors carry nothing identifying -----------------------------------

    #[test]
    fn every_error_code_is_free_of_paths_ids_and_secrets() {
        let identifying = "personas/secret-persona-id/personal.db";
        let errors = [
            RotationError::NoWrapper,
            RotationError::RotationInProgress,
            RotationError::EpochChanged,
            RotationError::ActiveRuns(4),
            RotationError::UnknownEncryptedFile(identifying.into()),
            RotationError::NoProofDatabase,
            RotationError::DatabaseUnreadable {
                rel_path: identifying.into(),
                detail: identifying.into(),
            },
            RotationError::InsufficientSpace {
                needed: 5 * 1024 * 1024,
                available: 1024,
            },
            RotationError::FreeSpaceUnknown,
            RotationError::Busy {
                rel_path: identifying.into(),
            },
            RotationError::Item {
                rel_path: identifying.into(),
                detail: identifying.into(),
            },
            RotationError::ReadBackFailed(identifying.into()),
            RotationError::CannotResume(identifying.into()),
            RotationError::WrongPhrase,
            RotationError::AlreadyPromoted,
            RotationError::RolledBack {
                cause: identifying.into(),
            },
            RotationError::NeedsResume {
                cause: identifying.into(),
            },
            RotationError::Epoch(identifying.into()),
        ];
        for e in &errors {
            let json = serde_json::to_string(&code_for(e)).unwrap();
            for needle in ["secret-persona-id", ".db", "personas/", "persona"] {
                assert!(!json.contains(needle), "{e:?} -> {json}");
            }
            assert!(json.starts_with("{\"code\":\""), "{json}");
        }
        assert_eq!(
            code_for(&RotationError::ActiveRuns(4)),
            KeyRotationError::ActiveRuns { count: 4 }
        );
        assert_eq!(
            code_for(&RotationError::InsufficientSpace {
                needed: 5 * 1024 * 1024 + 1,
                available: 3 * 1024 * 1024
            }),
            KeyRotationError::InsufficientSpace {
                needed_mb: 6,
                available_mb: 3
            }
        );
        assert_eq!(
            code_for(&RotationError::AlreadyPromoted),
            KeyRotationError::RotationAlreadyComplete
        );
        assert_eq!(
            serde_json::to_string(&KeyRotationError::RotationAlreadyComplete).unwrap(),
            "{\"code\":\"rotation_already_complete\"}"
        );
        // A recovery phrase never appears in any serialized code either.
        let all = serde_json::to_string(&[
            KeyRotationError::NotLoggedIn,
            KeyRotationError::PhraseInvalid,
            KeyRotationError::ConfirmationFailed,
            KeyRotationError::NoPendingPhrase,
            KeyRotationError::RotationAlreadyComplete,
        ])
        .unwrap();
        assert!(!all.contains("mnemonic"));
    }
}

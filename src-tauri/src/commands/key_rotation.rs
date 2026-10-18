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
//   get_rotation_status()
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
use crate::auth::kdf;
use crate::auth::key_rotation::{
    self, Credential, ProgressEvent, ProgressSink, ResumeOutcome, RotationEnv, RotationError,
    RotationRequest,
};
use crate::auth::registry::{GroupKeyRegistry, KeyRegistry, NewPhraseSlot};
use crate::auth::user_store;
use crate::commands::auth::{finish_login, is_locked_out, record_failed_attempt};
use crate::conductor::concurrency::ConductorScheduler;
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
    /// The typed words are not a valid 24-word recovery phrase.
    PhraseInvalid,
    /// A rotation is already running, or one is pending that must finish first.
    RotationInProgress,
    /// Tasks are still running; wait for them or cancel them.
    ActiveRuns {
        count: u32,
    },
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

/// One re-entered word.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct WordAnswer {
    pub position: u32,
    pub word: String,
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
        _ => KeyRotationError::Internal,
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
    scheduler: Option<&'a ConductorScheduler>,
    registry: Option<&'a KeyRegistry>,
) -> RotationEnv<'a> {
    let mut env = RotationEnv::new(pool, user_id, user_dir(user_id));
    env.scheduler = scheduler;
    env.registry = registry;
    env
}

// ---------------------------------------------------------------------------
// change_password
// ---------------------------------------------------------------------------

/// Change the account password. Re-verifies `old_password`, then rotates the
/// data key: everything is re-locked under a new key and the new password.
/// The recovery phrase is unchanged and stays valid. Returns when the change is
/// complete (or has failed); progress arrives as `task-progress` events.
#[tauri::command]
#[specta::specta]
pub async fn change_password(
    old_password: String,
    new_password: String,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    scheduler: State<'_, Arc<ConductorScheduler>>,
) -> Result<(), KeyRotationError> {
    let old_password = Zeroizing::new(old_password);
    let new_password = Zeroizing::new(new_password);
    if new_password.is_empty() {
        return Err(KeyRotationError::NewPasswordEmpty);
    }
    let (resident, _) = authorise_session_change(&key_registry, &pool, &old_password).await?;

    let env = env_for(
        &pool,
        &resident.user_id,
        Some(&scheduler),
        Some(&key_registry),
    );
    key_rotation::run_rotation(
        &env,
        &resident.data_key,
        resident.key_epoch,
        RotationRequest::PasswordChange {
            new_password: Zeroizing::new(new_password.as_bytes().to_vec()),
        },
    )
    .await
    .map(drop)
    .map_err(|e| code_for(&e))
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
/// for the password again (it is not held in memory in between).
#[tauri::command]
#[specta::specta]
pub async fn confirm_new_recovery_phrase(
    password: String,
    answers: Vec<WordAnswer>,
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    scheduler: State<'_, Arc<ConductorScheduler>>,
) -> Result<(), KeyRotationError> {
    let password = Zeroizing::new(password);
    let resident = resident(&key_registry).await?;

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

    authorise_session_change(&key_registry, &pool, &password).await?;
    let entropy = key_registry
        .with_pending_new_phrase(|slot| slot.entropy.clone())
        .await
        .ok_or(KeyRotationError::NoPendingPhrase)?;

    let env = env_for(
        &pool,
        &resident.user_id,
        Some(&scheduler),
        Some(&key_registry),
    );
    key_rotation::run_rotation(
        &env,
        &resident.data_key,
        resident.key_epoch,
        RotationRequest::NewPhrase {
            password: Zeroizing::new(password.as_bytes().to_vec()),
            recovery_entropy: entropy,
        },
    )
    .await
    .map_err(|e| code_for(&e))?;
    // Adopted: the held words have done their job.
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

/// Forgot the password: sign in with the 24 recovery words and set a new
/// password. The words unseal the data key, the data key is rotated exactly as
/// for a password change, and the account is unlocked. The words themselves do
/// not change. If a change was interrupted earlier, this is also the way to
/// finish it with the words.
#[tauri::command]
#[specta::specta]
pub async fn restore_with_recovery_phrase(
    display_name: String,
    phrase: String,
    new_password: String,
    key_registry: State<'_, KeyRegistry>,
    group_key_registry: State<'_, GroupKeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<(), KeyRotationError> {
    let new_password = Zeroizing::new(new_password);
    if new_password.is_empty() {
        return Err(KeyRotationError::NewPasswordEmpty);
    }
    // The words' shape is checked first and says nothing about any account,
    // so an unknown account cannot be told apart by it.
    let entropy = data_key::entropy_from_mnemonic(&normalise_phrase(&phrase))
        .map_err(|_| KeyRotationError::PhraseInvalid)?;
    // Unknown account and wrong words must look the same.
    let user = user_store::find_user_by_display_name(&pool, &display_name)
        .await
        .map_err(|e| internal("user", e))?
        .ok_or(KeyRotationError::InvalidCredentials)?;
    if is_locked_out(&pool, &display_name)
        .await
        .map_err(|e| internal("lockout check", e))?
    {
        return Err(KeyRotationError::AccountLocked);
    }
    let state = user_store::get_key_wrapper_state(&pool, &user.id)
        .await
        .map_err(|e| internal("wrapper", e))?
        .ok_or_else(|| internal("wrapper", "missing"))?;

    let wrong = || async {
        record_failed_attempt(&pool, &display_name)
            .await
            .map_err(|e| internal("failure bookkeeping", e))?;
        Err::<(), _>(KeyRotationError::InvalidCredentials)
    };

    // An interrupted change: finish it with the words.
    if state.pending_rotation.is_some() {
        let pending = user_store::get_pending_rotation(&pool, &user.id)
            .await
            .map_err(|e| internal("pending rotation", e))?
            .ok_or_else(|| internal("pending rotation", "flag without a row"))?;
        let new_dek = match key_rotation::new_dek_from_staged_phrase(&pending, &user.id, &entropy) {
            Ok(d) => d,
            Err(RotationError::WrongPhrase) | Err(RotationError::Crypto(_)) => {
                return wrong().await.map(|_| ());
            }
            Err(e) => return Err(code_for(&e)),
        };
        let env = env_for(&pool, &user.id, None, None);
        let finished = match key_rotation::resume_rotation(
            &env,
            &pending,
            new_dek,
            Credential::Phrase(&entropy),
        )
        .await
        {
            Ok(ResumeOutcome::Completed(outcome)) => outcome,
            Ok(ResumeOutcome::RolledBack) => return Err(KeyRotationError::RolledBack),
            Err(e) => return Err(code_for(&e)),
        };
        // The interrupted change is done, on whatever password it had chosen.
        // The person here forgot their password and typed a new one: run the
        // restore on top so the account ENDS on the typed password. (If this
        // second step fails the account stays on the finished change's
        // password and the failure is reported; no session is opened.)
        let outcome = key_rotation::run_rotation(
            &env,
            &finished.new_dek,
            finished.to_epoch,
            RotationRequest::Restore {
                new_password: Zeroizing::new(new_password.as_bytes().to_vec()),
            },
        )
        .await
        .map_err(|e| code_for(&e))?;
        return finish_login(
            &user.id,
            *outcome.new_dek,
            outcome.to_epoch,
            &key_registry,
            &group_key_registry,
            &pool,
        )
        .await
        .map_err(|e| internal("session", e));
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

    let env = env_for(&pool, &user.id, None, None);
    let outcome = match key_rotation::run_rotation(
        &env,
        &old_dek,
        state.wrapper.key_epoch,
        RotationRequest::Restore {
            new_password: Zeroizing::new(new_password.as_bytes().to_vec()),
        },
    )
    .await
    {
        Ok(o) => o,
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
            return wrong().await.map(|_| ());
        }
        Err(e) => return Err(code_for(&e)),
    };
    finish_login(
        &user.id,
        *outcome.new_dek,
        outcome.to_epoch,
        &key_registry,
        &group_key_registry,
        &pool,
    )
    .await
    .map_err(|e| internal("session", e))
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Where a rotation stands. Safe to poll; reads no secret. Without a session
/// there is nothing to report beyond whether this process is rotating.
#[tauri::command]
#[specta::specta]
pub async fn get_rotation_status(
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<RotationStatus, KeyRotationError> {
    let running = epoch_guard::is_rotating();
    let (phase, percent) = match key_rotation::last_progress().filter(|_| running) {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::Manager;

    use crate::auth::key_rotation::{FaultPoint, Phase};
    use crate::commands::auth::{
        get_recovery_key_display, login, logout, LOGIN_ROTATION_INTERRUPTED,
    };
    use crate::persistence::personal_store;
    use crate::test_support::ENV_MUTEX;

    const PASSWORD: &str = "password123";
    const NEW_PASSWORD: &str = "a different password";

    struct Harness {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        _tempdir: tempfile::TempDir,
        saved_root: Option<String>,
        app: tauri::App<tauri::test::MockRuntime>,
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
            app.manage(Arc::new(ConductorScheduler::new()));
            app.manage(pool);
            Harness {
                _lock: lock,
                _tempdir: tempdir,
                saved_root,
                app,
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
        fn scheduler(&self) -> State<'_, Arc<ConductorScheduler>> {
            self.app.state::<Arc<ConductorScheduler>>()
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
            change_password(
                old.to_owned(),
                new.to_owned(),
                self.registry(),
                self.pool(),
                self.scheduler(),
            )
            .await
        }

        async fn restore(&self, words: &str, new: &str) -> Result<(), KeyRotationError> {
            restore_with_recovery_phrase(
                "Alice".to_owned(),
                words.to_owned(),
                new.to_owned(),
                self.registry(),
                self.groups(),
                self.pool(),
            )
            .await
        }

        /// Leave an interrupted rotation behind, as a crash would, and lock
        /// the process (a restart).
        async fn crash_a_password_change(&self, fault: FaultPoint) {
            let (user_id, dek, epoch) = self
                .registry()
                .with_key(|k| (k.user_id.clone(), k.data_key, k.key_epoch))
                .await
                .unwrap();
            let pool = self.pool();
            let mut env = RotationEnv::new(&pool, &user_id, user_dir(&user_id));
            env.faults = vec![fault];
            let err = key_rotation::run_rotation(
                &env,
                &dek,
                epoch,
                RotationRequest::PasswordChange {
                    new_password: Zeroizing::new(NEW_PASSWORD.as_bytes().to_vec()),
                },
            )
            .await
            .unwrap_err();
            assert!(matches!(err, RotationError::SimulatedCrash), "{err:?}");
        }

        async fn restart(&self) {
            let user_id = self.user_id().await;
            self.registry().clear().await;
            epoch_guard::clear_resident_for(&user_id);
        }
    }

    // -- change_password ----------------------------------------------------

    #[tokio::test]
    async fn change_password_rotates_and_keeps_the_session_working() {
        let h = Harness::new().await;
        h.account().await;
        let user_id = h.user_id().await;
        let before = h
            .registry()
            .with_key(|k| (k.data_key, k.key_epoch))
            .await
            .unwrap();

        h.change_password(PASSWORD, NEW_PASSWORD).await.unwrap();

        // The session now holds a different key at the next epoch, and works.
        let after = h
            .registry()
            .with_key(|k| (k.data_key, k.key_epoch))
            .await
            .unwrap();
        assert_ne!(after.0, before.0);
        assert_eq!(after.1, before.1 + 1);
        let new_hex = crate::auth::registry::key_hex(&after.0);
        assert!(
            personal_store::open_personal_db(&user_id, "persona-1", &new_hex)
                .await
                .is_ok()
        );

        // Old password is dead, new password signs in.
        h.registry().clear().await;
        epoch_guard::clear_resident_for(&user_id);
        assert_eq!(h.login(PASSWORD).await.unwrap_err(), "invalid credentials");
        h.login(NEW_PASSWORD).await.unwrap();
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

    #[tokio::test]
    async fn a_running_task_blocks_change_password_with_a_count() {
        let h = Harness::new().await;
        h.account().await;
        assert!(
            h.scheduler()
                .acquire_run_slot(
                    "run-1",
                    crate::conductor::concurrency::PathPriority::Interactive
                )
                .await
        );
        assert_eq!(
            h.change_password(PASSWORD, NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::ActiveRuns { count: 1 }
        );
        h.scheduler().release_run_slot("run-1");
        h.change_password(PASSWORD, NEW_PASSWORD).await.unwrap();
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
        assert!(h.registry().is_occupied().await);
        assert!(user_store::get_pending_rotation(&h.pool(), &user_id)
            .await
            .unwrap()
            .is_none());
        // Two rotations ran: the interrupted change (epoch 2) and then the
        // restore on top (epoch 3), so the account ENDS on the typed password.
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(3));
        h.registry().clear().await;
        epoch_guard::clear_resident_for(&user_id);
        assert_eq!(h.login(PASSWORD).await.unwrap_err(), "invalid credentials");
        assert_eq!(
            h.login(NEW_PASSWORD).await.unwrap_err(),
            "invalid credentials",
            "the interrupted change's password was superseded"
        );
        h.login("the one typed at restore").await.unwrap();
    }

    // -- restore --------------------------------------------------------

    #[tokio::test]
    async fn restore_with_the_words_sets_a_new_password_and_signs_in() {
        let h = Harness::new().await;
        let words = h.account().await;
        let user_id = h.user_id().await;
        logout(h.registry(), h.groups(), h.pool(), h.app.state())
            .await
            .unwrap();
        epoch_guard::clear_resident_for(&user_id);

        h.restore(&words, NEW_PASSWORD).await.unwrap();
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
        logout(h.registry(), h.groups(), h.pool(), h.app.state())
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
        logout(h.registry(), h.groups(), h.pool(), h.app.state())
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
            restore_with_recovery_phrase(
                "Nobody".to_owned(),
                words.clone(),
                NEW_PASSWORD.to_owned(),
                h.registry(),
                h.groups(),
                h.pool(),
            )
            .await
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
        logout(h.registry(), h.groups(), h.pool(), h.app.state())
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
        logout(h.registry(), h.groups(), h.pool(), h.app.state())
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
            confirm_new_recovery_phrase(
                PASSWORD.to_owned(),
                wrong,
                h.registry(),
                h.pool(),
                h.scheduler()
            )
            .await
            .unwrap_err(),
            KeyRotationError::ConfirmationFailed
        );
        // An incomplete answer is wrong too.
        assert_eq!(
            confirm_new_recovery_phrase(
                PASSWORD.to_owned(),
                vec![],
                h.registry(),
                h.pool(),
                h.scheduler()
            )
            .await
            .unwrap_err(),
            KeyRotationError::ConfirmationFailed
        );
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(1));

        confirm_new_recovery_phrase(
            PASSWORD.to_owned(),
            answers_for(&shown.mnemonic.to_uppercase(), &shown.confirm_positions),
            h.registry(),
            h.pool(),
            h.scheduler(),
        )
        .await
        .unwrap();
        assert_eq!(h.registry().with_key(|k| k.key_epoch).await, Some(2));

        // The password did not change; the old words are dead; the new ones work.
        logout(h.registry(), h.groups(), h.pool(), h.app.state())
            .await
            .unwrap();
        epoch_guard::clear_resident_for(&user_id);
        assert_eq!(
            h.restore(&old_words, NEW_PASSWORD).await.unwrap_err(),
            KeyRotationError::InvalidCredentials
        );
        h.restore(&shown.mnemonic, NEW_PASSWORD).await.unwrap();
        // The slot was emptied by the adoption.
        assert_eq!(
            confirm_new_recovery_phrase(
                PASSWORD.to_owned(),
                vec![],
                h.registry(),
                h.pool(),
                h.scheduler()
            )
            .await
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
            confirm_new_recovery_phrase(
                "wrong".to_owned(),
                answers_for(&shown.mnemonic, &shown.confirm_positions),
                h.registry(),
                h.pool(),
                h.scheduler()
            )
            .await
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
                confirm_new_recovery_phrase(
                    PASSWORD.to_owned(),
                    vec![],
                    h.registry(),
                    h.pool(),
                    h.scheduler()
                )
                .await
                .unwrap_err(),
                KeyRotationError::ConfirmationFailed
            );
        }
        assert_eq!(
            confirm_new_recovery_phrase(
                PASSWORD.to_owned(),
                vec![],
                h.registry(),
                h.pool(),
                h.scheduler()
            )
            .await
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
                pending_kind: None
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
        // A recovery phrase never appears in any serialized code either.
        let all = serde_json::to_string(&[
            KeyRotationError::NotLoggedIn,
            KeyRotationError::PhraseInvalid,
            KeyRotationError::ConfirmationFailed,
            KeyRotationError::NoPendingPhrase,
        ])
        .unwrap();
        assert!(!all.contains("mnemonic"));
    }
}

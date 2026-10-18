// src-tauri/src/auth/epoch_guard.rs
//
// Key-epoch check on store open (items.id=629, decisions.id=893, AUTH
// Section 4.0 "Crash safety and sessions"). The key epoch is a counter in
// shared.db (user_key_wrappers.key_epoch) that increases at every rotation
// (items.id=630). Another OS session of the same account may rotate while
// this process still holds the old data key, so every store open compares
// the epoch THIS session unlocked under against the stored one, and a
// mismatch is refused (and, via auth::idle_timeout, locks the session).
//
// WHERE THE RESIDENT EPOCH LIVES: the stores take a bare `(user_id,
// key_hex)` and never see the KeyRegistry, so the epoch this process holds
// is mirrored here as a process-global (user_id, epoch), set by
// commands::auth::finish_login -- the only production path that installs a
// key -- and cleared by KeyRegistry::clear(). With nothing registered (no
// real login in this process, as in unit tests that install a key through a
// test helper) the guard has nothing to compare and passes. A source-scan
// test (tests below) asserts every opener that applies a SQLCipher key
// either calls this guard or is on an explicit exempt list.
//
// COST: one short read-only connection to the unencrypted shared.db per
// store open. shared.db is never created here: an unreadable shared.db
// while a key is registered fails closed.

use std::sync::Mutex;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::ConnectOptions;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KeyEpochError {
    /// The stored epoch differs from the one this session unlocked under:
    /// the account's key was rotated elsewhere.
    #[error("this account's key was changed in another session, unlock again")]
    Changed { held: u64, current: Option<u64> },
    #[error("could not check the account key epoch: {0}")]
    Unreadable(String),
    /// The account's data key is being rotated by this process
    /// (auth::key_rotation). Stores refuse to open until it finishes.
    #[error("the account's data key is being changed, try again in a moment")]
    Rotating,
}

static RESIDENT: Mutex<Option<(String, u64)>> = Mutex::new(None);

/// The user whose data key this process is rotating right now, if any
/// (items.id=630). While set, every store open for that user is refused and
/// the periodic sweeps stand down, so nothing touches a file the rotation is
/// re-keying and no scheduled work runs mid-rotation (decisions.id=847).
static ROTATING: Mutex<Option<String>> = Mutex::new(None);

fn rotating_lock() -> std::sync::MutexGuard<'static, Option<String>> {
    ROTATING.lock().unwrap_or_else(|p| p.into_inner())
}

/// Held for the duration of a rotation. Dropping it (including on a panic or
/// an early return) clears the flag.
#[must_use = "the rotating flag is cleared when this guard is dropped"]
#[derive(Debug)]
pub struct RotationGuard {
    user_id: String,
}

impl Drop for RotationGuard {
    fn drop(&mut self) {
        let mut flag = rotating_lock();
        if flag.as_deref() == Some(self.user_id.as_str()) {
            *flag = None;
        }
    }
}

/// Mark `user_id` as being rotated. Only one rotation runs per process: a
/// second call while one is active returns `Rotating`.
pub fn begin_rotation(user_id: &str) -> Result<RotationGuard, KeyEpochError> {
    let mut flag = rotating_lock();
    if flag.is_some() {
        return Err(KeyEpochError::Rotating);
    }
    *flag = Some(user_id.to_owned());
    Ok(RotationGuard {
        user_id: user_id.to_owned(),
    })
}

/// Whether this process is rotating any account's key right now. The
/// periodic sweeps check this and skip the tick.
pub fn is_rotating() -> bool {
    rotating_lock().is_some()
}

/// Whether this process is rotating THIS user's key right now.
pub fn is_rotating_for(user_id: &str) -> bool {
    rotating_lock().as_deref() == Some(user_id)
}

fn lock() -> std::sync::MutexGuard<'static, Option<(String, u64)>> {
    RESIDENT.lock().unwrap_or_else(|p| p.into_inner())
}

/// Record the (user, epoch) this process just unlocked under.
pub fn set_resident(user_id: &str, key_epoch: u64) {
    *lock() = Some((user_id.to_owned(), key_epoch));
}

/// Forget the registered epoch, but only if it belongs to `user_id` (so one
/// registry's clear can never wipe another account's registration).
pub fn clear_resident_for(user_id: &str) {
    let mut guard = lock();
    if guard.as_ref().is_some_and(|(u, _)| u == user_id) {
        *guard = None;
    }
}

/// The epoch registered for `user_id`, if this process unlocked it.
pub fn resident_epoch(user_id: &str) -> Option<u64> {
    lock()
        .as_ref()
        .filter(|(u, _)| u == user_id)
        .map(|(_, e)| *e)
}

fn compare(held: u64, current: Option<u64>) -> Result<(), KeyEpochError> {
    if current == Some(held) {
        Ok(())
    } else {
        Err(KeyEpochError::Changed { held, current })
    }
}

/// Check against shared.db via a pool the caller already has.
pub async fn ensure_current_with_pool(
    pool: &sqlx::SqlitePool,
    user_id: &str,
) -> Result<(), KeyEpochError> {
    if is_rotating_for(user_id) {
        return Err(KeyEpochError::Rotating);
    }
    let Some(held) = resident_epoch(user_id) else {
        return Ok(());
    };
    let current = crate::auth::user_store::get_key_epoch(pool, user_id)
        .await
        .map_err(|e| KeyEpochError::Unreadable(e.to_string()))?;
    compare(held, current)
}

/// Check against shared.db on a short read-only connection. Call first in
/// every store opener that takes a `user_id`.
pub async fn ensure_current(user_id: &str) -> Result<(), KeyEpochError> {
    if is_rotating_for(user_id) {
        return Err(KeyEpochError::Rotating);
    }
    let Some(held) = resident_epoch(user_id) else {
        return Ok(());
    };
    let mut conn = SqliteConnectOptions::new()
        .filename(crate::providers::utils::db_path_shared())
        .create_if_missing(false)
        .read_only(true)
        .connect()
        .await
        .map_err(|e| KeyEpochError::Unreadable(e.to_string()))?;
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT key_epoch FROM user_key_wrappers WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&mut conn)
            .await
            .map_err(|e| KeyEpochError::Unreadable(e.to_string()))?;
    let current = row.and_then(|(e,)| u64::try_from(e).ok());
    compare(held, current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_is_per_user_and_clears_only_for_that_user() {
        set_resident("guard-user-a", 3);
        assert_eq!(resident_epoch("guard-user-a"), Some(3));
        assert_eq!(resident_epoch("guard-user-b"), None);
        clear_resident_for("guard-user-b");
        assert_eq!(resident_epoch("guard-user-a"), Some(3));
        clear_resident_for("guard-user-a");
        assert_eq!(resident_epoch("guard-user-a"), None);
    }

    // These two touch the process-global rotating flag, which the rotation
    // engine's tests also use: serialize them on the shared test mutex.
    #[tokio::test]
    async fn the_rotating_flag_is_per_process_exclusive_and_clears_on_drop() {
        let _serial = crate::test_support::ENV_MUTEX.lock().await;
        let guard = begin_rotation("rot-flag-a").unwrap();
        assert!(is_rotating() && is_rotating_for("rot-flag-a"));
        assert!(!is_rotating_for("rot-flag-b"));
        assert!(matches!(
            begin_rotation("rot-flag-b"),
            Err(KeyEpochError::Rotating)
        ));
        drop(guard);
        assert!(!is_rotating());
        // Free again.
        drop(begin_rotation("rot-flag-b").unwrap());
        assert!(!is_rotating());
    }

    #[tokio::test]
    async fn a_rotating_user_is_refused_before_anything_is_read() {
        let _serial = crate::test_support::ENV_MUTEX.lock().await;
        // No shared.db exists for these paths: the answer must not need one.
        let _guard = begin_rotation("rot-flag-refused").unwrap();
        assert!(matches!(
            ensure_current("rot-flag-refused").await,
            Err(KeyEpochError::Rotating)
        ));
        // Other users are unaffected.
        assert!(ensure_current("rot-flag-other").await.is_ok());
    }

    #[test]
    fn compare_passes_only_on_an_exact_match() {
        assert!(compare(2, Some(2)).is_ok());
        assert!(matches!(
            compare(2, Some(3)),
            Err(KeyEpochError::Changed {
                held: 2,
                current: Some(3)
            })
        ));
        assert!(matches!(
            compare(2, None),
            Err(KeyEpochError::Changed {
                held: 2,
                current: None
            })
        ));
    }

    /// Every source file that applies an account-scoped SQLCipher key must
    /// either call the guard or be on this list with its reason. Coarse
    /// (file-level) on purpose: its job is to catch a NEW opener file added
    /// without the guard.
    const EXEMPT: &[(&str, &str)] = &[
        (
            "providers/utils.rs",
            "defines connect_options_encrypted, opens nothing",
        ),
        (
            "persistence/migrations.rs",
            "create/migrate helpers, run at account creation and unlock",
        ),
        (
            "persistence/group_store.rs",
            "group.db is keyed by the group key (items.id=658)",
        ),
        (
            "group_sync/engine.rs",
            "group.db is keyed by the group key (items.id=658)",
        ),
        (
            "conductor/executor.rs",
            "key applied only inside #[cfg(test)] code",
        ),
        (
            "commands/auth.rs",
            "key applied only inside #[cfg(test)] code",
        ),
        (
            "auth/key_rotation_io.rs",
            "rotation primitives: the engine owns the account and checks the epoch itself",
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

    #[test]
    fn every_account_keyed_opener_file_calls_the_epoch_guard() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rs_files(&src, &mut files);
        let mut missing = Vec::new();
        for path in files {
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap();
            let applies_key =
                text.contains("pragma(\"key\"") || text.contains("connect_options_encrypted(");
            if !applies_key || EXEMPT.iter().any(|(f, _)| *f == rel) {
                continue;
            }
            if !text.contains("epoch_guard::ensure_current(") {
                missing.push(rel);
            }
        }
        assert!(
            missing.is_empty(),
            "these files apply a SQLCipher key but never call epoch_guard::ensure_current \
             (add the guard to the opener, or list the file in EXEMPT with a reason): {missing:?}"
        );
    }

    #[tokio::test]
    async fn nothing_registered_means_nothing_to_compare() {
        // No shared.db exists for this path either: the guard must return
        // before touching it.
        assert!(ensure_current("guard-never-registered").await.is_ok());
    }
}

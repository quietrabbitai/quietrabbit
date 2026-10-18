// src-tauri/src/commands/auth.rs
//
// Group 11 — Auth. Commands: login, logout, get_recovery_key_display.
//
// login/logout wired items.id=205 (2026-08-01) against the auth foundation
// (auth::kdf, auth::registry, auth::user_store) built earlier this session.
// get_recovery_key_display wired items.id=229 (2026-08-09), rebuilt by
// items.id=629 (decisions.id=893): the 24-word phrase is now independent
// recovery entropy generated at account creation, not the key itself
// (Option B retired). Because it cannot be re-derived from anything stored,
// the entropy is held in KeyRegistry's one-shot pending slot from creation
// until this command takes it exactly once; a second call, or a call after
// logout, finds nothing. "not logged in" is a real, expected error path.
//
// SIGNATURE CHANGE, flagged: login() originally took only `password:
// String`. It now also takes `display_name: String` -- user_store's
// login-lookup path is keyed on display_name (see auth::user_store's own
// module header on why), so a single-parameter login() cannot identify
// which account to authenticate against. No frontend exists yet to be
// broken by this (confirmed this session, Architecture Section 14), but
// this is a real IPC contract change from the original stub, not a
// transparent one.
//
// SCOPE SEAM for future onboarding (Jason's direction, 2026-08-01): the
// bootstrap branch below creates a user account ONLY -- no persona is
// created. A logged-in fresh account has no usable persona/Focus/data
// store until onboarding (a separate, later, larger piece of work) runs.
// The exact point a future persona-creation call would go is marked
// explicitly below, so that addition is a small, additive change to this
// branch, not a rewrite.
//
// LOCKOUT MECHANISM, NOT POLICY (scoped this session): auth_failures/
// auth_lockouts are written to on every failed/successful attempt below,
// but auth_lockout_enabled (instance_config) stays 'disabled' -- no
// threshold, no duration, no actual lockout enforcement is decided or
// built here. is_locked_out() always returns false while the flag is
// disabled; the bookkeeping exists so a future session can flip
// enforcement on without a schema or data-flow change.
//
// PASSWORD CHECK (items.id=629, decisions.id=893): there is no password_hash
// column. login() derives the Argon2id password key (PK) and unwraps the
// account's random data key (DEK) from its user_key_wrappers row; the wrap's
// ChaCha20-Poly1305 tag is the password check, so a wrong password fails
// there, before any database is opened. The DEK, not the PK, is what the
// registry holds and what every store opens with.
//
// FAILURE CLASSIFICATION: a wrong password (wrap tag failure) and an
// unrelated failure (corrupt row, I/O error) are deliberately NOT treated
// the same (Jason's direction, 2026-08-01) -- only a confirmed wrong-
// password failure counts toward auth_failures/lockout bookkeeping.
//
// SESSION LIFECYCLE ON LOGOUT: auth_sessions rows are soft-expired
// (expires_at set to now), never deleted -- Jason's direction, 2026-08-01,
// to preserve an audit trail of when sessions existed and ended, matching
// how auth_failures is treated as data worth keeping rather than
// discarding.

use tauri::State;

use crate::auth::data_key::{self, DataKeyError};
use crate::auth::epoch_guard;
use crate::auth::kdf;
use crate::auth::registry::{
    key_hex, GroupKeyRegistry, KeyRegistry, UnlockedGroupKey, UnlockedKey,
};
use crate::auth::sharing_keypair;
use crate::auth::user_store;
use crate::persistence::{group_key_store, persona_store};

// ---------------------------------------------------------------------------
// IPC types
// ---------------------------------------------------------------------------

/// One-time recovery mnemonic display. Never persisted anywhere past this
/// response -- QR holds no copy of `mnemonic` or the entropy it was derived
/// from once this call returns (Section 5/8.5 invariant).
#[derive(Debug, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct RecoveryKeyDisplay {
    pub mnemonic: String,
}

/// The current session's user identity, for get_session (items.id=267).
/// Deliberately excludes master_key/key_hex -- CLAUDE.md's "Master key
/// never persisted" rule has no IPC-boundary carve-out.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct SessionInfo {
    pub user_id: String,
    pub display_name: String,
    pub role: String,
    pub is_primary: bool,
}

/// Shown for an account with no key wrapper -- created before the data-key
/// format. Deliberately neutral: it states what is true of the account and
/// says nothing about any remedy.
const LEGACY_ACCOUNT_MESSAGE: &str =
    "This account was created before the data-key format and can't be opened";

// ---------------------------------------------------------------------------
// Lockout bookkeeping (mechanism only -- see module header)
// ---------------------------------------------------------------------------

/// Always false while instance_config.auth_lockout_enabled = 'disabled'
/// (its current, unchanged default -- this session builds bookkeeping,
/// not enforcement). Queries the flag rather than hardcoding false, so
/// flipping enforcement on later is a data change, not a code change.
pub(crate) async fn is_locked_out(
    pool: &sqlx::SqlitePool,
    display_name: &str,
) -> Result<bool, String> {
    let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
    let enabled: Option<(String,)> =
        sqlx::query_as("SELECT value FROM instance_config WHERE key = 'auth_lockout_enabled'")
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;

    if enabled.map(|(v,)| v) != Some("enabled".to_owned()) {
        return Ok(false);
    }

    let row: Option<(String,)> =
        sqlx::query_as("SELECT locked_until FROM auth_lockouts WHERE display_name = ?")
            .bind(display_name)
            .fetch_optional(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;

    match row {
        None => Ok(false),
        Some((locked_until,)) => {
            Ok(locked_until.as_str() > crate::providers::utils::now().as_str())
        }
    }
}

/// Record a failed attempt in auth_failures. Lockout THRESHOLD/DURATION
/// logic is deliberately not built here (policy, not mechanism -- see
/// module header) -- this only appends the bookkeeping row.
pub(crate) async fn record_failed_attempt(
    pool: &sqlx::SqlitePool,
    display_name: &str,
) -> Result<(), String> {
    let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
    sqlx::query(
        "INSERT INTO auth_failures (id, display_name, attempted_at)
         VALUES (?, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(display_name)
    .bind(crate::providers::utils::now())
    .execute(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub async fn login(
    display_name: String,
    password: String,
    key_registry: State<'_, KeyRegistry>,
    group_key_registry: State<'_, GroupKeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<(), String> {
    let has_users = user_store::has_any_users(&pool)
        .await
        .map_err(|e| e.to_string())?;

    if !has_users {
        // BOOTSTRAP (Architecture Section 2.3: "First run: create the
        // primary admin user... require a password at that point, no
        // skip"). The supplied password becomes the new account's
        // password directly -- there is no separate account-creation step
        // for login() to defer to (Jason's direction, 2026-08-01).
        let salt = kdf::generate_salt().map_err(|e| e.to_string())?;
        let user_id = uuid::Uuid::new_v4().to_string();

        // items.id=629 (decisions.id=893): the Argon2id output is only the
        // password key. A random DEK and independent recovery entropy are
        // generated here, the DEK is wrapped under the PK and sealed to the
        // recovery public key, and the PK is dropped (zeroized) as soon as
        // the wrap exists.
        let material = {
            let password_key = zeroize::Zeroizing::new(
                kdf::derive_password_key(
                    password.as_bytes(),
                    &salt,
                    kdf::DEFAULT_ARGON2_MEMORY_KIB,
                    kdf::DEFAULT_ARGON2_ITERATIONS,
                    kdf::DEFAULT_ARGON2_PARALLELISM,
                )
                .map_err(|e| e.to_string())?,
            );
            data_key::create_account_key_material(&user_id, &password_key)
                .map_err(|e| e.to_string())?
        };
        let key_epoch = material.wrapper.key_epoch;

        // items.id=289, decisions.id=677: derived once here purely to get
        // the public key for the shared.db write below (create_user_with_wrapper
        // writes it atomically alongside users/user_salts and the key
        // wrapper). finish_login (below) re-derives the same keypair a second
        // time to populate the resident KeyRegistry entry -- a deliberate,
        // cheap redundancy (see that call site's own comment) rather than
        // threading this value through as extra state.
        let (_, sharing_public_key) =
            sharing_keypair::derive_sharing_keypair(&material.dek, &user_id, key_epoch);

        user_store::create_user_with_wrapper(
            &pool,
            &user_id,
            &display_name,
            "admin",
            true,
            &salt,
            kdf::DEFAULT_ARGON2_MEMORY_KIB,
            kdf::DEFAULT_ARGON2_ITERATIONS,
            kdf::DEFAULT_ARGON2_PARALLELISM,
            sharing_public_key.as_bytes(),
            &material.wrapper,
        )
        .await
        .map_err(|e| e.to_string())?;

        // items.id=630: every database key is derived for the epoch this
        // process unlocked under (auth::subkeys), so the epoch is registered
        // before any database is opened or created.
        epoch_guard::set_resident(&user_id, key_epoch);
        if let Err(e) = establish_account_dbs(&user_id, &material.dek).await {
            epoch_guard::clear_resident_for(&user_id);
            return Err(e);
        }

        // <<< SEAM: if future onboarding design decides the first-run
        // flow should also create a default persona, that call goes here
        // -- e.g. persona_store::create_persona(&user_id, ...). Not built
        // now: no onboarding design exists yet to specify what persona
        // type/name/Focus set that would be (Jason's direction,
        // 2026-08-01). A fresh account past this point can log in
        // successfully but has no usable persona/data store until
        // onboarding runs -- a real, named, deliberate intermediate
        // state, not an oversight. >>>

        // Held only until get_recovery_key_display takes it (one time).
        key_registry
            .set_pending_recovery(material.recovery_entropy.clone())
            .await;

        finish_login(
            &user_id,
            *material.dek,
            key_epoch,
            &key_registry,
            &group_key_registry,
            &pool,
        )
        .await
    } else {
        let user = user_store::find_user_by_display_name(&pool, &display_name)
            .await
            .map_err(|e| e.to_string())?
            // Same error message as a wrong password below -- do not
            // reveal whether the display_name itself exists.
            .ok_or_else(|| "invalid credentials".to_owned())?;

        if is_locked_out(&pool, &display_name).await? {
            return Err("account locked, try again later".to_owned());
        }

        let (salt, mem, iter, par) = user_store::get_salt_params(&pool, &user.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                // Should be impossible: create_user() always writes both
                // rows atomically. Surfaced distinctly rather than folded
                // into "invalid credentials" -- this is a real internal
                // inconsistency, not a wrong password, and hiding it
                // behind the generic message would make it undebuggable.
                "internal error: account exists with no salt record".to_owned()
            })?;

        // An account with no wrapper row predates the data-key format. It
        // is named as such rather than folded into "invalid credentials":
        // it is not a wrong password and retrying cannot fix it.
        let wrapper_state = user_store::get_key_wrapper_state(&pool, &user.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| LEGACY_ACCOUNT_MESSAGE.to_owned())?;
        let wrapper = wrapper_state.wrapper;
        if wrapper.format_version != data_key::WRAP_FORMAT_VERSION {
            return Err(format!(
                "couldn't verify credentials: unsupported key wrapper format {}",
                wrapper.format_version
            ));
        }

        // items.id=630: a data-key rotation was interrupted. The databases are
        // split between two keys, so there is no ordinary unlock; this
        // finishes the rotation (new password) or reports it (old password).
        if wrapper_state.pending_rotation.is_some() {
            return login_with_pending_rotation(
                &user,
                &display_name,
                &password,
                &key_registry,
                &group_key_registry,
                &pool,
            )
            .await;
        }

        let password_key = zeroize::Zeroizing::new(
            kdf::derive_password_key(password.as_bytes(), &salt, mem, iter, par)
                .map_err(|e| e.to_string())?,
        );

        match data_key::unwrap_dek(
            &password_key,
            &wrapper.password_wrap,
            &user.id,
            wrapper.key_epoch,
            wrapper.format_version,
        ) {
            Err(DataKeyError::WrongPassword) => {
                record_failed_attempt(&pool, &display_name).await?;
                Err("invalid credentials".to_owned())
            }
            Err(e) => {
                // Deliberately NOT recorded as a failed attempt (Jason's
                // direction, 2026-08-01) -- a malformed wrapper is not
                // evidence the password was wrong.
                Err(format!("couldn't verify credentials: {e}"))
            }
            Ok(dek) => {
                // Idempotent create-or-migrate: a crash between
                // create_user_with_wrapper() and the file creation in the
                // bootstrap branch leaves an account whose wrapper is
                // complete but whose first databases do not exist yet --
                // this recreates them under the same DEK instead of
                // leaving the account unusable.
                // items.id=630: register the epoch BEFORE any database is
                // opened, so its subkey is derived for the right epoch.
                epoch_guard::set_resident(&user.id, wrapper.key_epoch);
                if let Err(e) = establish_account_dbs(&user.id, &dek).await {
                    epoch_guard::clear_resident_for(&user.id);
                    return Err(e);
                }
                finish_login(
                    &user.id,
                    *dek,
                    wrapper.key_epoch,
                    &key_registry,
                    &group_key_registry,
                    &pool,
                )
                .await
            }
        }
    }
}

/// `login` error codes for an interrupted data-key rotation (items.id=630).
/// Stable strings the frontend matches on; they carry no ids or paths.
///
/// The account's password change (or restore, or new phrase) was interrupted
/// and the password typed is the OLD one, which can no longer open the data.
/// Sign in with the new password, or use the recovery phrase, to finish it.
pub const LOGIN_ROTATION_INTERRUPTED: &str = "rotation_interrupted";
/// The interrupted change was undone while resuming; sign in with the old
/// password.
pub const LOGIN_ROTATION_ROLLED_BACK: &str = "rotation_rolled_back";
/// The interrupted change could not be finished right now and stays pending.
pub const LOGIN_ROTATION_RESUME_FAILED: &str = "rotation_resume_failed";

/// Unlock an account whose data-key rotation is pending (items.id=630).
///
/// The typed password is tried against the STAGED new wrap first (that is the
/// only way to reach the new data key from a password; the old data key comes
/// out of the pending row). On success the rotation is finished and the
/// account is signed in. If the staged wrap does not open but the LIVE wrap
/// does, the user typed the old password: that is not a failed attempt and
/// not a session, it is the interrupted-change result. Anything else is a
/// wrong password.
async fn login_with_pending_rotation(
    user: &user_store::UserRecord,
    display_name: &str,
    password: &str,
    key_registry: &State<'_, KeyRegistry>,
    group_key_registry: &State<'_, GroupKeyRegistry>,
    pool: &sqlx::SqlitePool,
) -> Result<(), String> {
    use crate::auth::key_rotation::{
        self, ResumeOutcome, RotationEnv, StagedCredential, StagedUnlockError,
    };

    let pending = user_store::get_pending_rotation(pool, &user.id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "internal error: a rotation is flagged but missing".to_owned())?;

    match key_rotation::unlock_staged(
        &pending,
        &user.id,
        StagedCredential::Password(password.as_bytes()),
    )
    .await
    {
        Ok(unlock) => {
            let env = RotationEnv::new(
                pool,
                &user.id,
                crate::persistence::migrations::get_data_root()
                    .join("users")
                    .join(&user.id),
            );
            match key_rotation::resume_rotation(
                &env,
                &pending,
                unlock.new_dek.clone(),
                unlock.credential(),
            )
            .await
            {
                Ok(ResumeOutcome::Completed(outcome)) => {
                    finish_login(
                        &user.id,
                        *outcome.new_dek,
                        outcome.to_epoch,
                        key_registry,
                        group_key_registry,
                        pool,
                    )
                    .await
                }
                Ok(ResumeOutcome::RolledBack) => Err(LOGIN_ROTATION_ROLLED_BACK.to_owned()),
                Err(e) => {
                    log::warn!("login: couldn't finish the interrupted rotation: {e}");
                    Err(LOGIN_ROTATION_RESUME_FAILED.to_owned())
                }
            }
        }
        Err(StagedUnlockError::Unusable) => Err(LOGIN_ROTATION_RESUME_FAILED.to_owned()),
        Err(StagedUnlockError::Kdf(e)) => Err(e.to_string()),
        Err(StagedUnlockError::WrongCredential) => {
            // Not the new password. Is it the old one?
            let state = user_store::get_key_wrapper_state(pool, &user.id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| LEGACY_ACCOUNT_MESSAGE.to_owned())?;
            let (live_salt, lm, li, lp) = user_store::get_salt_params(pool, &user.id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "internal error: account exists with no salt record".to_owned())?;
            let live_pk =
                kdf::derive_password_key_blocking(password.as_bytes(), &live_salt, lm, li, lp)
                    .await
                    .map_err(|e| e.to_string())?;
            match data_key::unwrap_dek(
                &live_pk,
                &state.wrapper.password_wrap,
                &user.id,
                state.wrapper.key_epoch,
                state.wrapper.format_version,
            ) {
                Ok(_) => Err(LOGIN_ROTATION_INTERRUPTED.to_owned()),
                Err(DataKeyError::WrongPassword) => {
                    record_failed_attempt(pool, display_name).await?;
                    Err("invalid credentials".to_owned())
                }
                Err(e) => Err(format!("couldn't verify credentials: {e}")),
            }
        }
        Err(StagedUnlockError::Failed(e)) => Err(format!("couldn't verify credentials: {e}")),
    }
}

/// Create-or-migrate the two per-account databases every account has from
/// its first moment (integration_keys.db, cloud_chat_cookies.db) under the
/// DEK. Safe to repeat.
async fn establish_account_dbs(
    user_id: &str,
    dek: &[u8; kdf::MASTER_KEY_LEN],
) -> Result<(), String> {
    let dek_hex = zeroize::Zeroizing::new(key_hex(dek));
    // Establishes integration_keys.db under the real key (migrate_keys_db is
    // create-or-migrate).
    crate::persistence::migrations::migrate_keys_db(user_id, &dek_hex)
        .await
        // MigrationError's Display impl (via #[error(...)] on each
        // variant) already produces a message -- only the Failed
        // variant carries a plain_language field specifically,
        // matching it alone would miss Locked/Sqlx/Io. e.to_string()
        // via Display covers all four correctly.
        .map_err(|e| e.to_string())?;

    // Establishes cloud_chat_cookies.db, same create-or-migrate call shape
    // as integration_keys.db just above (items.id=224 resolution,
    // decisions.id=711) -- see tier3_cookies_001.sql's own header.
    crate::persistence::migrations::migrate_tier3_cookies_db(user_id, &dek_hex)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Decode a stored group_key_hex value into raw key bytes, or None if it's
/// malformed (odd length, non-hex characters, or the wrong decoded length).
/// Local to this module, matching this codebase's established convention of
/// duplicating this small hex-decode loop per module rather than factoring
/// out a shared utility (existing independent copies: auth/sharing_keypair.rs,
/// auth/group_invitations.rs, auth/group_membership.rs, auth/user_store.rs).
fn hex_decode_group_key(s: &str) -> Option<[u8; kdf::MASTER_KEY_LEN]> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect();
    bytes?.try_into().ok()
}

pub(crate) async fn finish_login(
    user_id: &str,
    data_key: [u8; kdf::MASTER_KEY_LEN],
    key_epoch: u64,
    key_registry: &State<'_, KeyRegistry>,
    group_key_registry: &State<'_, GroupKeyRegistry>,
    pool: &sqlx::SqlitePool,
) -> Result<(), String> {
    let now = crate::providers::utils::now();

    // items.id=289, decisions.id=677: re-derived here (not persisted -- see
    // auth::sharing_keypair's module header) so it follows data_key's own
    // single-slot lifecycle exactly -- both installed into the registry
    // together, both evicted together on logout/clear.
    let (sharing_private_key, _) =
        sharing_keypair::derive_sharing_keypair(&data_key, user_id, key_epoch);

    key_registry
        .replace(UnlockedKey {
            user_id: user_id.to_owned(),
            data_key,
            key_epoch,
            sharing_private_key: sharing_private_key.to_bytes(),
            unlocked_at: now.clone(),
        })
        .await;
    // items.id=629: from here every store open for this account compares
    // the epoch it unlocked under against the stored one (auth::epoch_guard).
    epoch_guard::set_resident(user_id, key_epoch);

    // items.id=303 (decisions.id=722): persona-share sync's "app-start" pull
    // trigger -- the moment this account's sharing private key becomes
    // resident, mirroring auth::group_invitations::accept_invitation's own
    // immediate pull for the identical reason (the registry starts empty at
    // process boot, so a literal process-boot pull would find nothing;
    // main.rs's periodic timer covers the steady-state case). Best-effort,
    // fire-and-forget inside the fn -- never turns an otherwise-successful
    // login into an Err.
    let personal_key_hex_for_sync = key_hex(&data_key);
    crate::persona_sync::engine::pull_all_accepted_shares_on_login(
        pool,
        user_id,
        &personal_key_hex_for_sync,
        &sharing_private_key,
    )
    .await;

    // items.id=304 (decisions.id=723): same "app-start" pull trigger, for
    // VIEW-ONLY shares -- mirrors the SYNCED call immediately above, as a
    // separate sibling call for the same module-separation reasoning
    // persona_view_sync::engine's own header gives.
    crate::persona_view_sync::engine::pull_all_accepted_view_shares_on_login(
        pool,
        user_id,
        &personal_key_hex_for_sync,
        &sharing_private_key,
    )
    .await;

    // items.id=252: crash-interrupted focus_runs recovery -- a third
    // sibling call in this same "the moment key material becomes resident"
    // sequence as the two pull calls immediately above. There is no
    // per-persona "unlock" event in this codebase (key_hex is derived once
    // per-user here; persona_id is just a routing parameter passed
    // alongside it) so this is the correct, and only, call site --
    // demote_interrupted_runs() (conductor::lifecycle) is otherwise never
    // invoked, leaving crash-interrupted runs stuck at running/initializing
    // forever. Best-effort per persona, same posture as the two calls
    // above: logged and skipped, never turns an otherwise-successful login
    // into an Err.
    match persona_store::list_personas_for_user(pool, user_id).await {
        Ok(personas) => {
            for persona in personas {
                if let Err(e) = crate::conductor::lifecycle::demote_interrupted_runs(
                    user_id,
                    &persona.id,
                    &personal_key_hex_for_sync,
                )
                .await
                {
                    log::warn!(
                        "finish_login: couldn't demote interrupted focus_runs for \
                         persona={}: {e} -- skipping this persona",
                        persona.id
                    );
                }
            }
        }
        Err(e) => log::warn!(
            "finish_login: couldn't list personas for user={user_id}: {e} -- \
             skipping interrupted-run recovery entirely"
        ),
    }

    // items.id=710: spend-ledger recovery -- one more sibling call in this
    // "the moment key material becomes resident" sequence. A hosted call that
    // was reserved in the ledger but never settled because the process died
    // is converted to `interrupted` and stays charged at its reserved worst
    // case, so a crash can never make spend vanish from the caps. Best-effort:
    // logged inside, never turns an otherwise-successful login into an Err.
    let _ = crate::persistence::provider_spend_store::recover_interrupted_spend(
        user_id,
        &personal_key_hex_for_sync,
    )
    .await;

    // items.id=290, decisions.id=718: rehydrate GroupKeyRegistry from every
    // one of this account's personas' personal.db group_keys tables --
    // GroupKeyRegistry itself stays deliberately volatile (auth/registry.rs's
    // own header), so this is the only point anything re-populates it after
    // a restart. Best-effort at every level (Q1, confirmed): a malformed
    // row, an entire persona's unreadable personal.db, or the persona list
    // itself failing are all logged and skipped, never propagated -- this
    // is account-level login, and blocking an entire account over one
    // corrupted ancillary row would be a lockout bug, not a safety
    // improvement (there would be no way to fix the row without being able
    // to log in in the first place).
    let personal_key_hex = key_hex(&data_key);
    match persona_store::list_personas_for_user(pool, user_id).await {
        Ok(personas) => {
            for persona in personas {
                match group_key_store::list_group_keys(user_id, &persona.id, &personal_key_hex)
                    .await
                {
                    Ok(rows) => {
                        for row in rows {
                            let Some(group_key) = hex_decode_group_key(&row.group_key_hex) else {
                                log::warn!(
                                    "finish_login: malformed group_key_hex for persona={} \
                                     group={} -- skipping this row",
                                    persona.id,
                                    row.group_id
                                );
                                continue;
                            };
                            group_key_registry
                                .replace(
                                    &persona.id,
                                    &row.group_id,
                                    UnlockedGroupKey {
                                        group_id: row.group_id.clone(),
                                        group_key,
                                        unlocked_at: now.clone(),
                                    },
                                )
                                .await;
                        }
                    }
                    Err(e) => log::warn!(
                        "finish_login: couldn't read group_keys for persona={}: {e} -- \
                         skipping this persona",
                        persona.id
                    ),
                }
            }
        }
        Err(e) => log::warn!(
            "finish_login: couldn't list personas for user={user_id}: {e} -- \
             skipping group-key rehydration entirely"
        ),
    }

    let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
    // Section 3.1: absolute session lifetime, "hard ceiling... currently
    // 30 days... stays out of the settings surface entirely" -- a
    // constant, not a per-row/per-user configurable field (unlike
    // idle_timeout_minutes on users, which is per-user and adjustable).
    let expires_at = (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339();
    sqlx::query(
        "INSERT INTO auth_sessions
         (session_id, user_id, created_at, last_active_at, expires_at, is_remember_me)
         VALUES (?, ?, ?, ?, ?, 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(user_id)
    .bind(&now)
    .bind(&now)
    .bind(&expires_at)
    .execute(&mut *conn)
    .await
    .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn logout(
    key_registry: State<'_, KeyRegistry>,
    group_key_registry: State<'_, GroupKeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
    connect_sessions: State<'_, crate::provider_credentials::session::ConnectSessions>,
) -> Result<(), String> {
    let user_id = key_registry.with_key(|k| k.user_id.clone()).await;
    key_registry.clear().await;
    // items.id=714: a connect dialog's session, and any clipboard text it was
    // holding for a key the user had not yet confirmed, must not outlive the
    // account that opened it. Cleared whether or not a key was resident.
    connect_sessions.clear();

    if let Some(user_id) = user_id {
        // items.id=469: group keys are per-Persona, not per-account, so
        // clearing KeyRegistry alone left every Persona's group key
        // resident until process exit. See auth::clear_group_keys_for_user
        // for the enumerate-and-clear logic shared with idle-timeout.
        crate::auth::clear_group_keys_for_user(&pool, &group_key_registry, &user_id).await;

        let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
        let now = crate::providers::utils::now();
        // Soft-expire (Jason's direction, 2026-08-01): update expires_at
        // rather than DELETE, to preserve an audit trail of when sessions
        // existed and ended.
        sqlx::query(
            "UPDATE auth_sessions SET expires_at = ?
             WHERE user_id = ? AND expires_at > ?",
        )
        .bind(&now)
        .bind(&user_id)
        .bind(&now)
        .execute(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

/// Record activity on the resident account's live session(s) (items.id=311).
/// Called by a debounced frontend listener (mouse/keyboard/scroll/click),
/// not on every IPC command -- see auth::idle_timeout's own module header
/// for why a frontend-driven activity signal was chosen over a backend-
/// command-tracked one. A no-op, not an error, when nobody is logged in --
/// a stray ping racing a logout is expected, not exceptional.
#[tauri::command]
#[specta::specta]
pub async fn record_activity(
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<(), String> {
    let user_id = key_registry.with_key(|k| k.user_id.clone()).await;
    if let Some(user_id) = user_id {
        let mut conn = pool.acquire().await.map_err(|e| e.to_string())?;
        let now = crate::providers::utils::now();
        sqlx::query(
            "UPDATE auth_sessions SET last_active_at = ?
             WHERE user_id = ? AND expires_at > ?",
        )
        .bind(&now)
        .bind(&user_id)
        .bind(&now)
        .execute(&mut *conn)
        .await
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

/// Return the new account's 24-word recovery phrase for its ONE-TIME
/// display (items.id=629, decisions.id=893, Architecture 8.5). The phrase is
/// independent 256-bit recovery entropy generated at account creation, not
/// the data key, and it is held only in KeyRegistry's pending slot: this
/// command takes it, so a second call (or a call after logout) finds
/// nothing. QR keeps no copy once this returns. Requires a resident session
/// the same way qr_hosted::get_qr_hosted_config does; "not logged in" is the
/// same error string for the same reason.
#[tauri::command]
#[specta::specta]
pub async fn get_recovery_key_display(
    key_registry: State<'_, KeyRegistry>,
) -> Result<RecoveryKeyDisplay, String> {
    if !key_registry.is_occupied().await {
        return Err("not logged in".to_owned());
    }

    let entropy = key_registry
        .take_pending_recovery()
        .await
        .ok_or_else(|| "recovery phrase is not available".to_owned())?;

    let mnemonic = data_key::mnemonic_from_entropy(&entropy).map_err(|e| e.to_string())?;

    Ok(RecoveryKeyDisplay {
        mnemonic: mnemonic.to_string(),
    })
}

/// Read back the current session's identity (items.id=267), if any.
/// `Ok(None)` -- not `Err` -- is the expected result on every cold app
/// launch: the master key is never persisted (CLAUDE.md), so KeyRegistry
/// starts empty every process lifetime and there is no auto-relogin. `Err`
/// is reserved for a real query failure, including the internal
/// inconsistency of a resident user_id with no matching users row (should
/// be impossible -- same reasoning login()'s "account exists with no salt
/// record" branch uses).
#[tauri::command]
#[specta::specta]
pub async fn get_session(
    key_registry: State<'_, KeyRegistry>,
    pool: State<'_, sqlx::SqlitePool>,
) -> Result<Option<SessionInfo>, String> {
    let Some(user_id) = key_registry.with_key(|k| k.user_id.clone()).await else {
        return Ok(None);
    };

    let user = user_store::find_user_by_id(&pool, &user_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "internal error: session references a nonexistent user".to_owned())?;

    Ok(Some(SessionInfo {
        user_id: user.id,
        display_name: user.display_name,
        role: user.role,
        is_primary: user.is_primary,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::Manager;

    // Shared across every test module that mutates QR_DATA_ROOT (items.id=
    // 205, 2026-08-01) -- see test_support.rs for why this must be one
    // true mutex, not a per-module copy. This file previously declared its
    // own local static ENV_MUTEX (reasoning at the time: "not shared...
    // small, stable, no real duplication cost") -- that reasoning was
    // wrong: three textually-identical but structurally independent
    // ENV_MUTEX statics (this file, auth::user_store, providers::utils)
    // raced against each other under parallel test execution, causing 15
    // spurious failures when the full suite ran (confirmed this session --
    // passed 438/438 serial via --test-threads=1, failed 15 in parallel
    // before this fix). Fixed by importing the one shared mutex instead.
    use crate::test_support::ENV_MUTEX;

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
        pool: sqlx::SqlitePool,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    async fn setup() -> TestEnv {
        let lock = ENV_MUTEX.lock().await;
        let saved_root = std::env::var("QR_DATA_ROOT").ok();

        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");

        let pool =
            sqlx::SqlitePool::connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .expect("shared.db pool must connect");

        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
            pool,
        }
    }

    /// A mock Tauri app with a fresh, empty KeyRegistry managed as state --
    /// tauri::test::mock_app() (feature "test", dev-dependencies only)
    /// builds a real App<MockRuntime> without needing a webview or config
    /// file (verified against tauri 2.11.x docs this session). Commands
    /// are called directly as functions, not through the IPC layer -- this
    /// tests login()/logout()'s own logic, not Tauri's invoke plumbing.
    fn mock_app_with_registry(pool: sqlx::SqlitePool) -> tauri::App<tauri::test::MockRuntime> {
        let app = tauri::test::mock_app();
        app.manage(KeyRegistry::default());
        app.manage(GroupKeyRegistry::default());
        app.manage(crate::provider_credentials::session::ConnectSessions::default());
        app.manage(pool);
        app
    }

    #[tokio::test]
    async fn bootstrap_login_creates_primary_admin_and_populates_registry() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        let result = login(
            "Alice".to_owned(),
            "correct horse battery staple".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await;
        assert!(result.is_ok(), "bootstrap login should succeed: {result:?}");
        assert!(
            registry.is_occupied().await,
            "registry should hold a key after bootstrap login"
        );

        let user = user_store::find_user_by_display_name(&pool, "Alice")
            .await
            .unwrap();
        assert!(user.is_some());
        let user = user.unwrap();
        assert_eq!(user.role, "admin");
        assert!(user.is_primary);
    }

    #[tokio::test]
    async fn bootstrap_login_writes_a_session_row() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM auth_sessions")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(
            count.0, 1,
            "exactly one session row should exist after bootstrap login"
        );
    }

    #[tokio::test]
    async fn record_activity_updates_last_active_at_for_the_logged_in_user() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "correct horse battery staple".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        let original: String = sqlx::query_scalar("SELECT last_active_at FROM auth_sessions")
            .fetch_one(&mut *conn)
            .await
            .unwrap();

        // Ensure the next timestamp is observably later -- to_rfc3339()
        // includes sub-second precision, but back the clock off by a
        // second's worth of margin rather than relying on real-time
        // granularity between two calls a few microseconds apart.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        record_activity(registry.clone(), pool.clone())
            .await
            .unwrap();

        let updated: String = sqlx::query_scalar("SELECT last_active_at FROM auth_sessions")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert!(
            updated > original,
            "record_activity must bump last_active_at forward"
        );
    }

    #[tokio::test]
    async fn second_bootstrap_attempt_is_a_real_login_not_a_second_bootstrap() {
        // Once has_any_users() is true, a subsequent login() call must take
        // the existing-account branch, not try to bootstrap a second
        // primary admin (which the DB would reject anyway via the partial
        // unique index -- but login() should never attempt it).
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        registry.clear().await; // simulate a fresh process, key not resident

        let result = login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await;
        assert!(
            result.is_ok(),
            "logging in again with the correct password should succeed: {result:?}"
        );
    }

    #[tokio::test]
    async fn wrong_password_on_existing_account_fails_and_does_not_populate_registry() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "correct-password".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        registry.clear().await;

        let result = login(
            "Alice".to_owned(),
            "wrong-password".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await;
        assert!(result.is_err(), "wrong password must fail");
        assert!(
            !registry.is_occupied().await,
            "registry must stay empty after a failed login"
        );
    }

    #[tokio::test]
    async fn wrong_password_records_a_failed_attempt() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "correct-password".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        registry.clear().await;

        let _ = login(
            "Alice".to_owned(),
            "wrong-password".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await;

        let mut conn = pool.acquire().await.unwrap();
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM auth_failures WHERE display_name = 'Alice'")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(
            count.0, 1,
            "one auth_failures row should be recorded for the wrong password"
        );
    }

    #[tokio::test]
    async fn unknown_display_name_fails_without_revealing_nonexistence() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        // Bootstrap someone first so has_any_users() is true and this
        // exercises the existing-account branch, not bootstrap.
        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        registry.clear().await;

        let result = login(
            "Nobody".to_owned(),
            "whatever".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await;
        assert_eq!(result, Err("invalid credentials".to_owned()));
    }

    #[tokio::test]
    async fn logout_clears_the_registry() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        assert!(registry.is_occupied().await);

        logout(
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
            app.state::<crate::provider_credentials::session::ConnectSessions>(),
        )
        .await
        .unwrap();
        assert!(
            !registry.is_occupied().await,
            "registry must be empty after logout"
        );
    }

    #[tokio::test]
    async fn logout_clears_group_keys_for_every_persona_under_the_account_but_not_others() {
        // items.id=469: logout() previously called only key_registry.clear()
        // -- group keys resident for the logged-out account's Personas
        // outlived the session. This exercises the multi-Persona case
        // (one account can have more than one Persona) and confirms the
        // eviction is scoped correctly: a different account's Persona group
        // key must survive this account's logout.
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        let alice = user_store::find_user_by_display_name(&pool, "Alice")
            .await
            .unwrap()
            .unwrap();

        let alice_persona_1 = uuid::Uuid::new_v4().to_string();
        let alice_persona_2 = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(
            &pool,
            &alice_persona_1,
            "Alice Persona 1",
            "personal",
            &alice.id,
            None,
        )
        .await
        .expect("create_persona must succeed");
        persona_store::create_persona(
            &pool,
            &alice_persona_2,
            "Alice Persona 2",
            "personal",
            &alice.id,
            None,
        )
        .await
        .expect("create_persona must succeed");

        // A different account's Persona -- registry.clear_persona() must
        // never touch this, since it belongs to an account other than the
        // one logging out (even though KeyRegistry's single-slot model
        // means only one account's master key can ever be resident at a
        // time, GroupKeyRegistry itself has no such constraint).
        let bob_id = uuid::Uuid::new_v4().to_string();
        user_store::create_user(
            &pool,
            &bob_id,
            "Bob",
            "user",
            false,
            &[0u8; 16],
            kdf::DEFAULT_ARGON2_MEMORY_KIB,
            kdf::DEFAULT_ARGON2_ITERATIONS,
            kdf::DEFAULT_ARGON2_PARALLELISM,
            &[0u8; 32],
        )
        .await
        .expect("create_user must succeed");
        let bob_persona = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(
            &pool,
            &bob_persona,
            "Bob Persona",
            "personal",
            &bob_id,
            None,
        )
        .await
        .expect("create_persona must succeed");

        for (persona_id, group_id) in [
            (&alice_persona_1, "group-a1"),
            (&alice_persona_2, "group-a2"),
            (&bob_persona, "group-b1"),
        ] {
            group_key_registry
                .replace(
                    persona_id,
                    group_id,
                    UnlockedGroupKey {
                        group_id: group_id.to_owned(),
                        group_key: [0xAAu8; kdf::MASTER_KEY_LEN],
                        unlocked_at: crate::providers::utils::now(),
                    },
                )
                .await;
        }
        assert!(
            group_key_registry
                .is_occupied(&alice_persona_1, "group-a1")
                .await
        );
        assert!(
            group_key_registry
                .is_occupied(&alice_persona_2, "group-a2")
                .await
        );
        assert!(
            group_key_registry
                .is_occupied(&bob_persona, "group-b1")
                .await
        );

        logout(
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
            app.state::<crate::provider_credentials::session::ConnectSessions>(),
        )
        .await
        .unwrap();

        assert!(
            !group_key_registry
                .is_occupied(&alice_persona_1, "group-a1")
                .await,
            "logout must evict this account's first Persona's group keys"
        );
        assert!(
            !group_key_registry
                .is_occupied(&alice_persona_2, "group-a2")
                .await,
            "logout must evict this account's second Persona's group keys too"
        );
        assert!(
            group_key_registry
                .is_occupied(&bob_persona, "group-b1")
                .await,
            "logout must not clear a different account's Persona group keys"
        );
    }

    #[tokio::test]
    async fn logout_soft_expires_the_session_row_rather_than_deleting_it() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        logout(
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
            app.state::<crate::provider_credentials::session::ConnectSessions>(),
        )
        .await
        .unwrap();

        let mut conn = pool.acquire().await.unwrap();
        // Row must still exist (not deleted) -- audit trail, Jason's
        // direction, 2026-08-01.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM auth_sessions")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(
            count.0, 1,
            "session row must persist after logout, not be deleted"
        );

        let row: (String,) = sqlx::query_as("SELECT expires_at FROM auth_sessions LIMIT 1")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        let now = crate::providers::utils::now();
        assert!(
            row.0 <= now,
            "expires_at must be updated to at or before now after logout"
        );
    }

    /// items.id=714: a connect session and the clipboard candidate it holds do
    /// not survive logout, whether or not a key was resident.
    #[tokio::test]
    async fn logout_ends_a_connect_session_and_drops_its_clipboard_candidate() {
        use crate::provider_credentials::session::{ConnectSessions, SessionError};
        use std::time::Instant;

        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();
        let sessions = app.state::<ConnectSessions>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        let user_id = registry.with_key(|k| k.user_id.clone()).await.unwrap();

        let now = Instant::now();
        let id = sessions.begin(&user_id, "some-provider", now).unwrap();
        sessions
            .with_session(&id, &user_id, now, |s| {
                s.set_candidate(zeroize::Zeroizing::new(
                    "fx_candidate_0123456789".to_owned(),
                ))
            })
            .unwrap();
        assert_eq!(
            sessions.with_session(&id, &user_id, now, |s| s.has_candidate()),
            Ok(true)
        );

        logout(
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
            sessions.clone(),
        )
        .await
        .unwrap();

        assert!(sessions.is_empty(), "logout must drop the session");
        assert_eq!(
            sessions.with_session(&id, &user_id, now, |s| s.has_candidate()),
            Err(SessionError::NoSession),
            "the candidate must not be reachable after logout, even for the same user"
        );
    }

    #[tokio::test]
    async fn logout_clears_a_connect_session_even_when_no_key_was_resident() {
        use crate::provider_credentials::session::ConnectSessions;

        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();
        let sessions = app.state::<ConnectSessions>();
        sessions
            .begin("someone", "some-provider", std::time::Instant::now())
            .unwrap();

        logout(
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
            sessions.clone(),
        )
        .await
        .unwrap();
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn logout_on_empty_registry_is_a_no_op_not_an_error() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        let result = logout(
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
            app.state::<crate::provider_credentials::session::ConnectSessions>(),
        )
        .await;
        assert!(result.is_ok(), "logout with no resident key must not error");
    }

    // -----------------------------------------------------------------
    // Data-key hierarchy (items.id=629, decisions.id=893)
    // -----------------------------------------------------------------

    /// Captures every `log` record emitted in this test process, so the
    /// no-key-material test can search them. Installed at most once.
    struct CaptureLogger;
    static CAPTURED_LOGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    impl log::Log for CaptureLogger {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            CAPTURED_LOGS
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(format!("{}", record.args()));
        }
        fn flush(&self) {}
    }

    fn hex_of(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    async fn do_login(
        app: &tauri::App<tauri::test::MockRuntime>,
        name: &str,
        password: &str,
    ) -> Result<(), String> {
        login(
            name.to_owned(),
            password.to_owned(),
            app.state::<KeyRegistry>(),
            app.state::<GroupKeyRegistry>(),
            app.state::<sqlx::SqlitePool>(),
        )
        .await
    }

    #[tokio::test]
    async fn create_then_unlock_round_trip_resolves_the_same_data_key_and_epoch() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();

        do_login(&app, "Alice", "correct horse battery staple")
            .await
            .unwrap();
        let first = registry
            .with_key(|k| (k.data_key, k.key_epoch, k.user_id.clone()))
            .await
            .unwrap();
        assert_eq!(first.1, 1, "a new account starts at key epoch 1");

        registry.clear().await;
        assert!(!registry.is_occupied().await);

        do_login(&app, "Alice", "correct horse battery staple")
            .await
            .unwrap();
        let second = registry
            .with_key(|k| (k.data_key, k.key_epoch, k.user_id.clone()))
            .await
            .unwrap();
        assert_eq!(
            first, second,
            "unlock must unwrap the same DEK, not derive a new one"
        );

        // The unwrapped DEK really is the key the account's stores use.
        crate::persistence::integration_keys_store::open_integration_keys_db(
            &second.2,
            &key_hex(&second.0),
        )
        .await
        .expect("integration_keys.db must open under the unwrapped DEK");
    }

    #[tokio::test]
    async fn the_dek_is_not_the_password_key() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        do_login(&app, "Alice", "a password").await.unwrap();
        let (dek, user_id) = registry
            .with_key(|k| (k.data_key, k.user_id.clone()))
            .await
            .unwrap();

        let (salt, mem, iter, par) = user_store::get_salt_params(&_env.pool, &user_id)
            .await
            .unwrap()
            .unwrap();
        let pk = kdf::derive_password_key(b"a password", &salt, mem, iter, par).unwrap();
        assert_ne!(pk, dek, "the Argon2id output is only the password key");
    }

    #[tokio::test]
    async fn recovery_phrase_unseals_the_resident_data_key() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        do_login(&app, "Alice", "pw-for-recovery").await.unwrap();
        let (dek, user_id) = registry
            .with_key(|k| (k.data_key, k.user_id.clone()))
            .await
            .unwrap();

        let display = get_recovery_key_display(registry.clone()).await.unwrap();
        assert_eq!(display.mnemonic.split_whitespace().count(), 24);

        let entropy = data_key::entropy_from_mnemonic(&display.mnemonic).unwrap();
        assert_ne!(
            *entropy, dek,
            "Option B is retired: the phrase is not the key"
        );

        let wrapper = user_store::get_key_wrapper(&_env.pool, &user_id)
            .await
            .unwrap()
            .unwrap();
        let (secret, public) = data_key::derive_recovery_keypair(&entropy, &user_id);
        assert_eq!(public.as_bytes(), &wrapper.recovery_public_key);

        let unsealed = data_key::unseal_dek_with_recovery_key(
            &secret,
            &wrapper.recovery_seal,
            &user_id,
            wrapper.key_epoch,
            wrapper.format_version,
        )
        .unwrap();
        assert_eq!(
            *unsealed, dek,
            "the typed words must recover the resident DEK"
        );
    }

    #[tokio::test]
    async fn get_recovery_key_display_fails_without_a_resident_session() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();

        let result = get_recovery_key_display(registry.clone()).await;
        assert_eq!(result.err(), Some("not logged in".to_owned()));
    }

    #[tokio::test]
    async fn get_recovery_key_display_shows_the_phrase_once() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        do_login(&app, "Alice", "pw").await.unwrap();

        assert!(get_recovery_key_display(registry.clone()).await.is_ok());
        assert_eq!(
            get_recovery_key_display(registry.clone()).await.err(),
            Some("recovery phrase is not available".to_owned()),
            "a second call must find nothing"
        );

        // A later unlock is not account creation: no phrase is available.
        registry.clear().await;
        do_login(&app, "Alice", "pw").await.unwrap();
        assert_eq!(
            get_recovery_key_display(registry.clone()).await.err(),
            Some("recovery phrase is not available".to_owned())
        );
    }

    #[tokio::test]
    async fn logout_discards_an_undisplayed_recovery_phrase() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        do_login(&app, "Alice", "pw").await.unwrap();
        registry.clear().await;
        assert!(registry.take_pending_recovery().await.is_none());
    }

    #[tokio::test]
    async fn wrong_password_fails_at_the_wrap_tag_before_any_store_is_opened() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        do_login(&app, "Alice", "right-password").await.unwrap();
        let user_id = registry.with_key(|k| k.user_id.clone()).await.unwrap();
        registry.clear().await;

        // Remove every per-account store. The old check opened
        // integration_keys.db and would have reported an I/O-style failure
        // here. The wrap tag needs no file.
        let ik = crate::providers::utils::db_path_integration_keys(&user_id);
        let ck = crate::providers::utils::db_path_tier3_cookies(&user_id);
        for p in [&ik, &ck] {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
            }
        }

        let result = do_login(&app, "Alice", "wrong-password").await;
        assert_eq!(result, Err("invalid credentials".to_owned()));
        assert!(!registry.is_occupied().await);
        assert!(
            !ik.exists() && !ck.exists(),
            "a failed password check must not touch any store"
        );

        let mut conn = _env.pool.acquire().await.unwrap();
        let failures: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM auth_failures WHERE display_name = 'Alice'")
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(failures.0, 1);
    }

    #[tokio::test]
    async fn interrupted_wrapper_write_leaves_no_half_made_account_and_a_retry_works() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();

        let mut conn = _env.pool.acquire().await.unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_wrapper_write BEFORE INSERT ON user_key_wrappers
             BEGIN SELECT RAISE(ABORT, 'simulated interruption'); END",
        )
        .execute(&mut *conn)
        .await
        .unwrap();

        let result = do_login(&app, "Alice", "pw").await;
        assert!(
            result.is_err(),
            "the failed wrapper write must fail account creation"
        );
        assert!(!registry.is_occupied().await);
        for table in [
            "users",
            "user_salts",
            "user_sharing_keys",
            "user_key_wrappers",
        ] {
            let n: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(&mut *conn)
                .await
                .unwrap();
            assert_eq!(n.0, 0, "{table} must be rolled back with the wrapper");
        }

        sqlx::query("DROP TRIGGER fail_wrapper_write")
            .execute(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        do_login(&app, "Alice", "pw")
            .await
            .expect("a retry must bootstrap cleanly");
        assert!(registry.is_occupied().await);
    }

    #[tokio::test]
    async fn crash_after_the_account_rows_are_written_still_unlocks_and_recreates_the_stores() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        do_login(&app, "Alice", "pw").await.unwrap();
        let user_id = registry.with_key(|k| k.user_id.clone()).await.unwrap();
        registry.clear().await;

        // The state a crash between create_user_with_wrapper() and the
        // first database creation leaves behind: complete rows, no files.
        let ik = crate::providers::utils::db_path_integration_keys(&user_id);
        let ck = crate::providers::utils::db_path_tier3_cookies(&user_id);
        for p in [&ik, &ck] {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
            }
        }

        do_login(&app, "Alice", "pw")
            .await
            .expect("account must still unlock");
        assert!(
            ik.exists() && ck.exists(),
            "unlock must recreate the first databases"
        );
    }

    #[tokio::test]
    async fn an_account_with_no_wrapper_is_refused_neutrally_and_not_counted_as_a_failure() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();

        user_store::create_user(
            &_env.pool,
            "legacy-user",
            "Legacy",
            "admin",
            true,
            b"salt1234salt1234",
            1024,
            1,
            1,
            &[0u8; 32],
        )
        .await
        .unwrap();

        let result = do_login(&app, "Legacy", "anything").await;
        assert_eq!(
            result,
            Err(
                "This account was created before the data-key format and can't be opened"
                    .to_owned()
            )
        );
        assert!(!registry.is_occupied().await);
        let mut conn = _env.pool.acquire().await.unwrap();
        let failures: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM auth_failures")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(
            failures.0, 0,
            "a pre-wrapper account is not a wrong password"
        );
    }

    #[tokio::test]
    async fn a_changed_key_epoch_refuses_store_opens() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        do_login(&app, "Alice", "pw").await.unwrap();
        let (dek, user_id) = registry
            .with_key(|k| (k.data_key, k.user_id.clone()))
            .await
            .unwrap();
        let hex = key_hex(&dek);

        crate::persistence::integration_keys_store::open_integration_keys_db(&user_id, &hex)
            .await
            .expect("opens while the epoch matches");

        // Another OS session rotates: the stored epoch moves on.
        sqlx::query("UPDATE user_key_wrappers SET key_epoch = 2 WHERE user_id = ?")
            .bind(&user_id)
            .execute(&_env.pool)
            .await
            .unwrap();

        let err =
            crate::persistence::integration_keys_store::open_integration_keys_db(&user_id, &hex)
                .await
                .err()
                .expect("a stale epoch must refuse the open");
        assert!(
            matches!(
                err,
                crate::persistence::integration_keys_store::IntegrationKeysStoreError::KeyEpoch(_)
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn no_key_material_reaches_logs_or_ipc_responses() {
        let _ = log::set_logger(&CaptureLogger);
        log::set_max_level(log::LevelFilter::Trace);

        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let password = "pw-with-a-distinct-marker-9f3a";

        let create = do_login(&app, "Alice", password).await;
        let (dek, user_id) = registry
            .with_key(|k| (k.data_key, k.user_id.clone()))
            .await
            .unwrap();
        let display = get_recovery_key_display(registry.clone()).await.unwrap();
        let entropy = data_key::entropy_from_mnemonic(&display.mnemonic).unwrap();
        let (salt, mem, iter, par) = user_store::get_salt_params(&_env.pool, &user_id)
            .await
            .unwrap()
            .unwrap();
        let pk = kdf::derive_password_key(password.as_bytes(), &salt, mem, iter, par).unwrap();
        let wrapper = user_store::get_key_wrapper(&_env.pool, &user_id)
            .await
            .unwrap()
            .unwrap();

        registry.clear().await;
        let wrong = do_login(&app, "Alice", "not-the-password").await;
        let unlock = do_login(&app, "Alice", password).await;
        let session = get_session(registry.clone(), app.state::<sqlx::SqlitePool>())
            .await
            .unwrap();

        let secrets = [
            ("dek", hex_of(&dek)),
            ("password key", hex_of(&pk)),
            ("recovery entropy", hex_of(entropy.as_slice())),
            ("mnemonic", display.mnemonic.clone()),
            ("password", password.to_owned()),
        ];

        // IPC: what login/get_session return, plus the serialized session.
        let ipc = format!(
            "{create:?} {wrong:?} {unlock:?} {}",
            serde_json::to_string(&session).unwrap()
        );
        for (name, secret) in &secrets {
            assert!(
                !ipc.contains(secret.as_str()),
                "{name} leaked into an IPC response"
            );
        }
        // The one response that carries the phrase carries nothing else.
        let display_json = serde_json::to_string(&display).unwrap();
        for (name, secret) in secrets.iter().filter(|(n, _)| *n != "mnemonic") {
            assert!(
                !display_json.contains(secret.as_str()),
                "{name} leaked into the phrase response"
            );
        }

        // Logs and Debug output of every stored/derived type.
        let logs = CAPTURED_LOGS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .join("\n");
        let debug = format!("{wrapper:?}");
        for (name, secret) in &secrets {
            assert!(
                !logs.contains(secret.as_str()),
                "{name} leaked into the logs: {:?}",
                logs.lines().find(|l| l.contains(secret.as_str()))
            );
            assert!(
                !debug.contains(secret.as_str()),
                "{name} leaked into Debug output"
            );
        }
    }

    #[tokio::test]
    async fn get_session_with_no_resident_key_returns_ok_none() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let _group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        let result = get_session(registry.clone(), pool.clone()).await;
        assert_eq!(result, Ok(None));
    }

    #[tokio::test]
    async fn get_session_after_login_returns_session_info() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "correct horse battery staple".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();

        let session = get_session(registry.clone(), pool.clone()).await.unwrap();
        assert!(session.is_some());
        let session = session.unwrap();
        assert_eq!(session.display_name, "Alice");
        assert_eq!(session.role, "admin");
        assert!(session.is_primary);

        let user = user_store::find_user_by_display_name(&pool, "Alice")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.user_id, user.id);
    }

    #[tokio::test]
    async fn get_session_after_logout_reverts_to_none() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();
        assert!(get_session(registry.clone(), pool.clone())
            .await
            .unwrap()
            .is_some());

        logout(
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
            app.state::<crate::provider_credentials::session::ConnectSessions>(),
        )
        .await
        .unwrap();

        let result = get_session(registry.clone(), pool.clone()).await;
        assert_eq!(result, Ok(None));
    }

    // -- group-key rehydration (items.id=290, decisions.id=718) --------------

    #[tokio::test]
    async fn login_rehydrates_group_key_registry_from_personal_db() {
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();

        let user = user_store::find_user_by_display_name(&pool, "Alice")
            .await
            .unwrap()
            .unwrap();
        let personal_key_hex = registry.personal_key_hex().await.unwrap();

        let persona_id = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(
            &pool,
            &persona_id,
            "Test Persona",
            "personal",
            &user.id,
            None,
        )
        .await
        .expect("create_persona must succeed");

        let group_id = "group-rehydrate-1";
        let group_key_hex = "11".repeat(kdf::MASTER_KEY_LEN);
        group_key_store::save_group_key(
            &user.id,
            &persona_id,
            &personal_key_hex,
            group_id,
            &group_key_hex,
            &crate::providers::utils::now(),
        )
        .await
        .expect("save_group_key must succeed");

        assert!(
            !group_key_registry.is_occupied(&persona_id, group_id).await,
            "sanity check: nothing populated the registry yet"
        );

        // Simulate a process restart -- KeyRegistry loses its resident key,
        // GroupKeyRegistry is untouched by anything in this test (it was
        // never populated), matching a fresh process's empty state.
        registry.clear().await;

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .expect("second login must succeed");

        assert!(
            group_key_registry.is_occupied(&persona_id, group_id).await,
            "finish_login must rehydrate the durable group_keys row into GroupKeyRegistry"
        );
        let rehydrated_hex = group_key_registry
            .key_hex_for(&persona_id, group_id)
            .await
            .unwrap();
        assert_eq!(rehydrated_hex, group_key_hex);
    }

    #[tokio::test]
    async fn login_skips_a_malformed_group_keys_row_without_failing() {
        // Q1 (decisions mid-planning, items.id=290): a corrupted personal.db
        // group_keys row must never block login -- account-level lockout
        // over one ancillary row would be a support-ticket bug, not a
        // safety improvement.
        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();

        let user = user_store::find_user_by_display_name(&pool, "Alice")
            .await
            .unwrap()
            .unwrap();
        let personal_key_hex = registry.personal_key_hex().await.unwrap();

        let persona_id = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(
            &pool,
            &persona_id,
            "Test Persona",
            "personal",
            &user.id,
            None,
        )
        .await
        .expect("create_persona must succeed");

        let group_id = "group-malformed-1";
        group_key_store::save_group_key(
            &user.id,
            &persona_id,
            &personal_key_hex,
            group_id,
            "not-valid-hex", // odd length AND non-hex characters
            &crate::providers::utils::now(),
        )
        .await
        .expect(
            "save_group_key must succeed even for a malformed value -- \
                 the store layer doesn't validate, only the rehydration reader does",
        );

        registry.clear().await;

        let result = login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await;

        assert!(
            result.is_ok(),
            "login must succeed even when a group_keys row is malformed: {result:?}"
        );
        assert!(
            !group_key_registry.is_occupied(&persona_id, group_id).await,
            "the malformed row must be skipped, not loaded"
        );
    }

    /// items.id=252: demote_interrupted_runs() (conductor::lifecycle) had
    /// zero call sites before this item -- never invoked from the running
    /// app, so a run left at status='running' by a crash stayed that way
    /// forever. This confirms finish_login()'s new sibling call actually
    /// reaches it: a stale running focus_runs row must come back as
    /// 'paused' after a fresh login(), mirroring
    /// login_rehydrates_group_key_registry_from_personal_db's persona
    /// fixture shape immediately above.
    #[tokio::test]
    async fn login_demotes_a_stale_running_focus_run_to_paused() {
        use sqlx::ConnectOptions;

        let _env = setup().await;
        let app = mock_app_with_registry(_env.pool.clone());
        let registry = app.state::<KeyRegistry>();
        let group_key_registry = app.state::<GroupKeyRegistry>();
        let pool = app.state::<sqlx::SqlitePool>();

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .unwrap();

        let user = user_store::find_user_by_display_name(&pool, "Alice")
            .await
            .unwrap()
            .unwrap();
        let personal_key_hex = registry.personal_key_hex().await.unwrap();

        let persona_id = uuid::Uuid::new_v4().to_string();
        persona_store::create_persona(
            &pool,
            &persona_id,
            "Test Persona",
            "personal",
            &user.id,
            None,
        )
        .await
        .expect("create_persona must succeed");

        crate::persistence::migrations::migrate_outputs_db(
            &user.id,
            &persona_id,
            &personal_key_hex,
        )
        .await
        .expect("outputs.db migration must succeed");

        let db_path = crate::providers::utils::db_path_outputs(&user.id, &persona_id);
        let run_id = uuid::Uuid::new_v4().to_string();
        {
            let outputs_key = crate::auth::subkeys::test_key_hex(
                &user.id,
                &personal_key_hex,
                crate::auth::subkeys::DbPurpose::Outputs,
                &persona_id,
            );
            let mut conn =
                crate::providers::utils::connect_options_encrypted(&db_path, &outputs_key)
                    .create_if_missing(false)
                    .connect()
                    .await
                    .expect("must reopen outputs.db under its key");
            // started_at is far enough in the past to be stale under any
            // QR_INTERRUPT_THRESHOLD_MINUTES value -- no env var needed.
            sqlx::query(
                "INSERT INTO focus_runs (id, focus_id, status, started_at)
                 VALUES (?, 'test-focus', 'running', '2020-01-01T00:00:00Z')",
            )
            .bind(&run_id)
            .execute(&mut conn)
            .await
            .expect("seeding a stale focus_runs row must succeed");
        }

        // Simulate a process restart -- KeyRegistry loses its resident key,
        // matching the empty state finish_login's recovery sweep must run
        // against at next login.
        registry.clear().await;

        login(
            "Alice".to_owned(),
            "password123".to_owned(),
            registry.clone(),
            group_key_registry.clone(),
            pool.clone(),
        )
        .await
        .expect("second login must succeed");

        let mut conn = crate::providers::utils::connect_options_encrypted(
            &db_path,
            &crate::auth::subkeys::test_key_hex(
                &user.id,
                &personal_key_hex,
                crate::auth::subkeys::DbPurpose::Outputs,
                &persona_id,
            ),
        )
        .create_if_missing(false)
        .connect()
        .await
        .expect("must reopen outputs.db under its key");
        let status: String = sqlx::query_scalar("SELECT status FROM focus_runs WHERE id = ?")
            .bind(&run_id)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(
            status, "paused",
            "finish_login must demote a stale running focus_run to paused"
        );
    }
}

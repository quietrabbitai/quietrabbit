// src-tauri/src/auth/user_store.rs
//
// User account CRUD against shared.db's users/user_salts tables (items.id=
// 205). Mirrors persistence/persona_store.rs's connection pattern
// (pooled per items.id=483, SAVEPOINT-atomic multi-table writes,
// sqlx::query() runtime style) -- callers pass a &sqlx::SqlitePool in.
//
// SCOPE BOUNDARY, deliberate: this module creates a user + salt row ONLY.
// It does not create a persona, does not run any per-persona DB migration,
// and is never called with persona-creation logic inside it. Onboarding
// design (a separate, later, larger piece of work -- Jason's direction,
// 2026-08-01) may decide the first-run flow should also create a default
// persona; if so, that's an ADDITIVE call from login()'s bootstrap branch
// to persona_store::create_persona() (which already exists), not a change
// to this module. Keeping create_user() single-purpose is what makes that
// addition clean later rather than requiring this module to be reworked.
//
// NO PASSWORD VERIFIER STORED HERE, deliberately: since items.id=629
// (decisions.id=893) the password check is the authentication tag of the
// DEK password wrap (auth::data_key), stored in user_key_wrappers by
// create_user_with_wrapper() below. There is still no separate
// password_hash. login() detects a tag failure and writes it to
// auth_failures.
//
// LOGIN IDENTIFIER: display_name is used as the lookup key for a login
// attempt. This is a SCHEMA-DERIVED inference, not a documented
// architectural decision -- Architecture/AUTH_MULTIUSER_ARCHITECTURE.md
// never discusses login-identifier mechanics at all (verified via direct
// search this session, zero hits for display_name/username/login
// identifier in that document). display_name is simply the only UNIQUE,
// human-facing field on the users table, making it the only viable
// candidate today. If onboarding design later introduces a separate
// login-identifier concept, this is the function to revisit.

use sqlx::Row;
use thiserror::Error;

use crate::auth::data_key::KeyWrapper;

#[derive(Debug, Error)]
pub enum UserStoreError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("User '{0}' already exists")]
    AlreadyExists(String),
    #[error("Stored salt for user '{0}' is not valid hex and cannot be decoded")]
    CorruptSalt(String),
    #[error("Stored KDF parameter for user '{0}' is out of range: {1}")]
    CorruptKdfParams(String, String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Stored key wrapper for user '{0}' is malformed")]
    CorruptWrapper(String),
    /// The account's key epoch is not the one the caller expected, or the
    /// rotation it was acting on is no longer the active one: the key was
    /// rotated elsewhere. Compare-and-set lost.
    #[error("the account's key epoch changed")]
    EpochChanged,
    /// A rotation is already active for this account.
    #[error("a key rotation is already in progress for this account")]
    RotationInProgress,
    #[error("key rotation '{0}' not found")]
    RotationNotFound(String),
    /// The rotation is in a state that does not allow this step.
    #[error("key rotation '{0}' is in state '{1}', which does not allow this step")]
    RotationWrongState(String, String),
    /// Promotion was asked for while some files are not yet on the new key.
    #[error("{0} file(s) are not yet re-keyed")]
    RotationItemsIncomplete(u64),
    #[error("stored key rotation '{0}' is malformed")]
    CorruptRotation(String),
    #[error("expected row missing in {0}")]
    MissingRow(&'static str),
}

#[derive(Debug, Clone)]
pub struct UserRecord {
    pub id: String,
    pub display_name: String,
    pub role: String,
    pub is_primary: bool,
    pub idle_timeout_minutes: i64,
}

/// True if any user row exists at all -- the "fresh install" check
/// login()'s bootstrap branch uses to decide whether to create the primary
/// admin (Section 2.3: "First run: create the primary admin user...").
pub async fn has_any_users(pool: &sqlx::SqlitePool) -> Result<bool, UserStoreError> {
    let mut conn = pool.acquire().await?;
    let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM users LIMIT 1")
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.is_some())
}

/// Fetch a user by display_name. See module header on why display_name is
/// the login identifier.
pub async fn find_user_by_display_name(
    pool: &sqlx::SqlitePool,
    display_name: &str,
) -> Result<Option<UserRecord>, UserStoreError> {
    let mut conn = pool.acquire().await?;
    let row = sqlx::query(
        "SELECT id, display_name, role, is_primary, idle_timeout_minutes
         FROM users WHERE display_name = ?",
    )
    .bind(display_name)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|r| UserRecord {
        id: r.get("id"),
        display_name: r.get("display_name"),
        role: r.get("role"),
        is_primary: r.get::<i64, _>("is_primary") != 0,
        idle_timeout_minutes: r.get("idle_timeout_minutes"),
    }))
}

/// Fetch a user by id. Mirrors find_user_by_display_name's row shape --
/// used by commands::auth::get_session to resolve the resident
/// KeyRegistry.user_id (a raw id, not a display_name) into the fields the
/// frontend session response needs.
pub async fn find_user_by_id(
    pool: &sqlx::SqlitePool,
    user_id: &str,
) -> Result<Option<UserRecord>, UserStoreError> {
    let mut conn = pool.acquire().await?;
    let row = sqlx::query(
        "SELECT id, display_name, role, is_primary, idle_timeout_minutes
         FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|r| UserRecord {
        id: r.get("id"),
        display_name: r.get("display_name"),
        role: r.get("role"),
        is_primary: r.get::<i64, _>("is_primary") != 0,
        idle_timeout_minutes: r.get("idle_timeout_minutes"),
    }))
}

/// Decode a hex string into bytes, failing on any malformed input rather
/// than silently dropping unparseable pairs. Authentication material must
/// fail loudly on corruption, not partially decode (corrected this
/// session, per external review -- an earlier draft used filter_map,
/// which would silently truncate malformed hex instead of erroring).
fn hex_decode(user_id: &str, s: &str) -> Result<Vec<u8>, UserStoreError> {
    if !s.len().is_multiple_of(2) {
        return Err(UserStoreError::CorruptSalt(user_id.to_owned()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| UserStoreError::CorruptSalt(user_id.to_owned()))
        })
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Read a user's stored salt + Argon2id parameters, for deriving the
/// master key at login. Returns None if the user has no salt row (should
/// not happen for any user created via create_user(), but checked
/// explicitly rather than assumed).
pub async fn get_salt_params(
    pool: &sqlx::SqlitePool,
    user_id: &str,
) -> Result<Option<(Vec<u8>, u32, u32, u32)>, UserStoreError> {
    let mut conn = pool.acquire().await?;
    let row = sqlx::query(
        "SELECT salt_hex, kdf_memory_kib, kdf_iterations, kdf_parallelism
         FROM user_salts WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;

    let Some(row) = row else { return Ok(None) };

    let salt_hex: String = row.get("salt_hex");
    let salt = hex_decode(user_id, &salt_hex)?;

    let to_u32 = |field: &str, value: i64| -> Result<u32, UserStoreError> {
        u32::try_from(value)
            .map_err(|_| UserStoreError::CorruptKdfParams(user_id.to_owned(), field.to_owned()))
    };
    let memory_kib = to_u32("kdf_memory_kib", row.get("kdf_memory_kib"))?;
    let iterations = to_u32("kdf_iterations", row.get("kdf_iterations"))?;
    let parallelism = to_u32("kdf_parallelism", row.get("kdf_parallelism"))?;

    Ok(Some((salt, memory_kib, iterations, parallelism)))
}

/// Create a new user + their salt row, atomically (SAVEPOINT, mirrors
/// persona_store::create_persona's pattern -- on failure, ROLLBACK TO
/// then RELEASE, so the pooled connection is returned with no transaction
/// open).
///
/// auth_enabled is hardcoded to 1 here, not exposed as a parameter --
/// Section 2.1 reframes it as an onboarding-time invariant evaluated once
/// at account creation (not a standing runtime toggle), so every account
/// this function creates satisfies that invariant by construction.
///
/// #[allow(clippy::too_many_arguments)] justification (items.id=207): 8
/// params, all distinct account-creation fields with no natural grouping;
/// bundling into a struct would be a real API change touching
/// commands/auth.rs's call site, out of scope for a lint-silencing pass.
/// Revisit if a struct-shaped caller emerges naturally (e.g. an onboarding
/// request type).
///
/// sharing_public_key (items.id=289, decisions.id=677): the account's
/// X25519 sharing public key, written into user_sharing_keys inside this
/// same SAVEPOINT -- same atomicity guarantee the users/user_salts pair
/// already gets. Not secret (shared.db is unencrypted, instance-wide); the
/// matching private key is never written anywhere -- see
/// auth::sharing_keypair's module header.
///
/// NO KEY WRAPPER (items.id=629): this variant writes none, so the account
/// it creates cannot be unlocked by login(). It exists for the many tests
/// that need a user row without real key material. Production account
/// creation uses create_user_with_wrapper().
#[allow(clippy::too_many_arguments)]
pub async fn create_user(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    display_name: &str,
    role: &str,
    is_primary: bool,
    salt: &[u8],
    kdf_memory_kib: u32,
    kdf_iterations: u32,
    kdf_parallelism: u32,
    sharing_public_key: &[u8; 32],
) -> Result<(), UserStoreError> {
    create_user_inner(
        pool,
        user_id,
        display_name,
        role,
        is_primary,
        salt,
        kdf_memory_kib,
        kdf_iterations,
        kdf_parallelism,
        sharing_public_key,
        None,
    )
    .await
}

/// create_user() plus the account's key wrapper (items.id=629,
/// decisions.id=893), all in the one SAVEPOINT: users, user_salts,
/// user_sharing_keys and user_key_wrappers either all exist or none do, so
/// an interrupted write leaves no half-made account.
#[allow(clippy::too_many_arguments)]
pub async fn create_user_with_wrapper(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    display_name: &str,
    role: &str,
    is_primary: bool,
    salt: &[u8],
    kdf_memory_kib: u32,
    kdf_iterations: u32,
    kdf_parallelism: u32,
    sharing_public_key: &[u8; 32],
    wrapper: &KeyWrapper,
) -> Result<(), UserStoreError> {
    create_user_inner(
        pool,
        user_id,
        display_name,
        role,
        is_primary,
        salt,
        kdf_memory_kib,
        kdf_iterations,
        kdf_parallelism,
        sharing_public_key,
        Some(wrapper),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn create_user_inner(
    pool: &sqlx::SqlitePool,
    user_id: &str,
    display_name: &str,
    role: &str,
    is_primary: bool,
    salt: &[u8],
    kdf_memory_kib: u32,
    kdf_iterations: u32,
    kdf_parallelism: u32,
    sharing_public_key: &[u8; 32],
    wrapper: Option<&KeyWrapper>,
) -> Result<(), UserStoreError> {
    let created_at = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;

    sqlx::query("SAVEPOINT create_user")
        .execute(&mut *conn)
        .await?;

    let step: Result<(), sqlx::Error> = async {
        sqlx::query(
            "INSERT INTO users
             (id, display_name, role, is_primary, auth_enabled, created_at)
             VALUES (?, ?, ?, ?, 1, ?)",
        )
        .bind(user_id)
        .bind(display_name)
        .bind(role)
        .bind(is_primary as i64)
        .bind(&created_at)
        .execute(&mut *conn)
        .await?;

        sqlx::query(
            "INSERT INTO user_salts
             (user_id, salt_hex, kdf_algorithm, kdf_memory_kib,
              kdf_iterations, kdf_parallelism, created_at)
             VALUES (?, ?, 'argon2id', ?, ?, ?, ?)",
        )
        .bind(user_id)
        .bind(hex_encode(salt))
        .bind(kdf_memory_kib)
        .bind(kdf_iterations)
        .bind(kdf_parallelism)
        .bind(&created_at)
        .execute(&mut *conn)
        .await?;

        sqlx::query(
            "INSERT INTO user_sharing_keys
             (user_id, public_key_hex, created_at)
             VALUES (?, ?, ?)",
        )
        .bind(user_id)
        .bind(hex_encode(sharing_public_key))
        .bind(&created_at)
        .execute(&mut *conn)
        .await?;

        if let Some(w) = wrapper {
            sqlx::query(
                "INSERT INTO user_key_wrappers
                 (user_id, key_epoch, format_version, password_wrap_hex,
                  recovery_public_key_hex, recovery_seal_hex,
                  created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(user_id)
            .bind(w.key_epoch as i64)
            .bind(i64::from(w.format_version))
            .bind(hex_encode(&w.password_wrap))
            .bind(hex_encode(&w.recovery_public_key))
            .bind(hex_encode(&w.recovery_seal))
            .bind(&created_at)
            .bind(&created_at)
            .execute(&mut *conn)
            .await?;
        }

        Ok(())
    }
    .await;

    match step {
        Ok(()) => {
            sqlx::query("RELEASE create_user")
                .execute(&mut *conn)
                .await?;
            Ok(())
        }
        Err(e) => {
            // ROLLBACK TO alone undoes the writes but leaves the savepoint --
            // and the transaction it opened, with its write lock -- open.
            // The connection goes back to the pool, so without the RELEASE
            // a failed creation would leave shared.db locked for every
            // later writer (found by items.id=629's interrupted-write test).
            let _ = sqlx::query("ROLLBACK TO create_user")
                .execute(&mut *conn)
                .await;
            let _ = sqlx::query("RELEASE create_user").execute(&mut *conn).await;
            // Structured constraint detection (DatabaseError::is_unique_violation),
            // not string-matching -- corrected this session per external
            // review; verified sqlx exposes this method directly rather
            // than assumed.
            if let sqlx::Error::Database(ref db_err) = e {
                if db_err.is_unique_violation() {
                    return Err(UserStoreError::AlreadyExists(display_name.to_owned()));
                }
            }
            Err(UserStoreError::Database(e))
        }
    }
}

/// Read an account's key wrapper (user_key_wrappers). `None` for an account
/// created before the data-key format: it cannot be unlocked.
pub async fn get_key_wrapper(
    pool: &sqlx::SqlitePool,
    user_id: &str,
) -> Result<Option<KeyWrapper>, UserStoreError> {
    let mut conn = pool.acquire().await?;
    let row = sqlx::query(
        "SELECT key_epoch, format_version, password_wrap_hex,
                recovery_public_key_hex, recovery_seal_hex
         FROM user_key_wrappers WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else { return Ok(None) };

    let corrupt = || UserStoreError::CorruptWrapper(user_id.to_owned());
    let key_epoch = u64::try_from(row.get::<i64, _>("key_epoch")).map_err(|_| corrupt())?;
    let format_version =
        u8::try_from(row.get::<i64, _>("format_version")).map_err(|_| corrupt())?;
    let decode = |col: &str| -> Result<Vec<u8>, UserStoreError> {
        hex_decode(user_id, &row.get::<String, _>(col)).map_err(|_| corrupt())
    };
    let recovery_public_key: [u8; 32] = decode("recovery_public_key_hex")?
        .try_into()
        .map_err(|_| corrupt())?;

    Ok(Some(KeyWrapper {
        key_epoch,
        format_version,
        password_wrap: decode("password_wrap_hex")?,
        recovery_public_key,
        recovery_seal: decode("recovery_seal_hex")?,
    }))
}

/// The account's current key epoch, or None if it has no wrapper row. Read
/// by auth::epoch_guard on every store open.
pub async fn get_key_epoch(
    pool: &sqlx::SqlitePool,
    user_id: &str,
) -> Result<Option<u64>, UserStoreError> {
    let mut conn = pool.acquire().await?;
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT key_epoch FROM user_key_wrappers WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&mut *conn)
            .await?;
    row.map(|(e,)| u64::try_from(e).map_err(|_| UserStoreError::CorruptWrapper(user_id.to_owned())))
        .transpose()
}

// ---------------------------------------------------------------------------
// Key rotation state (items.id=630, decisions.id=893)
// ---------------------------------------------------------------------------
//
// Every write below is one SQLite transaction on a pooled connection, run
// inside a SAVEPOINT. A failed step does ROLLBACK TO and then ALWAYS RELEASE,
// so the connection goes back to the pool outside any transaction (ROLLBACK
// TO alone leaves the savepoint, and the write lock, open).

/// What a rotation is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationKind {
    PasswordChange,
    Restore,
    NewPhrase,
}

impl RotationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RotationKind::PasswordChange => "password_change",
            RotationKind::Restore => "restore",
            RotationKind::NewPhrase => "new_phrase",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "password_change" => RotationKind::PasswordChange,
            "restore" => RotationKind::Restore,
            "new_phrase" => RotationKind::NewPhrase,
            _ => return None,
        })
    }
}

/// The Architecture 8.6 state vocabulary, used for a rotation and for each
/// of its items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationState {
    Pending,
    InProgress,
    Verifying,
    Committed,
    RolledBack,
    Failed,
}

impl RotationState {
    pub fn as_str(self) -> &'static str {
        match self {
            RotationState::Pending => "pending",
            RotationState::InProgress => "in_progress",
            RotationState::Verifying => "verifying",
            RotationState::Committed => "committed",
            RotationState::RolledBack => "rolled_back",
            RotationState::Failed => "failed",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => RotationState::Pending,
            "in_progress" => RotationState::InProgress,
            "verifying" => RotationState::Verifying,
            "committed" => RotationState::Committed,
            "rolled_back" => RotationState::RolledBack,
            "failed" => RotationState::Failed,
            _ => return None,
        })
    }
    /// A rotation in a final state no longer holds the account's pending flag.
    pub fn is_final(self) -> bool {
        matches!(self, RotationState::Committed | RotationState::RolledBack)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationDirection {
    Forward,
    Rollback,
}

impl RotationDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            RotationDirection::Forward => "forward",
            RotationDirection::Rollback => "rollback",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "forward" => Some(RotationDirection::Forward),
            "rollback" => Some(RotationDirection::Rollback),
            _ => None,
        }
    }
}

/// One file a rotation re-keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationItem {
    /// Relative to the account's directory (`users/<user_id>/`), `/`-separated.
    pub rel_path: String,
    pub is_blob: bool,
    pub state: RotationState,
    pub bytes: u64,
    pub error: Option<String>,
}

/// Everything a rotation stages before touching any file.
#[derive(Clone)]
pub struct StagedRotation {
    pub rotation_id: String,
    pub user_id: String,
    pub kind: RotationKind,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub old_dek_under_new: Vec<u8>,
    pub salt: Vec<u8>,
    pub kdf_memory_kib: u32,
    pub kdf_iterations: u32,
    pub kdf_parallelism: u32,
    pub format_version: u8,
    pub password_wrap: Vec<u8>,
    pub recovery_public_key: [u8; 32],
    pub recovery_seal: Vec<u8>,
    pub sharing_public_key: [u8; 32],
    pub items: Vec<RotationItem>,
}

impl std::fmt::Debug for StagedRotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedRotation")
            .field("rotation_id", &self.rotation_id)
            .field("kind", &self.kind)
            .field("from_epoch", &self.from_epoch)
            .field("to_epoch", &self.to_epoch)
            .finish_non_exhaustive()
    }
}

/// An active rotation as read back from shared.db. The secret columns are
/// `None` once it has finished.
#[derive(Clone)]
pub struct PendingRotation {
    pub rotation_id: String,
    pub user_id: String,
    pub kind: RotationKind,
    pub state: RotationState,
    pub direction: RotationDirection,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub last_error: Option<String>,
    pub old_dek_under_new: Option<Vec<u8>>,
    pub salt: Option<Vec<u8>>,
    pub kdf_memory_kib: Option<u32>,
    pub kdf_iterations: Option<u32>,
    pub kdf_parallelism: Option<u32>,
    pub format_version: Option<u8>,
    pub password_wrap: Option<Vec<u8>>,
    pub recovery_public_key: Option<[u8; 32]>,
    pub recovery_seal: Option<Vec<u8>>,
    pub sharing_public_key: Option<[u8; 32]>,
}

impl std::fmt::Debug for PendingRotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingRotation")
            .field("rotation_id", &self.rotation_id)
            .field("kind", &self.kind)
            .field("state", &self.state)
            .field("direction", &self.direction)
            .field("from_epoch", &self.from_epoch)
            .field("to_epoch", &self.to_epoch)
            .finish_non_exhaustive()
    }
}

/// The wrapper and the account's pending-rotation flag from ONE select, so a
/// reader cannot see the wrapper of one instant and the flag of another.
#[derive(Debug, Clone)]
pub struct WrapperState {
    pub wrapper: KeyWrapper,
    pub pending_rotation: Option<String>,
}

/// get_key_wrapper plus `pending_rotation` (shared_033.sql).
pub async fn get_key_wrapper_state(
    pool: &sqlx::SqlitePool,
    user_id: &str,
) -> Result<Option<WrapperState>, UserStoreError> {
    let mut conn = pool.acquire().await?;
    let row = sqlx::query(
        "SELECT key_epoch, format_version, password_wrap_hex,
                recovery_public_key_hex, recovery_seal_hex, pending_rotation
         FROM user_key_wrappers WHERE user_id = ?",
    )
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else { return Ok(None) };

    let corrupt = || UserStoreError::CorruptWrapper(user_id.to_owned());
    let key_epoch = u64::try_from(row.get::<i64, _>("key_epoch")).map_err(|_| corrupt())?;
    let format_version =
        u8::try_from(row.get::<i64, _>("format_version")).map_err(|_| corrupt())?;
    let decode = |col: &str| -> Result<Vec<u8>, UserStoreError> {
        hex_decode(user_id, &row.get::<String, _>(col)).map_err(|_| corrupt())
    };
    let recovery_public_key: [u8; 32] = decode("recovery_public_key_hex")?
        .try_into()
        .map_err(|_| corrupt())?;
    Ok(Some(WrapperState {
        wrapper: KeyWrapper {
            key_epoch,
            format_version,
            password_wrap: decode("password_wrap_hex")?,
            recovery_public_key,
            recovery_seal: decode("recovery_seal_hex")?,
        },
        pending_rotation: row.get("pending_rotation"),
    }))
}

/// Undo a savepoint and prove the connection is outside any transaction.
/// Returns false if it could not be made clean.
async fn undo_savepoint(conn: &mut sqlx::SqliteConnection, name: &str) -> bool {
    let rolled = sqlx::query(&format!("ROLLBACK TO {name}"))
        .execute(&mut *conn)
        .await
        .is_ok();
    let released = sqlx::query(&format!("RELEASE {name}"))
        .execute(&mut *conn)
        .await
        .is_ok();
    if !(rolled && released) {
        return false;
    }
    // Authoritative probe: BEGIN fails inside a transaction.
    if sqlx::query("BEGIN").execute(&mut *conn).await.is_err() {
        return false;
    }
    sqlx::query("ROLLBACK").execute(&mut *conn).await.is_ok()
}

/// Finish a SAVEPOINT: RELEASE on success, ROLLBACK TO then RELEASE on error.
/// Takes the pooled connection by value because a connection that cannot be
/// proven to be outside a transaction must never go back to the pool: a
/// failed RELEASE (for example a deferred constraint failing at commit) is
/// undone with ROLLBACK TO and RELEASE, and if THAT fails too the connection
/// is detached from the pool and dropped (closing it, which ends whatever
/// transaction it held).
async fn end_savepoint<T>(
    mut conn: sqlx::pool::PoolConnection<sqlx::Sqlite>,
    name: &str,
    step: Result<T, UserStoreError>,
) -> Result<T, UserStoreError> {
    let outcome = match step {
        Ok(v) => match sqlx::query(&format!("RELEASE {name}"))
            .execute(&mut *conn)
            .await
        {
            Ok(_) => return Ok(v),
            Err(e) => UserStoreError::Database(e),
        },
        Err(e) => e,
    };
    if !undo_savepoint(&mut conn, name).await {
        drop(conn.detach());
    }
    Err(outcome)
}

/// Stage a rotation: the rotation row, its items and the account's
/// `pending_rotation` flag, in one transaction. Compare-and-set: the flag is
/// only set if the account is still at `from_epoch` with no active rotation.
/// From the end of this call an interrupted rotation is resumable.
pub async fn stage_rotation(
    pool: &sqlx::SqlitePool,
    staged: &StagedRotation,
) -> Result<(), UserStoreError> {
    let now = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;
    sqlx::query("SAVEPOINT stage_rotation")
        .execute(&mut *conn)
        .await?;

    let step: Result<(), UserStoreError> = async {
        let flagged = sqlx::query(
            "UPDATE user_key_wrappers SET pending_rotation = ?, updated_at = ?
             WHERE user_id = ? AND key_epoch = ? AND pending_rotation IS NULL",
        )
        .bind(&staged.rotation_id)
        .bind(&now)
        .bind(&staged.user_id)
        .bind(staged.from_epoch as i64)
        .execute(&mut *conn)
        .await?;
        if flagged.rows_affected() != 1 {
            let current: Option<(i64, Option<String>)> = sqlx::query_as(
                "SELECT key_epoch, pending_rotation FROM user_key_wrappers WHERE user_id = ?",
            )
            .bind(&staged.user_id)
            .fetch_optional(&mut *conn)
            .await?;
            return Err(match current {
                Some((_, Some(_))) => UserStoreError::RotationInProgress,
                _ => UserStoreError::EpochChanged,
            });
        }

        sqlx::query(
            "INSERT INTO user_key_rotations
             (rotation_id, user_id, kind, direction, state, from_epoch, to_epoch,
              started_at, old_dek_under_new_hex, staged_salt_hex,
              staged_kdf_memory_kib, staged_kdf_iterations, staged_kdf_parallelism,
              staged_format_version, staged_password_wrap_hex,
              staged_recovery_public_key_hex, staged_recovery_seal_hex,
              staged_sharing_public_key_hex)
             VALUES (?, ?, ?, 'forward', 'pending', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&staged.rotation_id)
        .bind(&staged.user_id)
        .bind(staged.kind.as_str())
        .bind(staged.from_epoch as i64)
        .bind(staged.to_epoch as i64)
        .bind(&now)
        .bind(hex_encode(&staged.old_dek_under_new))
        .bind(hex_encode(&staged.salt))
        .bind(staged.kdf_memory_kib)
        .bind(staged.kdf_iterations)
        .bind(staged.kdf_parallelism)
        .bind(i64::from(staged.format_version))
        .bind(hex_encode(&staged.password_wrap))
        .bind(hex_encode(&staged.recovery_public_key))
        .bind(hex_encode(&staged.recovery_seal))
        .bind(hex_encode(&staged.sharing_public_key))
        .execute(&mut *conn)
        .await?;

        for item in &staged.items {
            sqlx::query(
                "INSERT INTO user_key_rotation_items
                 (rotation_id, rel_path, kind, state, bytes)
                 VALUES (?, ?, ?, 'pending', ?)",
            )
            .bind(&staged.rotation_id)
            .bind(&item.rel_path)
            .bind(if item.is_blob { "blob" } else { "db" })
            .bind(item.bytes as i64)
            .execute(&mut *conn)
            .await?;
        }
        Ok(())
    }
    .await;

    end_savepoint(conn, "stage_rotation", step).await
}

fn decode_opt(rid: &str, v: Option<String>) -> Result<Option<Vec<u8>>, UserStoreError> {
    v.map(|h| hex_decode(rid, &h).map_err(|_| UserStoreError::CorruptRotation(rid.to_owned())))
        .transpose()
}

fn row_to_rotation(row: &sqlx::sqlite::SqliteRow) -> Result<PendingRotation, UserStoreError> {
    let rid: String = row.get("rotation_id");
    let corrupt = || UserStoreError::CorruptRotation(rid.clone());
    let key32 = |v: Option<Vec<u8>>| -> Result<Option<[u8; 32]>, UserStoreError> {
        v.map(|b| <[u8; 32]>::try_from(b).map_err(|_| corrupt()))
            .transpose()
    };
    let opt_u32 = |col: &str| -> Result<Option<u32>, UserStoreError> {
        row.get::<Option<i64>, _>(col)
            .map(|n| u32::try_from(n).map_err(|_| corrupt()))
            .transpose()
    };
    Ok(PendingRotation {
        user_id: row.get("user_id"),
        kind: RotationKind::parse(&row.get::<String, _>("kind")).ok_or_else(corrupt)?,
        state: RotationState::parse(&row.get::<String, _>("state")).ok_or_else(corrupt)?,
        direction: RotationDirection::parse(&row.get::<String, _>("direction"))
            .ok_or_else(corrupt)?,
        from_epoch: u64::try_from(row.get::<i64, _>("from_epoch")).map_err(|_| corrupt())?,
        to_epoch: u64::try_from(row.get::<i64, _>("to_epoch")).map_err(|_| corrupt())?,
        last_error: row.get("last_error"),
        old_dek_under_new: decode_opt(&rid, row.get("old_dek_under_new_hex"))?,
        salt: decode_opt(&rid, row.get("staged_salt_hex"))?,
        kdf_memory_kib: opt_u32("staged_kdf_memory_kib")?,
        kdf_iterations: opt_u32("staged_kdf_iterations")?,
        kdf_parallelism: opt_u32("staged_kdf_parallelism")?,
        format_version: row
            .get::<Option<i64>, _>("staged_format_version")
            .map(|n| u8::try_from(n).map_err(|_| corrupt()))
            .transpose()?,
        password_wrap: decode_opt(&rid, row.get("staged_password_wrap_hex"))?,
        recovery_public_key: key32(decode_opt(&rid, row.get("staged_recovery_public_key_hex"))?)?,
        recovery_seal: decode_opt(&rid, row.get("staged_recovery_seal_hex"))?,
        sharing_public_key: key32(decode_opt(&rid, row.get("staged_sharing_public_key_hex"))?)?,
        rotation_id: rid,
    })
}

const ROTATION_COLUMNS: &str = "rotation_id, user_id, kind, direction, state, from_epoch,
    to_epoch, last_error, old_dek_under_new_hex, staged_salt_hex, staged_kdf_memory_kib,
    staged_kdf_iterations, staged_kdf_parallelism, staged_format_version,
    staged_password_wrap_hex, staged_recovery_public_key_hex, staged_recovery_seal_hex,
    staged_sharing_public_key_hex";

/// The account's active rotation (not committed, not rolled back), if any.
pub async fn get_pending_rotation(
    pool: &sqlx::SqlitePool,
    user_id: &str,
) -> Result<Option<PendingRotation>, UserStoreError> {
    let mut conn = pool.acquire().await?;
    let row = sqlx::query(&format!(
        "SELECT {ROTATION_COLUMNS} FROM user_key_rotations
         WHERE user_id = ? AND state NOT IN ('committed', 'rolled_back')"
    ))
    .bind(user_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(row_to_rotation).transpose()
}

/// A rotation by id, active or finished (finished ones have no secrets).
pub async fn get_rotation(
    pool: &sqlx::SqlitePool,
    rotation_id: &str,
) -> Result<Option<PendingRotation>, UserStoreError> {
    let mut conn = pool.acquire().await?;
    let row = sqlx::query(&format!(
        "SELECT {ROTATION_COLUMNS} FROM user_key_rotations WHERE rotation_id = ?"
    ))
    .bind(rotation_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(row_to_rotation).transpose()
}

/// A rotation's items in a stable order.
pub async fn list_rotation_items(
    pool: &sqlx::SqlitePool,
    rotation_id: &str,
) -> Result<Vec<RotationItem>, UserStoreError> {
    let mut conn = pool.acquire().await?;
    let rows = sqlx::query(
        "SELECT rel_path, kind, state, bytes, error FROM user_key_rotation_items
         WHERE rotation_id = ? ORDER BY rowid",
    )
    .bind(rotation_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            let corrupt = || UserStoreError::CorruptRotation(rotation_id.to_owned());
            Ok(RotationItem {
                rel_path: r.get("rel_path"),
                is_blob: r.get::<String, _>("kind") == "blob",
                state: RotationState::parse(&r.get::<String, _>("state")).ok_or_else(corrupt)?,
                bytes: u64::try_from(r.get::<i64, _>("bytes")).map_err(|_| corrupt())?,
                error: r.get("error"),
            })
        })
        .collect()
}

/// Set one item's state. A missing item is `MissingRow`.
pub async fn set_rotation_item_state(
    pool: &sqlx::SqlitePool,
    rotation_id: &str,
    rel_path: &str,
    state: RotationState,
    error: Option<&str>,
) -> Result<(), UserStoreError> {
    let mut conn = pool.acquire().await?;
    let done = sqlx::query(
        "UPDATE user_key_rotation_items SET state = ?, error = ?
         WHERE rotation_id = ? AND rel_path = ?",
    )
    .bind(state.as_str())
    .bind(error)
    .bind(rotation_id)
    .bind(rel_path)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() != 1 {
        return Err(UserStoreError::MissingRow("user_key_rotation_items"));
    }
    Ok(())
}

/// Set a rotation's progress state and direction. Refuses a finished
/// rotation, and `verifying` is only reached through promote_rotation.
pub async fn set_rotation_progress(
    pool: &sqlx::SqlitePool,
    rotation_id: &str,
    state: RotationState,
    direction: RotationDirection,
    last_error: Option<&str>,
) -> Result<(), UserStoreError> {
    if matches!(
        state,
        RotationState::Verifying | RotationState::Committed | RotationState::RolledBack
    ) {
        return Err(UserStoreError::RotationWrongState(
            rotation_id.to_owned(),
            state.as_str().to_owned(),
        ));
    }
    let mut conn = pool.acquire().await?;
    let done = sqlx::query(
        "UPDATE user_key_rotations SET state = ?, direction = ?, last_error = ?
         WHERE rotation_id = ? AND state NOT IN ('verifying', 'committed', 'rolled_back')",
    )
    .bind(state.as_str())
    .bind(direction.as_str())
    .bind(last_error)
    .bind(rotation_id)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() != 1 {
        return Err(UserStoreError::RotationWrongState(
            rotation_id.to_owned(),
            "not active or already promoted".to_owned(),
        ));
    }
    Ok(())
}

/// Promotion step 1: copy the staged values into the live wrapper, the
/// salt/KDF row and the published sharing public key, and move the rotation
/// to `verifying`, all in one transaction. Compare-and-set on the account's
/// epoch and pending flag. Staging and the pending flag are KEPT until
/// finish_rotation. Refused unless the rotation is going forward and every
/// item is `committed`. Idempotent: an already-`verifying` rotation is a no-op.
/// This is the "update the wrapper in one transaction, compare-and-set on the
/// expected epoch" write.
pub async fn promote_rotation(
    pool: &sqlx::SqlitePool,
    rotation_id: &str,
    expected_from_epoch: u64,
) -> Result<(), UserStoreError> {
    let now = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;
    sqlx::query("SAVEPOINT promote_rotation")
        .execute(&mut *conn)
        .await?;

    let step: Result<(), UserStoreError> = async {
        let row = sqlx::query(&format!(
            "SELECT {ROTATION_COLUMNS} FROM user_key_rotations WHERE rotation_id = ?"
        ))
        .bind(rotation_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| UserStoreError::RotationNotFound(rotation_id.to_owned()))?;
        let rot = row_to_rotation(&row)?;

        if rot.state == RotationState::Verifying {
            return Ok(());
        }
        if rot.state.is_final() {
            return Err(UserStoreError::RotationWrongState(
                rotation_id.to_owned(),
                rot.state.as_str().to_owned(),
            ));
        }
        if rot.from_epoch != expected_from_epoch {
            return Err(UserStoreError::EpochChanged);
        }
        // Promotion makes the new wraps live, which is only right when every
        // file is already on the new key and the rotation is going forward.
        if rot.direction != RotationDirection::Forward {
            return Err(UserStoreError::RotationWrongState(
                rotation_id.to_owned(),
                "rolling back".to_owned(),
            ));
        }
        let (unfinished,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM user_key_rotation_items
             WHERE rotation_id = ? AND state <> 'committed'",
        )
        .bind(rotation_id)
        .fetch_one(&mut *conn)
        .await?;
        if unfinished > 0 {
            return Err(UserStoreError::RotationItemsIncomplete(unfinished as u64));
        }
        let corrupt = || UserStoreError::CorruptRotation(rotation_id.to_owned());
        let (
            Some(salt),
            Some(mem),
            Some(iter),
            Some(par),
            Some(fmt),
            Some(wrap),
            Some(rpk),
            Some(seal),
            Some(spk),
        ) = (
            rot.salt.as_ref(),
            rot.kdf_memory_kib,
            rot.kdf_iterations,
            rot.kdf_parallelism,
            rot.format_version,
            rot.password_wrap.as_ref(),
            rot.recovery_public_key.as_ref(),
            rot.recovery_seal.as_ref(),
            rot.sharing_public_key.as_ref(),
        )
        else {
            return Err(corrupt());
        };

        let wrapper = sqlx::query(
            "UPDATE user_key_wrappers
             SET key_epoch = ?, format_version = ?, password_wrap_hex = ?,
                 recovery_public_key_hex = ?, recovery_seal_hex = ?, updated_at = ?
             WHERE user_id = ? AND key_epoch = ? AND pending_rotation = ?",
        )
        .bind(rot.to_epoch as i64)
        .bind(i64::from(fmt))
        .bind(hex_encode(wrap))
        .bind(hex_encode(rpk))
        .bind(hex_encode(seal))
        .bind(&now)
        .bind(&rot.user_id)
        .bind(rot.from_epoch as i64)
        .bind(rotation_id)
        .execute(&mut *conn)
        .await?;
        if wrapper.rows_affected() != 1 {
            return Err(UserStoreError::EpochChanged);
        }

        let salts = sqlx::query(
            "UPDATE user_salts
             SET salt_hex = ?, kdf_memory_kib = ?, kdf_iterations = ?, kdf_parallelism = ?
             WHERE user_id = ?",
        )
        .bind(hex_encode(salt))
        .bind(mem)
        .bind(iter)
        .bind(par)
        .bind(&rot.user_id)
        .execute(&mut *conn)
        .await?;
        if salts.rows_affected() != 1 {
            return Err(UserStoreError::MissingRow("user_salts"));
        }

        let sharing = sqlx::query(
            "UPDATE user_sharing_keys SET public_key_hex = ?, key_epoch = ?, rotated_at = ?
             WHERE user_id = ?",
        )
        .bind(hex_encode(spk))
        .bind(rot.to_epoch as i64)
        .bind(&now)
        .bind(&rot.user_id)
        .execute(&mut *conn)
        .await?;
        if sharing.rows_affected() != 1 {
            return Err(UserStoreError::MissingRow("user_sharing_keys"));
        }

        sqlx::query("UPDATE user_key_rotations SET state = 'verifying' WHERE rotation_id = ?")
            .bind(rotation_id)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }
    .await;

    end_savepoint(conn, "promote_rotation", step).await
}

/// Promotion step 2, only after the read-back unlock succeeded: drop the
/// staging and the wrapped old DEK, clear the account's pending flag and mark
/// the rotation committed. Idempotent for an already-committed rotation.
pub async fn finish_rotation(
    pool: &sqlx::SqlitePool,
    rotation_id: &str,
) -> Result<(), UserStoreError> {
    let now = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;
    sqlx::query("SAVEPOINT finish_rotation")
        .execute(&mut *conn)
        .await?;

    let step: Result<(), UserStoreError> = async {
        let (user_id, state): (String, String) =
            sqlx::query_as("SELECT user_id, state FROM user_key_rotations WHERE rotation_id = ?")
                .bind(rotation_id)
                .fetch_optional(&mut *conn)
                .await?
                .ok_or_else(|| UserStoreError::RotationNotFound(rotation_id.to_owned()))?;
        match RotationState::parse(&state) {
            Some(RotationState::Committed) => return Ok(()),
            Some(RotationState::Verifying) => {}
            _ => {
                return Err(UserStoreError::RotationWrongState(
                    rotation_id.to_owned(),
                    state,
                ))
            }
        }
        clear_rotation(
            &mut conn,
            rotation_id,
            &user_id,
            RotationState::Committed,
            &now,
            None,
        )
        .await
    }
    .await;

    end_savepoint(conn, "finish_rotation", step).await
}

async fn clear_rotation(
    conn: &mut sqlx::SqliteConnection,
    rotation_id: &str,
    user_id: &str,
    final_state: RotationState,
    now: &str,
    last_error: Option<&str>,
) -> Result<(), UserStoreError> {
    sqlx::query(
        "UPDATE user_key_rotations
         SET state = ?, direction = 'forward', finished_at = ?, last_error = ?,
             old_dek_under_new_hex = NULL, staged_salt_hex = NULL,
             staged_kdf_memory_kib = NULL, staged_kdf_iterations = NULL,
             staged_kdf_parallelism = NULL, staged_format_version = NULL,
             staged_password_wrap_hex = NULL, staged_recovery_public_key_hex = NULL,
             staged_recovery_seal_hex = NULL, staged_sharing_public_key_hex = NULL
         WHERE rotation_id = ?",
    )
    .bind(final_state.as_str())
    .bind(now)
    .bind(last_error)
    .bind(rotation_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE user_key_wrappers SET pending_rotation = NULL, updated_at = ?
         WHERE user_id = ? AND pending_rotation = ?",
    )
    .bind(now)
    .bind(user_id)
    .bind(rotation_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// End a rotation that is NOT being promoted. `RolledBack` clears the staging
/// and the pending flag (allowed only before promotion, while the live wraps
/// are still the old ones). `Failed` only records the error and keeps
/// everything so the rotation can be resumed.
pub async fn abort_rotation(
    pool: &sqlx::SqlitePool,
    rotation_id: &str,
    final_state: RotationState,
    last_error: Option<&str>,
) -> Result<(), UserStoreError> {
    if !matches!(
        final_state,
        RotationState::RolledBack | RotationState::Failed
    ) {
        return Err(UserStoreError::RotationWrongState(
            rotation_id.to_owned(),
            final_state.as_str().to_owned(),
        ));
    }
    let now = crate::providers::utils::now();
    let mut conn = pool.acquire().await?;
    sqlx::query("SAVEPOINT abort_rotation")
        .execute(&mut *conn)
        .await?;

    let step: Result<(), UserStoreError> = async {
        let (user_id, state): (String, String) =
            sqlx::query_as("SELECT user_id, state FROM user_key_rotations WHERE rotation_id = ?")
                .bind(rotation_id)
                .fetch_optional(&mut *conn)
                .await?
                .ok_or_else(|| UserStoreError::RotationNotFound(rotation_id.to_owned()))?;
        if matches!(
            RotationState::parse(&state),
            Some(RotationState::Verifying | RotationState::Committed | RotationState::RolledBack)
        ) {
            return Err(UserStoreError::RotationWrongState(
                rotation_id.to_owned(),
                state,
            ));
        }
        if final_state == RotationState::Failed {
            sqlx::query(
                "UPDATE user_key_rotations SET state = 'failed', last_error = ?
                 WHERE rotation_id = ?",
            )
            .bind(last_error)
            .bind(rotation_id)
            .execute(&mut *conn)
            .await?;
            return Ok(());
        }
        clear_rotation(
            &mut conn,
            rotation_id,
            &user_id,
            final_state,
            &now,
            last_error,
        )
        .await
    }
    .await;

    end_savepoint(conn, "abort_rotation", step).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serialize all env-mutating tests -- Rust test runner is multi-threaded,
    // and QR_DATA_ROOT is process-global state. Shared across every test
    // module that mutates QR_DATA_ROOT (items.id=205, 2026-08-01) -- see
    // test_support.rs for why this must be one true mutex, not a per-module
    // copy (this file previously had its own local static, which raced
    // against providers::utils's and commands::auth's own local copies
    // under parallel test execution; fixed this session).
    use crate::test_support::ENV_MUTEX;

    /// Sets QR_DATA_ROOT to a fresh temp directory, migrates the shared
    /// schema into it (via the real migrations::migrate_shared_db() path,
    /// same one production startup uses), and returns the TempDir --
    /// caller must keep it alive for the duration of the test (dropping it
    /// deletes the directory). Restores the prior QR_DATA_ROOT on scope
    /// exit via the returned guard.
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

    #[tokio::test]
    async fn fresh_db_has_no_users() {
        let _env = setup().await;
        let pool = &_env.pool;
        assert!(!has_any_users(pool,).await.unwrap());
    }

    #[tokio::test]
    async fn create_user_then_has_any_users_is_true() {
        let _env = setup().await;
        let pool = &_env.pool;
        create_user(
            pool,
            "u1",
            "Alice",
            "admin",
            true,
            b"salt1234",
            1024,
            1,
            1,
            &[0u8; 32],
        )
        .await
        .expect("create_user should succeed");
        assert!(has_any_users(pool,).await.unwrap());
    }

    #[tokio::test]
    async fn find_user_by_display_name_round_trips() {
        let _env = setup().await;
        let pool = &_env.pool;
        create_user(
            pool,
            "u1",
            "Alice",
            "admin",
            true,
            b"salt1234",
            1024,
            1,
            1,
            &[0u8; 32],
        )
        .await
        .unwrap();

        let found = find_user_by_display_name(pool, "Alice").await.unwrap();
        assert!(found.is_some());
        let record = found.unwrap();
        assert_eq!(record.id, "u1");
        assert_eq!(record.role, "admin");
        assert!(record.is_primary);
        assert_eq!(record.idle_timeout_minutes, 15, "schema default must apply");
    }

    #[tokio::test]
    async fn find_user_by_display_name_returns_none_when_absent() {
        let _env = setup().await;
        let pool = &_env.pool;
        let found = find_user_by_display_name(pool, "Nobody").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn find_user_by_id_round_trips() {
        let _env = setup().await;
        let pool = &_env.pool;
        create_user(
            pool,
            "u1",
            "Alice",
            "admin",
            true,
            b"salt1234",
            1024,
            1,
            1,
            &[0u8; 32],
        )
        .await
        .unwrap();

        let found = find_user_by_id(pool, "u1").await.unwrap();
        assert!(found.is_some());
        let record = found.unwrap();
        assert_eq!(record.display_name, "Alice");
        assert_eq!(record.role, "admin");
        assert!(record.is_primary);
    }

    #[tokio::test]
    async fn find_user_by_id_returns_none_when_absent() {
        let _env = setup().await;
        let pool = &_env.pool;
        let found = find_user_by_id(pool, "nonexistent").await.unwrap();
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn create_user_duplicate_display_name_is_already_exists() {
        let _env = setup().await;
        let pool = &_env.pool;
        create_user(
            pool,
            "u1",
            "Alice",
            "admin",
            true,
            b"salt1234",
            1024,
            1,
            1,
            &[0u8; 32],
        )
        .await
        .unwrap();

        let result = create_user(
            pool,
            "u2",
            "Alice",
            "user",
            false,
            b"salt5678",
            1024,
            1,
            1,
            &[0u8; 32],
        )
        .await;
        assert!(matches!(result, Err(UserStoreError::AlreadyExists(_))));
    }

    #[tokio::test]
    async fn create_user_second_primary_fails() {
        // shared_001.sql's idx_users_single_primary partial unique index
        // must reject a second is_primary=true row -- confirms create_user
        // doesn't accidentally bypass that constraint.
        let _env = setup().await;
        let pool = &_env.pool;
        create_user(
            pool,
            "u1",
            "Alice",
            "admin",
            true,
            b"salt1234",
            1024,
            1,
            1,
            &[0u8; 32],
        )
        .await
        .unwrap();

        let result = create_user(
            pool,
            "u2",
            "Bob",
            "admin",
            true,
            b"salt5678",
            1024,
            1,
            1,
            &[0u8; 32],
        )
        .await;
        assert!(
            result.is_err(),
            "a second is_primary=true user must be rejected"
        );
    }

    #[tokio::test]
    async fn get_salt_params_round_trips() {
        let _env = setup().await;
        let pool = &_env.pool;
        create_user(
            pool,
            "u1",
            "Alice",
            "admin",
            true,
            &[0xDE, 0xAD, 0xBE, 0xEF],
            65536,
            3,
            4,
            &[0u8; 32],
        )
        .await
        .unwrap();

        let params = get_salt_params(pool, "u1").await.unwrap();
        assert!(params.is_some());
        let (salt, mem, iter, par) = params.unwrap();
        assert_eq!(salt, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(mem, 65536);
        assert_eq!(iter, 3);
        assert_eq!(par, 4);
    }

    #[tokio::test]
    async fn get_salt_params_returns_none_for_unknown_user() {
        let _env = setup().await;
        let pool = &_env.pool;
        let params = get_salt_params(pool, "nonexistent").await.unwrap();
        assert!(params.is_none());
    }

    #[test]
    fn hex_decode_round_trips_with_hex_encode() {
        let original = vec![0x00, 0xFF, 0x42, 0xAB, 0xCD];
        let encoded = hex_encode(&original);
        let decoded = hex_decode("test-user", &encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn hex_decode_rejects_odd_length() {
        let result = hex_decode("test-user", "abc");
        assert!(matches!(result, Err(UserStoreError::CorruptSalt(_))));
    }

    #[test]
    fn hex_decode_rejects_non_hex_characters() {
        // Regression test for the silent-truncation bug found this session
        // (external review) -- must error, not silently drop the bad pair.
        let result = hex_decode("test-user", "abxxcd");
        assert!(matches!(result, Err(UserStoreError::CorruptSalt(_))));
    }

    #[test]
    fn hex_decode_empty_string_is_empty_vec_not_error() {
        let result = hex_decode("test-user", "").unwrap();
        assert_eq!(result, Vec::<u8>::new());
    }

    // -- key rotation state (items.id=630) ----------------------------------

    use crate::auth::data_key;
    use sqlx::sqlite::SqlitePoolOptions;

    const PK1: [u8; 32] = [0x11u8; 32];

    /// A user with a real epoch-1 wrapper, plus a StagedRotation to epoch 2.
    async fn user_with_staged(
        pool: &sqlx::SqlitePool,
        rid: &str,
    ) -> (data_key::NewAccountKeyMaterial, StagedRotation) {
        let m = data_key::create_account_key_material("u1", &PK1).unwrap();
        create_user_with_wrapper(
            pool,
            "u1",
            "Alice",
            "admin",
            true,
            b"oldsalt1",
            1024,
            1,
            1,
            &[0xAAu8; 32],
            &m.wrapper,
        )
        .await
        .unwrap();
        let new_dek = data_key::generate_dek().unwrap();
        let new_pk = [0x22u8; 32];
        let (wrap, seal) = data_key::build_wrapper_parts(
            "u1",
            &new_dek,
            &new_pk,
            &m.wrapper.recovery_public_key,
            2,
        )
        .unwrap();
        let staged = StagedRotation {
            rotation_id: rid.to_owned(),
            user_id: "u1".to_owned(),
            kind: RotationKind::PasswordChange,
            from_epoch: 1,
            to_epoch: 2,
            old_dek_under_new: data_key::wrap_old_dek_under_new(&new_dek, &m.dek, "u1", 1, 2, rid)
                .unwrap(),
            salt: b"newsalt2".to_vec(),
            kdf_memory_kib: 2048,
            kdf_iterations: 2,
            kdf_parallelism: 1,
            format_version: data_key::WRAP_FORMAT_VERSION,
            password_wrap: wrap,
            recovery_public_key: m.wrapper.recovery_public_key,
            recovery_seal: seal,
            sharing_public_key: [0xBBu8; 32],
            items: vec![
                RotationItem {
                    rel_path: "integration_keys.db".into(),
                    is_blob: false,
                    state: RotationState::Pending,
                    bytes: 10,
                    error: None,
                },
                RotationItem {
                    rel_path: "personas/p1/ingested_documents/o1/v1.enc".into(),
                    is_blob: true,
                    state: RotationState::Pending,
                    bytes: 20,
                    error: None,
                },
            ],
        };
        (m, staged)
    }

    /// Mark every item of a rotation committed (what the engine does as it
    /// re-keys), so promotion is allowed.
    async fn commit_all_items(pool: &sqlx::SqlitePool, rid: &str) {
        for item in list_rotation_items(pool, rid).await.unwrap() {
            set_rotation_item_state(pool, rid, &item.rel_path, RotationState::Committed, None)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn staging_sets_the_flag_and_round_trips() {
        let env = setup().await;
        let (_m, staged) = user_with_staged(&env.pool, "rot-1").await;
        stage_rotation(&env.pool, &staged).await.unwrap();

        let state = get_key_wrapper_state(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.pending_rotation.as_deref(), Some("rot-1"));
        assert_eq!(state.wrapper.key_epoch, 1, "staging changes no live wrap");

        let pending = get_pending_rotation(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.rotation_id, "rot-1");
        assert_eq!(pending.kind, RotationKind::PasswordChange);
        assert_eq!(pending.state, RotationState::Pending);
        assert_eq!(pending.direction, RotationDirection::Forward);
        assert_eq!((pending.from_epoch, pending.to_epoch), (1, 2));
        assert_eq!(
            pending.password_wrap.as_deref(),
            Some(&staged.password_wrap[..])
        );
        assert_eq!(pending.sharing_public_key, Some([0xBBu8; 32]));
        assert_eq!(
            pending.old_dek_under_new.as_deref(),
            Some(&staged.old_dek_under_new[..])
        );

        let items = list_rotation_items(&env.pool, "rot-1").await.unwrap();
        assert_eq!(items.len(), 2);
        assert!(!items[0].is_blob && items[1].is_blob);
        assert_eq!(items[1].bytes, 20);
    }

    #[tokio::test]
    async fn staging_is_compare_and_set() {
        let env = setup().await;
        let (_m, staged) = user_with_staged(&env.pool, "rot-1").await;
        stage_rotation(&env.pool, &staged).await.unwrap();

        let mut second = staged.clone();
        second.rotation_id = "rot-2".into();
        assert!(matches!(
            stage_rotation(&env.pool, &second).await,
            Err(UserStoreError::RotationInProgress)
        ));
        // The loser left nothing behind.
        assert!(get_rotation(&env.pool, "rot-2").await.unwrap().is_none());

        abort_rotation(&env.pool, "rot-1", RotationState::RolledBack, None)
            .await
            .unwrap();
        let mut stale = staged.clone();
        stale.rotation_id = "rot-3".into();
        stale.from_epoch = 5;
        stale.to_epoch = 6;
        assert!(matches!(
            stage_rotation(&env.pool, &stale).await,
            Err(UserStoreError::EpochChanged)
        ));
    }

    #[tokio::test]
    async fn promotion_swaps_every_live_value_atomically_and_keeps_staging() {
        let env = setup().await;
        let (_m, staged) = user_with_staged(&env.pool, "rot-1").await;
        stage_rotation(&env.pool, &staged).await.unwrap();
        commit_all_items(&env.pool, "rot-1").await;
        promote_rotation(&env.pool, "rot-1", 1).await.unwrap();

        let state = get_key_wrapper_state(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.wrapper.key_epoch, 2);
        assert_eq!(state.wrapper.password_wrap, staged.password_wrap);
        assert_eq!(state.wrapper.recovery_seal, staged.recovery_seal);
        assert_eq!(
            state.pending_rotation.as_deref(),
            Some("rot-1"),
            "flag kept"
        );
        let salts = get_salt_params(&env.pool, "u1").await.unwrap().unwrap();
        assert_eq!(salts, (b"newsalt2".to_vec(), 2048, 2, 1));
        assert_eq!(
            crate::auth::sharing_keypair::get_public_key(&env.pool, "u1")
                .await
                .unwrap(),
            Some([0xBBu8; 32])
        );
        let (epoch, rotated): (i64, Option<String>) = sqlx::query_as(
            "SELECT key_epoch, rotated_at FROM user_sharing_keys WHERE user_id = 'u1'",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(epoch, 2);
        assert!(rotated.is_some());

        let pending = get_pending_rotation(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.state, RotationState::Verifying);
        assert!(
            pending.old_dek_under_new.is_some(),
            "staging kept until finish"
        );

        // Idempotent: a second promotion changes nothing and does not error.
        promote_rotation(&env.pool, "rot-1", 1).await.unwrap();
    }

    #[tokio::test]
    async fn promotion_with_a_stale_epoch_changes_nothing() {
        let env = setup().await;
        let (m, staged) = user_with_staged(&env.pool, "rot-1").await;
        stage_rotation(&env.pool, &staged).await.unwrap();
        assert!(matches!(
            promote_rotation(&env.pool, "rot-1", 7).await,
            Err(UserStoreError::EpochChanged)
        ));
        let state = get_key_wrapper_state(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.wrapper, m.wrapper);
        assert_eq!(
            get_pending_rotation(&env.pool, "u1")
                .await
                .unwrap()
                .unwrap()
                .state,
            RotationState::Pending
        );
    }

    #[tokio::test]
    async fn finish_clears_every_secret_and_the_flag() {
        let env = setup().await;
        let (_m, staged) = user_with_staged(&env.pool, "rot-1").await;
        stage_rotation(&env.pool, &staged).await.unwrap();
        assert!(
            matches!(
                finish_rotation(&env.pool, "rot-1").await,
                Err(UserStoreError::RotationWrongState(..))
            ),
            "cannot finish before promotion"
        );
        commit_all_items(&env.pool, "rot-1").await;
        promote_rotation(&env.pool, "rot-1", 1).await.unwrap();
        finish_rotation(&env.pool, "rot-1").await.unwrap();

        let state = get_key_wrapper_state(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.pending_rotation, None);
        assert_eq!(state.wrapper.key_epoch, 2);
        assert!(get_pending_rotation(&env.pool, "u1")
            .await
            .unwrap()
            .is_none());
        let done = get_rotation(&env.pool, "rot-1").await.unwrap().unwrap();
        assert_eq!(done.state, RotationState::Committed);
        assert!(done.old_dek_under_new.is_none());
        assert!(done.password_wrap.is_none() && done.recovery_seal.is_none());
        assert!(done.salt.is_none() && done.sharing_public_key.is_none());
        let finished: (Option<String>,) = sqlx::query_as(
            "SELECT finished_at FROM user_key_rotations WHERE rotation_id = 'rot-1'",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert!(finished.0.is_some());
        finish_rotation(&env.pool, "rot-1").await.unwrap();

        // A new rotation may start once the last one is final.
        let mut next = staged.clone();
        next.rotation_id = "rot-2".into();
        next.from_epoch = 2;
        next.to_epoch = 3;
        stage_rotation(&env.pool, &next).await.unwrap();
    }

    #[tokio::test]
    async fn abort_rolls_back_before_promotion_only_and_failed_keeps_the_row_active() {
        let env = setup().await;
        let (m, staged) = user_with_staged(&env.pool, "rot-1").await;
        stage_rotation(&env.pool, &staged).await.unwrap();

        abort_rotation(&env.pool, "rot-1", RotationState::Failed, Some("disk full"))
            .await
            .unwrap();
        let failed = get_pending_rotation(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.state, RotationState::Failed);
        assert_eq!(failed.last_error.as_deref(), Some("disk full"));
        assert!(
            failed.old_dek_under_new.is_some(),
            "a failed rotation stays resumable"
        );

        abort_rotation(
            &env.pool,
            "rot-1",
            RotationState::RolledBack,
            Some("undone"),
        )
        .await
        .unwrap();
        let state = get_key_wrapper_state(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.pending_rotation, None);
        assert_eq!(
            state.wrapper, m.wrapper,
            "rollback leaves the live wraps untouched"
        );
        let done = get_rotation(&env.pool, "rot-1").await.unwrap().unwrap();
        assert_eq!(done.state, RotationState::RolledBack);
        assert!(done.old_dek_under_new.is_none());

        // After promotion a rollback is refused: the live wraps are new.
        let mut next = staged.clone();
        next.rotation_id = "rot-2".into();
        stage_rotation(&env.pool, &next).await.unwrap();
        commit_all_items(&env.pool, "rot-2").await;
        promote_rotation(&env.pool, "rot-2", 1).await.unwrap();
        assert!(matches!(
            abort_rotation(&env.pool, "rot-2", RotationState::RolledBack, None).await,
            Err(UserStoreError::RotationWrongState(..))
        ));
    }

    #[tokio::test]
    async fn item_and_progress_updates_respect_their_guards() {
        let env = setup().await;
        let (_m, staged) = user_with_staged(&env.pool, "rot-1").await;
        stage_rotation(&env.pool, &staged).await.unwrap();

        set_rotation_item_state(
            &env.pool,
            "rot-1",
            "integration_keys.db",
            RotationState::Committed,
            None,
        )
        .await
        .unwrap();
        let items = list_rotation_items(&env.pool, "rot-1").await.unwrap();
        assert_eq!(items[0].state, RotationState::Committed);
        assert_eq!(items[1].state, RotationState::Pending);
        assert!(matches!(
            set_rotation_item_state(&env.pool, "rot-1", "nope.db", RotationState::Failed, None)
                .await,
            Err(UserStoreError::MissingRow(_))
        ));

        set_rotation_progress(
            &env.pool,
            "rot-1",
            RotationState::InProgress,
            RotationDirection::Rollback,
            None,
        )
        .await
        .unwrap();
        let p = get_pending_rotation(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (p.state, p.direction),
            (RotationState::InProgress, RotationDirection::Rollback)
        );
        // verifying is only reachable through promote_rotation.
        assert!(set_rotation_progress(
            &env.pool,
            "rot-1",
            RotationState::Verifying,
            RotationDirection::Forward,
            None
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn a_failed_promotion_leaves_the_pooled_connection_outside_any_transaction() {
        let env = setup().await;
        let (m, staged) = user_with_staged(&env.pool, "rot-1").await;
        stage_rotation(&env.pool, &staged).await.unwrap();
        commit_all_items(&env.pool, "rot-1").await;
        // Make the promotion fail after it has already written the wrapper row.
        sqlx::query("DELETE FROM user_salts WHERE user_id = 'u1'")
            .execute(&env.pool)
            .await
            .unwrap();

        // One connection only: the failed call's connection is the one reused.
        let single = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            promote_rotation(&single, "rot-1", 1).await,
            Err(UserStoreError::MissingRow("user_salts"))
        ));
        // The wrapper write was rolled back with the rest of the savepoint.
        let state = get_key_wrapper_state(&single, "u1").await.unwrap().unwrap();
        assert_eq!(state.wrapper, m.wrapper);
        // And the connection is not left inside a transaction: BEGIN works.
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&single)
            .await
            .unwrap();
        sqlx::query("ROLLBACK").execute(&single).await.unwrap();
    }

    #[tokio::test]
    async fn rotation_secrets_never_appear_in_debug_output() {
        let env = setup().await;
        let (_m, staged) = user_with_staged(&env.pool, "rot-1").await;
        let dbg = format!("{staged:?}");
        assert!(!dbg.contains(&hex_encode(&staged.password_wrap)));
        stage_rotation(&env.pool, &staged).await.unwrap();
        let pending = get_pending_rotation(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert!(!format!("{pending:?}").contains(&hex_encode(&staged.password_wrap)));
    }

    #[tokio::test]
    async fn promotion_is_refused_while_any_item_is_not_committed() {
        let env = setup().await;
        let (m, staged) = user_with_staged(&env.pool, "rot-1").await;
        stage_rotation(&env.pool, &staged).await.unwrap();
        assert!(matches!(
            promote_rotation(&env.pool, "rot-1", 1).await,
            Err(UserStoreError::RotationItemsIncomplete(2))
        ));
        set_rotation_item_state(
            &env.pool,
            "rot-1",
            "integration_keys.db",
            RotationState::Committed,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            promote_rotation(&env.pool, "rot-1", 1).await,
            Err(UserStoreError::RotationItemsIncomplete(1))
        ));
        // Verifying/rolled-back items do not count as committed either.
        set_rotation_item_state(
            &env.pool,
            "rot-1",
            "personas/p1/ingested_documents/o1/v1.enc",
            RotationState::Verifying,
            None,
        )
        .await
        .unwrap();
        assert!(promote_rotation(&env.pool, "rot-1", 1).await.is_err());

        let state = get_key_wrapper_state(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.wrapper, m.wrapper, "nothing was promoted");
        assert_eq!(
            get_pending_rotation(&env.pool, "u1")
                .await
                .unwrap()
                .unwrap()
                .state,
            RotationState::Pending
        );
    }

    #[tokio::test]
    async fn promotion_is_refused_unless_the_direction_is_forward() {
        let env = setup().await;
        let (m, staged) = user_with_staged(&env.pool, "rot-1").await;
        stage_rotation(&env.pool, &staged).await.unwrap();
        commit_all_items(&env.pool, "rot-1").await;
        set_rotation_progress(
            &env.pool,
            "rot-1",
            RotationState::InProgress,
            RotationDirection::Rollback,
            None,
        )
        .await
        .unwrap();
        assert!(matches!(
            promote_rotation(&env.pool, "rot-1", 1).await,
            Err(UserStoreError::RotationWrongState(..))
        ));
        let state = get_key_wrapper_state(&env.pool, "u1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.wrapper, m.wrapper);

        // Back to forward, the same rotation promotes.
        set_rotation_progress(
            &env.pool,
            "rot-1",
            RotationState::InProgress,
            RotationDirection::Forward,
            None,
        )
        .await
        .unwrap();
        promote_rotation(&env.pool, "rot-1", 1).await.unwrap();
    }

    /// A pool whose single connection is the one every call reuses, so a
    /// connection wrongly returned inside a transaction shows up at once.
    async fn single_connection_pool() -> sqlx::SqlitePool {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(crate::providers::utils::connect_options_unencrypted(
                &crate::providers::utils::db_path_shared(),
            ))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_failed_release_is_undone_and_the_connection_goes_back_clean() {
        let env = setup().await;
        let single = single_connection_pool().await;
        let mut conn = single.acquire().await.unwrap();
        sqlx::query("SAVEPOINT t")
            .execute(&mut *conn)
            .await
            .unwrap();
        // A deferred foreign key violation makes the outermost RELEASE (the
        // commit) fail.
        sqlx::query("PRAGMA defer_foreign_keys = ON")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO user_key_rotation_items (rotation_id, rel_path, kind)
             VALUES ('no-such-rotation', 'x.db', 'db')",
        )
        .execute(&mut *conn)
        .await
        .unwrap();

        let result = end_savepoint(conn, "t", Ok(())).await;
        assert!(
            matches!(result, Err(UserStoreError::Database(_))),
            "{result:?}"
        );

        // Same (only) connection: outside a transaction, the bad row gone.
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&single)
            .await
            .unwrap();
        sqlx::query("ROLLBACK").execute(&single).await.unwrap();
        let (n,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM user_key_rotation_items WHERE rotation_id = 'no-such-rotation'",
        )
        .fetch_one(&single)
        .await
        .unwrap();
        assert_eq!(n, 0);
        drop(env);
    }

    #[tokio::test]
    async fn a_connection_that_cannot_be_cleaned_is_detached_not_returned_to_the_pool() {
        let env = setup().await;
        let single = single_connection_pool().await;
        let mut conn = single.acquire().await.unwrap();
        // Open a real transaction that the savepoint name below does not
        // belong to, so ROLLBACK TO fails and the connection stays dirty.
        sqlx::query("BEGIN").execute(&mut *conn).await.unwrap();
        assert_eq!(single.size(), 1);

        let result: Result<(), _> =
            end_savepoint(conn, "never_opened", Err(UserStoreError::EpochChanged)).await;
        assert!(matches!(result, Err(UserStoreError::EpochChanged)));

        // The dirty connection left the pool (and was closed): the pool has
        // none, and a fresh one works and is outside any transaction.
        assert_eq!(single.size(), 0, "the dirty connection must be gone");
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&single)
            .await
            .unwrap();
        sqlx::query("ROLLBACK").execute(&single).await.unwrap();
        drop(env);
    }
}

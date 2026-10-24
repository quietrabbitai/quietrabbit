// src-tauri/src/auth/key_rotation_io.rs
//
// File-level primitives for data-key rotation (items.id=630, stage B): what
// there is to re-key, how one database is re-keyed, how a database is
// verified. No rotation state and no account logic -- auth::key_rotation (the
// engine, stage C) drives these.
//
// INVENTORY: the account's directory (`users/<user_id>/`) is walked on disk,
// not read from shared.db, whose topic index is documented as a discovery
// aid rather than the truth. Every file is classified by its position in the
// tree. A `*.db` or `*.enc` that is not a known database or blob is a hard
// stop (UnknownEncryptedFile): rotating around an encrypted file this code
// does not understand would leave it readable by the old password.
//
// THE REKEY PRIMITIVE (evidence: items.id=630 step 0, SQLCipher 4.18.0 on
// SQLite 3.53.4, `kill -9` at 13 instants in each of WAL and DELETE mode):
//   - PRAGMA rekey works on a WAL database with a live -wal, but writes every
//     page into the WAL, so the main file keeps old-key pages until a
//     checkpoint. rekey_db_file therefore checkpoints (TRUNCATE) and switches
//     the file to journal_mode=DELETE first, which makes the rekey a single-
//     file rollback-journal transaction.
//   - A crash during that transaction leaves the file openable under exactly
//     one of the old or new key, never both and never neither, and probing it
//     with the wrong key first is harmless (db_key_state).
//   - A stale -wal or -journal left beside a database of a different key
//     silently corrupts it when replayed. rekey_db_file refuses to finish
//     with a sidecar present and verify_db_key runs cipher_integrity_check so
//     such damage is found, not served.
//   - sqlcipher_export was rejected: it does not carry user_version, needs the
//     sidecars handled by hand and twice the disk, and is no faster.
// The openers set journal_mode=WAL again on the next ordinary open.

use std::path::{Path, PathBuf};

use sqlx::{ConnectOptions, Connection, Row, SqliteConnection};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::auth::kdf;
use crate::auth::subkeys::{self, DbPurpose, SubkeyError};

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

/// A database's identity: which purpose and scope derive its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbTarget {
    pub purpose: DbPurpose,
    pub scope: String,
}

/// An ingested document blob's identity (the inputs to its key and AAD).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobTarget {
    pub persona_id: String,
    pub output_id: String,
    pub version: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemTarget {
    Db(DbTarget),
    Blob(BlobTarget),
}

/// One file a rotation must re-key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryItem {
    /// Relative to `users/<user_id>/`, `/`-separated. The key in
    /// user_key_rotation_items.
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub target: ItemTarget,
    pub bytes: u64,
}

#[derive(Debug, Default)]
pub struct Inventory {
    /// In re-key order (see `order_rank`).
    pub items: Vec<InventoryItem>,
    /// `<blob>.enc.tmp` left by an interrupted atomic write. Old-key
    /// ciphertext; the rotation deletes them.
    pub stale_tmp: Vec<PathBuf>,
    /// `-wal`, `-shm` and `-journal` files beside known databases.
    pub sidecars: Vec<PathBuf>,
}

impl Inventory {
    pub fn total_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.bytes).sum()
    }
    /// Free space a rotation needs: one database's journal (about its own
    /// size) or one blob's temp copy, at a time.
    pub fn required_free_space(&self) -> u64 {
        self.items.iter().map(|i| i.bytes).max().unwrap_or(0)
    }
}

#[derive(Debug, Error)]
pub enum InventoryError {
    #[error("I/O error reading the account directory: {0}")]
    Io(#[from] std::io::Error),
    /// An encrypted-looking file this code cannot classify. Names the file.
    #[error("unrecognised encrypted file in the account directory: {0}")]
    UnknownEncryptedFile(String),
}

/// What a path (relative components under the account directory) is.
#[derive(Debug, PartialEq, Eq)]
enum Classified {
    Item(ItemTarget),
    StaleTmp,
    Sidecar,
    /// Not encrypted by this app, or not ours: ignored.
    Other,
    /// Looks encrypted but is not recognised.
    Unknown,
}

fn db_name_purpose(name: &str) -> Option<DbPurpose> {
    Some(match name {
        "personal.db" => DbPurpose::Personal,
        "outputs.db" => DbPurpose::Outputs,
        "messages.db" => DbPurpose::Messages,
        _ => return None,
    })
}

fn is_sidecar_of_known_db(name: &str) -> bool {
    ["-wal", "-shm", "-journal"].iter().any(|suffix| {
        name.strip_suffix(suffix).is_some_and(|base| {
            matches!(
                base,
                "personal.db"
                    | "outputs.db"
                    | "messages.db"
                    | "domain_context.db"
                    | "plan_state.db"
                    | "view_cache.db"
                    | "integration_keys.db"
                    | "cloud_chat_cookies.db"
                    | "tier3_cookies.db"
            )
        })
    })
}

fn parse_blob_name(name: &str) -> Option<i32> {
    let digits = name.strip_prefix('v')?.strip_suffix(".enc")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn classify(parts: &[&str]) -> Classified {
    let Some(name) = parts.last().copied() else {
        return Classified::Other;
    };
    if is_sidecar_of_known_db(name) {
        return Classified::Sidecar;
    }
    if let Some(base) = name.strip_suffix(".tmp") {
        if parse_blob_name(base).is_some() {
            return Classified::StaleTmp;
        }
    }
    let item = |t: ItemTarget| Classified::Item(t);
    let db = |purpose, scope: String| item(ItemTarget::Db(DbTarget { purpose, scope }));
    match parts {
        ["integration_keys.db"] => db(
            DbPurpose::IntegrationKeys,
            subkeys::ACCOUNT_SCOPE.to_owned(),
        ),
        ["cloud_chat_cookies.db"] | ["tier3_cookies.db"] => {
            db(DbPurpose::CloudCookies, subkeys::ACCOUNT_SCOPE.to_owned())
        }
        ["personas", pid, file] if db_name_purpose(file).is_some() => {
            db(db_name_purpose(file).expect("checked"), (*pid).to_owned())
        }
        ["personas", pid, "focuses", fid, "domain_context.db"] => match subkeys::scope(&[pid, fid])
        {
            Ok(scope) => db(DbPurpose::DomainContext, scope),
            Err(_) => Classified::Unknown,
        },
        ["personas", pid, "focuses", fid, "topics", tid, "plan_state.db"] => {
            match subkeys::scope(&[pid, fid, tid]) {
                Ok(scope) => db(DbPurpose::PlanState, scope),
                Err(_) => Classified::Unknown,
            }
        }
        ["persona_view_shares", sid, "view_cache.db"] => {
            db(DbPurpose::ViewCache, (*sid).to_owned())
        }
        ["personas", pid, "ingested_documents", oid, file] => match parse_blob_name(file) {
            Some(version) => item(ItemTarget::Blob(BlobTarget {
                persona_id: (*pid).to_owned(),
                output_id: (*oid).to_owned(),
                version,
            })),
            None => Classified::Unknown,
        },
        _ => {
            let encrypted_looking = name.ends_with(".db")
                || name.ends_with(".enc")
                || name.contains(".db-")
                || name.contains(".enc.");
            if encrypted_looking {
                Classified::Unknown
            } else {
                Classified::Other
            }
        }
    }
}

/// The target for a stored `rel_path` (as inventory() produced it), so a
/// resumed rotation can rebuild an item's keys from its row alone.
pub fn target_for_rel_path(rel_path: &str) -> Option<ItemTarget> {
    let parts: Vec<&str> = rel_path.split('/').collect();
    match classify(&parts) {
        Classified::Item(t) => Some(t),
        _ => None,
    }
}

/// Delete any `-wal`, `-shm` and `-journal` beside `path`; returns how many
/// existed. Only correct once the database has been opened and closed under
/// its right key (which consumes a genuine WAL or hot journal): what is left
/// then belongs to no live state. Never call it on a database that has not
/// been through that, or committed data in a real WAL would be discarded.
pub fn remove_leftover_sidecars(path: &Path) -> std::io::Result<usize> {
    let mut removed = 0;
    for sidecar in sidecar_paths(path) {
        match std::fs::remove_file(&sidecar) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(removed)
}

/// Delete a file if it exists; returns whether it did.
pub fn remove_if_exists(path: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Re-key order: account databases, then per persona the three core
/// databases, then focus and topic databases, view caches, and blobs last.
fn order_rank(t: &ItemTarget) -> u8 {
    match t {
        ItemTarget::Db(d) => match d.purpose {
            DbPurpose::IntegrationKeys => 0,
            DbPurpose::CloudCookies => 1,
            DbPurpose::Personal => 2,
            DbPurpose::Outputs => 3,
            DbPurpose::Messages => 4,
            DbPurpose::DomainContext => 5,
            DbPurpose::PlanState => 6,
            DbPurpose::ViewCache => 7,
            DbPurpose::IngestBlob => 8,
        },
        ItemTarget::Blob(_) => 9,
    }
}

fn walk(dir: &Path, rel: &mut Vec<String>, out: &mut Inventory) -> Result<(), InventoryError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        rel.push(name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk(&path, rel, out)?;
        } else if file_type.is_file() {
            let parts: Vec<&str> = rel.iter().map(String::as_str).collect();
            match classify(&parts) {
                Classified::Item(target) => out.items.push(InventoryItem {
                    rel_path: rel.join("/"),
                    abs_path: path,
                    target,
                    bytes: entry.metadata()?.len(),
                }),
                Classified::StaleTmp => out.stale_tmp.push(path),
                Classified::Sidecar => out.sidecars.push(path),
                Classified::Other => {}
                Classified::Unknown => {
                    return Err(InventoryError::UnknownEncryptedFile(rel.join("/")));
                }
            }
        }
        rel.pop();
    }
    Ok(())
}

/// Walk `user_dir` (`<data root>/users/<user_id>`) and list what a rotation
/// must re-key. A missing directory is an empty inventory. Blocking file I/O
/// runs on the blocking pool.
pub async fn inventory(user_dir: &Path) -> Result<Inventory, InventoryError> {
    let user_dir = user_dir.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut inv = Inventory::default();
        if user_dir.is_dir() {
            walk(&user_dir, &mut Vec::new(), &mut inv)?;
        }
        inv.items.sort_by(|a, b| {
            (order_rank(&a.target), &a.rel_path).cmp(&(order_rank(&b.target), &b.rel_path))
        });
        Ok(inv)
    })
    .await
    .map_err(|e| InventoryError::Io(std::io::Error::other(e.to_string())))?
}

/// The SQLCipher key (hex) for a database target, for an explicit DEK and
/// epoch. The engine calls this for both the old and the new key.
pub fn db_key_for(
    target: &DbTarget,
    dek: &[u8; kdf::MASTER_KEY_LEN],
    user_id: &str,
    epoch: u64,
) -> Result<Zeroizing<String>, SubkeyError> {
    let key = subkeys::derive_db_key(dek, target.purpose, &target.scope, user_id, epoch)?;
    Ok(subkeys::encode_hex(key.as_ref()))
}

/// Free bytes on the filesystem holding `path`, or None if it cannot be
/// determined (the caller must then decide; the engine treats None as "not
/// enough information" and says so rather than guessing).
///
/// On unix this asks the kernel about the path itself (statvfs), so it is
/// the path's own filesystem that answers -- tmpfs, a network home, a bind
/// mount, whatever it is. It never consults a mount list and never falls back
/// to a nearby mount's figure: any failure is None (items.id=791; the old
/// sysinfo longest-prefix match skipped tmpfs and NFS/CIFS mounts and so
/// reported the parent filesystem's free space for a data dir on one).
pub fn available_space(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        statvfs_available(path)
    }
    #[cfg(not(unix))]
    {
        sysinfo_available_space(path)
    }
}

/// How many times an EINTR from statvfs is retried before giving up (None).
#[cfg(unix)]
const STATVFS_EINTR_RETRIES: usize = 5;

/// `f_bavail` (blocks available to an unprivileged user, as sysinfo reports)
/// times the fragment size, from one statvfs on `path`.
#[cfg(unix)]
fn statvfs_available(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    for _ in 0..STATVFS_EINTR_RETRIES {
        // SAFETY: `c_path` is a valid NUL-terminated C string that outlives
        // the call, and `stat.as_mut_ptr()` is a valid, writable pointer to a
        // `libc::statvfs`.
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
        if rc == 0 {
            // SAFETY: statvfs returned 0, so it fully initialised `stat`.
            let stat = unsafe { stat.assume_init() };
            // The field widths differ by target (u32 blocks on macOS), so
            // widen before multiplying.
            #[allow(clippy::unnecessary_cast)]
            return bytes_available(
                stat.f_bavail as u64,
                stat.f_frsize as u64,
                stat.f_bsize as u64,
            );
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
    None
}

/// Free bytes from a statvfs result: available blocks times the fragment
/// size (`f_frsize`, falling back to `f_bsize` when a filesystem reports 0).
/// None when no usable block size is known or the product overflows -- numbers
/// that cannot be right are "unknown", never "plenty".
#[cfg(any(unix, test))]
fn bytes_available(bavail: u64, frsize: u64, bsize: u64) -> Option<u64> {
    let block = if frsize != 0 { frsize } else { bsize };
    if block == 0 {
        return None;
    }
    bavail.checked_mul(block)
}

/// Non-unix targets: the longest-prefix sysinfo match this function used
/// everywhere before items.id=791. It can pick a parent volume when sysinfo
/// omits the path's own, and is not fixed here (no Windows build exists to
/// test a replacement on).
#[cfg(not(unix))]
fn sysinfo_available_space(path: &Path) -> Option<u64> {
    let path = std::fs::canonicalize(path).ok()?;
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

// ---------------------------------------------------------------------------
// Re-keying and verifying one database
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum RekeyError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The old key does not open the file.
    #[error("the file does not open under the old key")]
    WrongOldKey,
    /// Another connection holds the file, so it could not be checkpointed.
    #[error("the database is in use by another connection")]
    Busy,
    #[error("could not switch the database to rollback-journal mode (got '{0}')")]
    JournalMode(String),
    /// A -wal or -journal file is still beside the database after the rekey.
    #[error("a journal or WAL file is still present beside the database: {0}")]
    LeftoverSidecar(String),
    #[error("verification failed: {0}")]
    Verification(String),
    /// The file could not be opened for a reason other than the key.
    #[error("the database could not be opened: {0}")]
    Unreadable(String),
}

/// The SQLite primary result code of a database error (the extended code
/// masked to its low byte).
fn primary_code(e: &sqlx::Error) -> Option<i32> {
    e.as_database_error()
        .and_then(|d| d.code())
        .and_then(|c| c.parse::<i32>().ok())
        .map(|c| c & 0xff)
}

const SQLITE_BUSY: i32 = 5;
const SQLITE_LOCKED: i32 = 6;
const SQLITE_NOTADB: i32 = 26;

/// SQLITE_BUSY / SQLITE_LOCKED: another connection holds the file.
fn is_busy(e: &sqlx::Error) -> bool {
    matches!(primary_code(e), Some(SQLITE_BUSY | SQLITE_LOCKED))
}

/// What opening a file under one key came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenProbe {
    Opens,
    /// SQLITE_NOTADB (26): the key is wrong, or the file is damaged, or a
    /// stale sidecar is being replayed over it. The ONLY failure that says
    /// anything about the key.
    NotADatabase,
    /// SQLITE_BUSY / SQLITE_LOCKED: someone else holds the file. Says nothing
    /// about the key and must never lead to a file being moved or deleted.
    Busy,
    /// Anything else (missing file, I/O error, ...), described.
    Failed(String),
}

fn probe_from_error(e: &sqlx::Error) -> OpenProbe {
    match primary_code(e) {
        Some(SQLITE_NOTADB) => OpenProbe::NotADatabase,
        Some(SQLITE_BUSY | SQLITE_LOCKED) => OpenProbe::Busy,
        _ => OpenProbe::Failed(e.to_string()),
    }
}

fn busy_or(e: sqlx::Error) -> RekeyError {
    if is_busy(&e) {
        RekeyError::Busy
    } else {
        RekeyError::Database(e)
    }
}

fn keyed_options_with(
    path: &Path,
    key_hex: &str,
    busy_timeout_ms: u32,
) -> sqlx::sqlite::SqliteConnectOptions {
    crate::providers::utils::quiet_connect_options()
        .filename(path)
        .create_if_missing(false)
        .pragma("key", format!("\"x'{key_hex}'\""))
        .pragma("cipher_compatibility", "4")
        .pragma("busy_timeout", busy_timeout_ms.to_string())
}

fn keyed_options(path: &Path, key_hex: &str) -> sqlx::sqlite::SqliteConnectOptions {
    // Waiting for a lock a long time is only worth it in production.
    let wait = if cfg!(test) { 250 } else { 5000 };
    keyed_options_with(path, key_hex, wait)
}

fn sidecar_paths(path: &Path) -> [PathBuf; 3] {
    ["-wal", "-shm", "-journal"].map(|suffix| {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        PathBuf::from(p)
    })
}

/// Open under `key_hex` and say what happened, keeping "wrong key" apart from
/// "someone else has the file". Probes wait only briefly for a lock.
pub async fn probe_open(path: &Path, key_hex: &str) -> OpenProbe {
    let mut conn = match keyed_options_with(path, key_hex, 250).connect().await {
        Ok(c) => c,
        Err(e) => return probe_from_error(&e),
    };
    let result = sqlx::query("SELECT count(*) FROM sqlite_master")
        .execute(&mut conn)
        .await;
    // Closed, not dropped: the file (and any WAL) must be released before the
    // caller does anything else with it.
    let _ = conn.close().await;
    match result {
        Ok(_) => OpenProbe::Opens,
        Err(e) => probe_from_error(&e),
    }
}

/// Whether the file opens under `key_hex`. Read-only in effect; safe to call
/// with the wrong key on a database left by a crash. Any failure is false;
/// use probe_open when the reason matters.
pub async fn db_opens_under(path: &Path, key_hex: &str) -> bool {
    probe_open(path, key_hex).await == OpenProbe::Opens
}

/// Which key a database file is on, found by probing the file itself (a
/// crash can fall between a rekey and the row that records it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKeyState {
    New,
    Old,
    /// Both keys failed with "not a database" (26), and nothing else.
    Neither,
}

fn probe_error(probe: OpenProbe) -> RekeyError {
    match probe {
        OpenProbe::Busy => RekeyError::Busy,
        OpenProbe::Failed(detail) => RekeyError::Unreadable(detail),
        OpenProbe::Opens | OpenProbe::NotADatabase => {
            unreachable!("not an error probe")
        }
    }
}

/// Probe NEW first, then OLD. A busy or locked file, or any failure other
/// than "not a database", is an ERROR (Busy / Unreadable), never `Neither`:
/// only a clean "not a database" from BOTH keys means the file is on neither.
pub async fn db_key_state(
    path: &Path,
    old_key_hex: &str,
    new_key_hex: &str,
) -> Result<DbKeyState, RekeyError> {
    match probe_open(path, new_key_hex).await {
        OpenProbe::Opens => return Ok(DbKeyState::New),
        OpenProbe::NotADatabase => {}
        other => return Err(probe_error(other)),
    }
    match probe_open(path, old_key_hex).await {
        OpenProbe::Opens => Ok(DbKeyState::Old),
        OpenProbe::NotADatabase => Ok(DbKeyState::Neither),
        other => Err(probe_error(other)),
    }
}

/// A database that does not open under any key, with sidecar files beside it,
/// may be failing BECAUSE of them: SQLite replays any non-empty `-wal` (and
/// rolls back any `-journal`) it finds, so one left over from an earlier life
/// of the file makes it unreadable.
///
/// This acts ONLY on the "not a database" failure (SQLITE_NOTADB, 26): it
/// first probes the file under every key and goes on only if each says 26. A
/// busy or locked file (someone else is using it, and the sidecars may be
/// theirs and live) or any other failure returns Busy / Unreadable with
/// nothing moved. Past that gate it moves the sidecars aside, probes again,
/// and if the file then opens deletes them (they were stale) and returns true;
/// if it still does not open they are put back untouched and false is
/// returned. A genuine WAL or hot journal is never lost: with the right key the
/// file opens on the first, unmoved, attempt and this is never called.
pub async fn settle_stale_sidecars(path: &Path, key_hexes: &[&str]) -> Result<bool, RekeyError> {
    let present: Vec<PathBuf> = sidecar_paths(path)
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    if present.is_empty() {
        return Ok(false);
    }
    // The gate: every key must fail with "not a database", nothing else.
    for key in key_hexes {
        match probe_open(path, key).await {
            OpenProbe::NotADatabase => {}
            OpenProbe::Opens => return Ok(false),
            other => return Err(probe_error(other)),
        }
    }
    // The probes above may have consumed the sidecars themselves.
    let present: Vec<PathBuf> = present.into_iter().filter(|p| p.exists()).collect();
    if present.is_empty() {
        return Ok(false);
    }
    let mut moved = Vec::new();
    for original in &present {
        let mut aside = original.as_os_str().to_owned();
        aside.push(".quarantined");
        let aside = PathBuf::from(aside);
        std::fs::rename(original, &aside)?;
        moved.push((original.clone(), aside));
    }
    let mut opened = false;
    let mut blocked: Option<RekeyError> = None;
    for key in key_hexes {
        match probe_open(path, key).await {
            OpenProbe::Opens => {
                opened = true;
                break;
            }
            OpenProbe::NotADatabase => {}
            other => {
                blocked = Some(probe_error(other));
                break;
            }
        }
    }
    if opened {
        for (_, aside) in &moved {
            let _ = std::fs::remove_file(aside);
        }
    } else {
        for (original, aside) in &moved {
            std::fs::rename(aside, original)?;
        }
    }
    match blocked {
        Some(e) => Err(e),
        None => Ok(opened),
    }
}

async fn user_version(conn: &mut SqliteConnection) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *conn)
        .await
}

/// Open under `key_hex` and prove every page authenticates and the database
/// is structurally sound: `cipher_integrity_check` must return no rows,
/// `quick_check` must say ok, and sqlite_master must be readable. Returns the
/// database's `user_version`.
pub async fn verify_db_key(path: &Path, key_hex: &str) -> Result<i64, RekeyError> {
    let mut conn = keyed_options(path, key_hex)
        .connect()
        .await
        .map_err(|e| RekeyError::Verification(format!("open: {e}")))?;
    let result = async {
        let bad_pages = sqlx::query("PRAGMA cipher_integrity_check")
            .fetch_all(&mut conn)
            .await
            .map_err(|e| RekeyError::Verification(format!("cipher_integrity_check: {e}")))?;
        if !bad_pages.is_empty() {
            return Err(RekeyError::Verification(format!(
                "{} page(s) failed authentication",
                bad_pages.len()
            )));
        }
        let quick: String = sqlx::query_scalar("PRAGMA quick_check")
            .fetch_one(&mut conn)
            .await
            .map_err(|e| RekeyError::Verification(format!("quick_check: {e}")))?;
        if quick != "ok" {
            return Err(RekeyError::Verification(format!("quick_check: {quick}")));
        }
        sqlx::query("SELECT count(*) FROM sqlite_master")
            .execute(&mut conn)
            .await
            .map_err(|e| RekeyError::Verification(format!("sqlite_master: {e}")))?;
        user_version(&mut conn)
            .await
            .map_err(|e| RekeyError::Verification(format!("user_version: {e}")))
    }
    .await;
    let _ = conn.close().await;
    result
}

/// Re-key one database file from `old_key_hex` to `new_key_hex` and verify it
/// opens (and is sound) under the new key. Sequence from the step-0 evidence
/// (module header): open under the old key, checkpoint the WAL to nothing,
/// switch to journal_mode=DELETE, PRAGMA rekey, close, reopen under the new
/// key and verify, and check no sidecar remains. Stale `-wal`, `-shm` and
/// `-journal` files are deleted twice: once right after the switch to
/// rollback-journal mode (the file is then provably ours alone) and once after
/// the verification. Not idempotent by itself: to
/// resume, call db_key_state first and only re-key a file that is still Old.
pub async fn rekey_db_file(
    path: &Path,
    old_key_hex: &str,
    new_key_hex: &str,
) -> Result<(), RekeyError> {
    let mut conn = keyed_options(path, old_key_hex).connect().await?;
    let step = async {
        if let Err(e) = sqlx::query("SELECT count(*) FROM sqlite_master")
            .execute(&mut conn)
            .await
        {
            // Only "not a database" says the key is wrong. Busy is busy.
            return Err(match primary_code(&e) {
                Some(SQLITE_NOTADB) => RekeyError::WrongOldKey,
                _ => busy_or(e),
            });
        }
        let before = user_version(&mut conn).await?;

        let checkpoint = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_optional(&mut conn)
            .await
            .map_err(busy_or)?;
        if let Some(row) = checkpoint {
            // (busy, log, checkpointed): busy=1 means a reader or writer held
            // the WAL and the checkpoint did not complete.
            if row.try_get::<i64, _>(0).unwrap_or(0) != 0 {
                return Err(RekeyError::Busy);
            }
        }
        // Leaving WAL mode needs the file to itself: any other open connection
        // makes this (and so the whole re-key) report Busy, untouched.
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode=DELETE")
            .fetch_one(&mut conn)
            .await
            .map_err(busy_or)?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(RekeyError::JournalMode(mode));
        }
        // Leaving WAL mode succeeded, which proves this connection has the
        // file to itself. Anything still beside it now belongs to no live
        // state (a genuine WAL or hot journal was consumed by the reads
        // above and the checkpoint), and a stale -wal would otherwise be
        // replayed over the database the next time it is opened in WAL mode.
        remove_leftover_sidecars(path)?;

        sqlx::query(&format!("PRAGMA rekey = \"x'{new_key_hex}'\""))
            .execute(&mut conn)
            .await
            .map_err(busy_or)?;
        Ok(before)
    }
    .await;
    // Close (not just drop) so the file is released before it is reopened.
    let _ = conn.close().await;
    let before = step?;

    let after = verify_db_key(path, new_key_hex).await?;
    if after != before {
        return Err(RekeyError::Verification(format!(
            "user_version changed from {before} to {after}"
        )));
    }
    for sidecar in sidecar_paths(path) {
        if std::fs::metadata(&sidecar).is_ok_and(|m| m.len() > 0) {
            return Err(RekeyError::LeftoverSidecar(
                sidecar.to_string_lossy().into_owned(),
            ));
        }
    }
    remove_leftover_sidecars(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqliteConnectOptions;

    const K1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const K2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn c(parts: &[&str]) -> Classified {
        classify(parts)
    }

    #[test]
    fn classification_covers_every_known_file() {
        let db = |purpose, scope: &str| {
            Classified::Item(ItemTarget::Db(DbTarget {
                purpose,
                scope: scope.to_owned(),
            }))
        };
        assert_eq!(
            c(&["integration_keys.db"]),
            db(DbPurpose::IntegrationKeys, "account")
        );
        assert_eq!(
            c(&["cloud_chat_cookies.db"]),
            db(DbPurpose::CloudCookies, "account")
        );
        assert_eq!(
            c(&["personas", "p1", "personal.db"]),
            db(DbPurpose::Personal, "p1")
        );
        assert_eq!(
            c(&["personas", "p1", "outputs.db"]),
            db(DbPurpose::Outputs, "p1")
        );
        assert_eq!(
            c(&["personas", "p1", "messages.db"]),
            db(DbPurpose::Messages, "p1")
        );
        assert_eq!(
            c(&["personas", "p1", "focuses", "f1", "domain_context.db"]),
            db(DbPurpose::DomainContext, "p1/f1")
        );
        assert_eq!(
            c(&[
                "personas",
                "p1",
                "focuses",
                "f1",
                "topics",
                "t1",
                "plan_state.db"
            ]),
            db(DbPurpose::PlanState, "p1/f1/t1")
        );
        assert_eq!(
            c(&["persona_view_shares", "s1", "view_cache.db"]),
            db(DbPurpose::ViewCache, "s1")
        );
        assert_eq!(
            c(&["personas", "p1", "ingested_documents", "o1", "v7.enc"]),
            Classified::Item(ItemTarget::Blob(BlobTarget {
                persona_id: "p1".into(),
                output_id: "o1".into(),
                version: 7
            }))
        );
    }

    #[test]
    fn sidecars_tmp_files_and_unrelated_files_are_told_apart() {
        assert_eq!(
            c(&["personas", "p1", "personal.db-wal"]),
            Classified::Sidecar
        );
        assert_eq!(c(&["integration_keys.db-journal"]), Classified::Sidecar);
        assert_eq!(
            c(&["personas", "p1", "ingested_documents", "o1", "v1.enc.tmp"]),
            Classified::StaleTmp
        );
        assert_eq!(c(&["notes.txt"]), Classified::Other);
        assert_eq!(c(&["personas", "p1", "avatar.png"]), Classified::Other);
    }

    #[test]
    fn unrecognised_encrypted_looking_files_are_unknown() {
        for parts in [
            vec!["mystery.db"],
            vec!["personas", "p1", "extra.db"],
            vec!["personas", "p1", "ingested_documents", "o1", "vX.enc"],
            vec!["personas", "p1", "ingested_documents", "o1", "v1.enc.bak"],
            vec!["personas", "p1", "focuses", "f|1", "domain_context.db"],
            vec!["personas", "p1", "somewhere", "deep", "personal.db"],
        ] {
            assert_eq!(c(&parts), Classified::Unknown, "{parts:?}");
        }
    }

    #[tokio::test]
    async fn inventory_walks_the_tree_in_rekey_order_and_stops_on_unknown_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let touch = |rel: &str, bytes: usize| {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![0u8; bytes]).unwrap();
        };
        touch("personas/p1/ingested_documents/o1/v1.enc", 30);
        touch("personas/p1/ingested_documents/o1/v2.enc", 40);
        touch("personas/p1/ingested_documents/o1/v2.enc.tmp", 5);
        touch("personas/p1/personal.db", 100);
        touch("personas/p1/personal.db-wal", 7);
        touch("personas/p1/outputs.db", 50);
        touch("integration_keys.db", 20);
        touch("persona_view_shares/s1/view_cache.db", 10);
        touch("readme.txt", 1);

        let inv = inventory(root).await.unwrap();
        let order: Vec<&str> = inv.items.iter().map(|i| i.rel_path.as_str()).collect();
        assert_eq!(
            order,
            [
                "integration_keys.db",
                "personas/p1/personal.db",
                "personas/p1/outputs.db",
                "persona_view_shares/s1/view_cache.db",
                "personas/p1/ingested_documents/o1/v1.enc",
                "personas/p1/ingested_documents/o1/v2.enc",
            ]
        );
        assert_eq!(inv.stale_tmp.len(), 1);
        assert_eq!(inv.sidecars.len(), 1);
        assert_eq!(inv.total_bytes(), 250);
        assert_eq!(inv.required_free_space(), 100);

        touch("personas/p1/surprise.db", 1);
        match inventory(root).await {
            Err(InventoryError::UnknownEncryptedFile(p)) => {
                assert_eq!(p, "personas/p1/surprise.db")
            }
            other => panic!("expected UnknownEncryptedFile, got {other:?}"),
        }
        assert!(inventory(&root.join("does-not-exist"))
            .await
            .unwrap()
            .items
            .is_empty());
    }

    #[test]
    fn db_key_for_matches_the_opener_derivation_and_differs_by_epoch() {
        let dek = [0x42u8; 32];
        let target = DbTarget {
            purpose: DbPurpose::Personal,
            scope: "p1".into(),
        };
        let e1 = db_key_for(&target, &dek, "u1", 1).unwrap();
        let e2 = db_key_for(&target, &dek, "u1", 2).unwrap();
        assert_ne!(*e1, *e2);
        let opener = subkeys::derive_db_key_hex(
            &subkeys::encode_hex(&dek),
            DbPurpose::Personal,
            "p1",
            "u1",
            1,
        )
        .unwrap();
        assert_eq!(*e1, *opener);
    }

    // -- real SQLCipher files -----------------------------------------------

    /// A database under `key` holding 300 rows and user_version 7. With
    /// `wal`, it is left in WAL mode with an UNCHECKPOINTED -wal held open by
    /// the returned connection.
    async fn make_db(path: &Path, key: &str, wal: bool) -> Option<SqliteConnection> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .pragma("key", format!("\"x'{key}'\""))
            .pragma("cipher_compatibility", "4");
        let mut conn = opts.connect().await.unwrap();
        if wal {
            sqlx::query("PRAGMA journal_mode=WAL")
                .execute(&mut conn)
                .await
                .unwrap();
            sqlx::query("PRAGMA wal_autocheckpoint=0")
                .execute(&mut conn)
                .await
                .unwrap();
        }
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .execute(&mut conn)
            .await
            .unwrap();
        for i in 0..300 {
            sqlx::query("INSERT INTO t (v) VALUES (?)")
                .bind(format!("row-{i}"))
                .execute(&mut conn)
                .await
                .unwrap();
        }
        sqlx::query("PRAGMA user_version=7")
            .execute(&mut conn)
            .await
            .unwrap();
        if wal {
            Some(conn)
        } else {
            conn.close().await.unwrap();
            None
        }
    }

    async fn row_count(path: &Path, key: &str) -> i64 {
        let mut conn = keyed_options(path, key).connect().await.unwrap();
        let n = sqlx::query_scalar("SELECT count(*) FROM t")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        n
    }

    #[tokio::test]
    async fn rekey_moves_a_wal_database_whose_wal_was_never_checkpointed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        // The writer closes without a checkpoint of its own (autocheckpoint
        // off), leaving rows only in the -wal where the file system allows.
        let live = make_db(&path, K1, true).await.unwrap();
        assert!(
            std::fs::metadata(&sidecar_paths(&path)[0]).unwrap().len() > 0,
            "setup must leave a non-empty -wal"
        );
        live.close().await.unwrap();

        rekey_db_file(&path, K1, K2).await.unwrap();

        assert_eq!(row_count(&path, K2).await, 300);
        assert!(!db_opens_under(&path, K1).await, "old key must not open it");
        assert_eq!(db_key_state(&path, K1, K2).await.unwrap(), DbKeyState::New);
        for sidecar in sidecar_paths(&path) {
            assert!(
                std::fs::metadata(&sidecar).map(|m| m.len()).unwrap_or(0) == 0,
                "{sidecar:?} must be gone"
            );
        }
        assert_eq!(
            verify_db_key(&path, K2).await.unwrap(),
            7,
            "user_version kept"
        );
    }

    #[tokio::test]
    async fn rekey_reports_busy_and_changes_nothing_while_another_connection_holds_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        let live = make_db(&path, K1, true).await.unwrap();

        assert!(matches!(
            rekey_db_file(&path, K1, K2).await,
            Err(RekeyError::Busy)
        ));
        // Nothing was changed: the holder is still on the old key and the file
        // still opens under it.
        assert!(db_opens_under(&path, K1).await);
        assert!(!db_opens_under(&path, K2).await);

        // Once the holder lets go the same call succeeds, rows and all.
        live.close().await.unwrap();
        rekey_db_file(&path, K1, K2).await.unwrap();
        assert_eq!(row_count(&path, K2).await, 300);
    }

    #[tokio::test]
    async fn rekey_with_the_wrong_old_key_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        make_db(&path, K1, false).await;
        let before = std::fs::read(&path).unwrap();
        let wrong = "3333333333333333333333333333333333333333333333333333333333333333";
        assert!(matches!(
            rekey_db_file(&path, wrong, K2).await,
            Err(RekeyError::WrongOldKey)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(row_count(&path, K1).await, 300);
    }

    #[tokio::test]
    async fn rekey_works_on_a_rollback_journal_database_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        make_db(&path, K1, false).await;
        rekey_db_file(&path, K1, K2).await.unwrap();
        assert_eq!(row_count(&path, K2).await, 300);
        assert_eq!(db_key_state(&path, K1, K2).await.unwrap(), DbKeyState::New);
        // A second rekey of the same file is refused: it is no longer old.
        assert!(matches!(
            rekey_db_file(&path, K1, K2).await,
            Err(RekeyError::WrongOldKey)
        ));
    }

    #[tokio::test]
    async fn key_state_probing_is_harmless_and_reports_neither_for_a_stranger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        make_db(&path, K1, false).await;
        assert_eq!(db_key_state(&path, K1, K2).await.unwrap(), DbKeyState::Old);
        // Probing with the wrong key first left it intact.
        assert_eq!(row_count(&path, K1).await, 300);
        let other = "4444444444444444444444444444444444444444444444444444444444444444";
        assert_eq!(
            db_key_state(&path, other, K2).await.unwrap(),
            DbKeyState::Neither
        );
        assert!(!db_opens_under(&dir.path().join("missing.db"), K1).await);
    }

    /// Step-0 finding: a stale -wal beside a database it does not belong to is
    /// replayed over it. verify_db_key must find that damage.
    #[tokio::test]
    async fn verification_finds_a_stale_wal_replayed_over_a_rekeyed_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        let stale = dir.path().join("stale-wal");

        // Capture a live, uncheckpointed -wal from an old-key life of the file.
        let live = make_db(&path, K1, true).await.unwrap();
        std::fs::copy(&sidecar_paths(&path)[0], &stale).unwrap();
        live.close().await.unwrap();

        // The same file is made again, then re-keyed.
        for p in sidecar_paths(&path) {
            let _ = std::fs::remove_file(p);
        }
        std::fs::remove_file(&path).unwrap();
        make_db(&path, K1, false).await;
        rekey_db_file(&path, K1, K2).await.unwrap();
        assert_eq!(verify_db_key(&path, K2).await.unwrap(), 7);

        std::fs::copy(&stale, &sidecar_paths(&path)[0]).unwrap();
        assert!(
            verify_db_key(&path, K2).await.is_err(),
            "a stale old-key -wal must fail verification, not be served"
        );
    }

    #[test]
    fn available_space_finds_the_filesystem_of_an_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let free = available_space(dir.path());
        assert!(free.is_some_and(|n| n > 0), "{free:?}");
        assert_eq!(available_space(&dir.path().join("nope")), None);
    }

    /// items.id=791: a data dir on a tmpfs (which sysinfo omits) must be
    /// measured on that tmpfs, not on the parent mount. The oracle is
    /// independent of the code under test: writing 8 MiB into the tmpfs must
    /// lower the reported free space by about 8 MiB, which the parent
    /// filesystem's figure would not do.
    #[cfg(target_os = "linux")]
    #[test]
    fn available_space_measures_a_tmpfs_not_its_parent_mount() {
        let shm = Path::new("/dev/shm");
        let is_tmpfs = std::fs::read_to_string("/proc/mounts").is_ok_and(|mounts| {
            mounts.lines().any(|l| {
                let mut f = l.split_whitespace();
                let (_, mount_point, fs_type) = (f.next(), f.next(), f.next());
                mount_point == Some("/dev/shm") && fs_type == Some("tmpfs")
            })
        });
        let dir = match tempfile::tempdir_in(shm) {
            Ok(dir) if is_tmpfs => dir,
            _ => {
                eprintln!("skipped: /dev/shm is missing, not writable or not a tmpfs here");
                return;
            }
        };
        const WRITTEN: u64 = 8 * 1024 * 1024;
        const SLACK: u64 = 2 * 1024 * 1024;
        let before = available_space(dir.path()).expect("free space on /dev/shm");
        std::fs::write(dir.path().join("fill"), vec![0xA5u8; WRITTEN as usize]).unwrap();
        let after = available_space(dir.path()).expect("free space on /dev/shm");
        let drop = before.saturating_sub(after);
        assert!(
            drop.abs_diff(WRITTEN) <= SLACK,
            "free space fell by {drop} bytes after writing {WRITTEN} into the tmpfs \
             (before {before}, after {after}): the parent filesystem is being measured"
        );
    }

    #[test]
    fn bytes_available_uses_the_fragment_size_and_refuses_nonsense() {
        assert_eq!(bytes_available(10, 4096, 512), Some(40_960));
        // A filesystem that reports no fragment size falls back to the block size.
        assert_eq!(bytes_available(10, 0, 512), Some(5_120));
        assert_eq!(bytes_available(0, 4096, 4096), Some(0));
        // No usable block size, or an overflowing product, is "unknown".
        assert_eq!(bytes_available(10, 0, 0), None);
        assert_eq!(bytes_available(u64::MAX, 2, 2), None);
    }

    #[test]
    fn rel_paths_round_trip_to_their_targets() {
        for rel in [
            "integration_keys.db",
            "personas/p1/personal.db",
            "personas/p1/focuses/f1/topics/t1/plan_state.db",
            "persona_view_shares/s1/view_cache.db",
            "personas/p1/ingested_documents/o1/v3.enc",
        ] {
            assert!(target_for_rel_path(rel).is_some(), "{rel}");
        }
        assert!(target_for_rel_path("personas/p1/personal.db-wal").is_none());
        assert!(target_for_rel_path("readme.txt").is_none());
    }

    #[test]
    fn leftover_sidecar_and_tmp_removal() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("a.db");
        std::fs::write(&db, b"x").unwrap();
        assert_eq!(remove_leftover_sidecars(&db).unwrap(), 0);
        for p in sidecar_paths(&db) {
            std::fs::write(p, b"junk").unwrap();
        }
        assert_eq!(remove_leftover_sidecars(&db).unwrap(), 3);
        assert!(db.exists(), "the database itself is never touched");
        let tmp = dir.path().join("v1.enc.tmp");
        std::fs::write(&tmp, b"x").unwrap();
        assert!(remove_if_exists(&tmp).unwrap());
        assert!(!remove_if_exists(&tmp).unwrap());
    }

    #[tokio::test]
    async fn a_stale_wal_that_blocks_opening_is_settled_then_the_rekey_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        let stale = dir.path().join("stale-wal");

        let live = make_db(&path, K1, true).await.unwrap();
        std::fs::copy(&sidecar_paths(&path)[0], &stale).unwrap();
        live.close().await.unwrap();
        for p in sidecar_paths(&path) {
            let _ = std::fs::remove_file(p);
        }
        std::fs::remove_file(&path).unwrap();

        // A rollback-journal database with a stale old-key -wal beside it.
        make_db(&path, K1, false).await;
        std::fs::copy(&stale, &sidecar_paths(&path)[0]).unwrap();
        assert!(
            matches!(
                rekey_db_file(&path, K1, K2).await,
                Err(RekeyError::WrongOldKey)
            ),
            "the stale -wal makes a good database unreadable"
        );

        // The failed open did not delete the stale file on every platform and
        // SQLite version, so settle (a no-op when nothing is left) and probe
        // again: the database is then plainly on its old key.
        let _ = settle_stale_sidecars(&path, &[K1, K2]).await.unwrap();
        assert!(!sidecar_paths(&path)[0].exists());
        assert_eq!(db_key_state(&path, K1, K2).await.unwrap(), DbKeyState::Old);
        rekey_db_file(&path, K1, K2).await.unwrap();

        // The next ordinary open (WAL mode, as the stores do) is clean.
        let mut conn = keyed_options(&path, K2).connect().await.unwrap();
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        assert_eq!(verify_db_key(&path, K2).await.unwrap(), 7);
        assert_eq!(row_count(&path, K2).await, 300);
    }

    #[tokio::test]
    async fn settling_puts_the_sidecars_back_when_they_were_not_the_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        make_db(&path, "5".repeat(64).as_str(), false).await;
        // -shm: SQLite does not delete one beside a rollback-journal database,
        // so it survives the gate's own probes and reaches the quarantine.
        let shm = sidecar_paths(&path)[1].clone();
        std::fs::write(&shm, b"shared memory bytes").unwrap();
        // Neither probed key opens the file: the sidecar was not the problem.
        assert!(!settle_stale_sidecars(&path, &[K1, K2]).await.unwrap());
        assert_eq!(std::fs::read(&shm).unwrap(), b"shared memory bytes");
        let mut aside = shm.as_os_str().to_owned();
        aside.push(".quarantined");
        assert!(!PathBuf::from(aside).exists(), "nothing left in quarantine");
        // And with nothing beside it there is nothing to settle.
        std::fs::remove_file(&shm).unwrap();
        assert!(!settle_stale_sidecars(&path, &[K1, K2]).await.unwrap());
    }

    /// A rollback-journal database held under an EXCLUSIVE lock by another
    /// connection, which the returned connection keeps.
    async fn lock_exclusively(path: &Path, key: &str) -> SqliteConnection {
        let mut conn = keyed_options(path, key).connect().await.unwrap();
        sqlx::query("BEGIN EXCLUSIVE")
            .execute(&mut conn)
            .await
            .unwrap();
        conn
    }

    #[tokio::test]
    async fn a_busy_database_is_reported_busy_never_neither_and_its_sidecars_stay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        make_db(&path, K1, false).await;
        let holder = lock_exclusively(&path, K1).await;

        // Probing says Busy, with the right key AND the wrong one.
        assert_eq!(probe_open(&path, K1).await, OpenProbe::Busy);
        assert!(matches!(
            db_key_state(&path, K1, K2).await,
            Err(RekeyError::Busy)
        ));

        // A sidecar beside it (which may be the holder's live one) is not
        // touched: settle refuses on anything but "not a database".
        let wal = sidecar_paths(&path)[0].clone();
        std::fs::write(&wal, b"somebody else's live wal").unwrap();
        assert!(matches!(
            settle_stale_sidecars(&path, &[K1, K2]).await,
            Err(RekeyError::Busy)
        ));
        assert_eq!(std::fs::read(&wal).unwrap(), b"somebody else's live wal");
        assert!(!wal.with_extension("db-wal.quarantined").exists());
        let mut quarantined = wal.as_os_str().to_owned();
        quarantined.push(".quarantined");
        assert!(!PathBuf::from(quarantined).exists());

        // And a rekey attempt reports Busy without changing the file.
        assert!(matches!(
            rekey_db_file(&path, K1, K2).await,
            Err(RekeyError::Busy)
        ));
        holder.close().await.unwrap();
    }

    #[tokio::test]
    async fn only_not_a_database_counts_as_the_wrong_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        make_db(&path, K1, false).await;
        assert_eq!(probe_open(&path, K2).await, OpenProbe::NotADatabase);
        assert_eq!(probe_open(&path, K1).await, OpenProbe::Opens);
        // A missing file is Failed, not NotADatabase, and is not a key state.
        let missing = dir.path().join("missing.db");
        assert!(matches!(
            probe_open(&missing, K1).await,
            OpenProbe::Failed(_)
        ));
        assert!(matches!(
            db_key_state(&missing, K1, K2).await,
            Err(RekeyError::Unreadable(_))
        ));
    }
}

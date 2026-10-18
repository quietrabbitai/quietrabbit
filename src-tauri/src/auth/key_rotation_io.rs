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
pub fn available_space(path: &Path) -> Option<u64> {
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
}

/// SQLITE_BUSY / SQLITE_LOCKED: another connection holds the file.
fn is_busy(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "5" || c == "6")
}

fn busy_or(e: sqlx::Error) -> RekeyError {
    if is_busy(&e) {
        RekeyError::Busy
    } else {
        RekeyError::Database(e)
    }
}

fn keyed_options(path: &Path, key_hex: &str) -> sqlx::sqlite::SqliteConnectOptions {
    crate::providers::utils::quiet_connect_options()
        .filename(path)
        .create_if_missing(false)
        .pragma("key", format!("\"x'{key_hex}'\""))
        .pragma("cipher_compatibility", "4")
        .pragma("busy_timeout", "5000")
}

fn sidecar_paths(path: &Path) -> [PathBuf; 3] {
    ["-wal", "-shm", "-journal"].map(|suffix| {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        PathBuf::from(p)
    })
}

/// Whether the file opens under `key_hex`. Read-only in effect; safe to call
/// with the wrong key on a database left by a crash.
pub async fn db_opens_under(path: &Path, key_hex: &str) -> bool {
    let Ok(mut conn) = keyed_options(path, key_hex).connect().await else {
        return false;
    };
    sqlx::query("SELECT count(*) FROM sqlite_master")
        .execute(&mut conn)
        .await
        .is_ok()
}

/// Which key a database file is on, found by probing the file itself (a
/// crash can fall between a rekey and the row that records it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKeyState {
    New,
    Old,
    Neither,
}

/// Probe NEW first, then OLD.
pub async fn db_key_state(path: &Path, old_key_hex: &str, new_key_hex: &str) -> DbKeyState {
    if db_opens_under(path, new_key_hex).await {
        DbKeyState::New
    } else if db_opens_under(path, old_key_hex).await {
        DbKeyState::Old
    } else {
        DbKeyState::Neither
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
/// key and verify, and check no sidecar remains. Not idempotent by itself: to
/// resume, call db_key_state first and only re-key a file that is still Old.
pub async fn rekey_db_file(
    path: &Path,
    old_key_hex: &str,
    new_key_hex: &str,
) -> Result<(), RekeyError> {
    let mut conn = keyed_options(path, old_key_hex).connect().await?;
    let step = async {
        if sqlx::query("SELECT count(*) FROM sqlite_master")
            .execute(&mut conn)
            .await
            .is_err()
        {
            return Err(RekeyError::WrongOldKey);
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
        match std::fs::metadata(&sidecar) {
            Ok(m) if m.len() > 0 => {
                return Err(RekeyError::LeftoverSidecar(
                    sidecar.to_string_lossy().into_owned(),
                ))
            }
            _ => {}
        }
    }
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
        assert_eq!(db_key_state(&path, K1, K2).await, DbKeyState::New);
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
        assert_eq!(db_key_state(&path, K1, K2).await, DbKeyState::New);
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
        assert_eq!(db_key_state(&path, K1, K2).await, DbKeyState::Old);
        // Probing with the wrong key first left it intact.
        assert_eq!(row_count(&path, K1).await, 300);
        let other = "4444444444444444444444444444444444444444444444444444444444444444";
        assert_eq!(db_key_state(&path, other, K2).await, DbKeyState::Neither);
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
}

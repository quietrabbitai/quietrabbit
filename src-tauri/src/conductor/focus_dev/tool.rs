// src-tauri/src/conductor/focus_dev/tool.rs
//
// The database half of focus_dev (items.id=898, decisions.id=951). Debug and test builds
// only (see mod.rs). Two operations and a reader:
//   validate_seed   a scratch shared.db in a temp directory, migrated, the Focus written by
//                   the one writer and loaded by the REAL loader. Writes nothing real.
//   reseed          dry run by default, or --apply, on a DevRoot's instance/shared.db only.
//   schema_dir_versions  the numbers of the schema/*.sql shared files, for emit-migration.
// Network: none. Encrypted databases and users/: never opened.
//
// RESEED, in order:
//   1  shape::check (cheap, pure);
//   2  validate_seed on a scratch database (an invalid spec never gets near the dev database);
//   3  if the dev database exists and is migrated: VACUUM INTO a temp copy, delete and write
//      the Focus there and load it with the real loader (the loader takes a pool, so the
//      reload check runs on the copy, never on the dev database). Any failure stops here
//      and the dev database has not been opened for writing;
//   4  Apply only: migrate the dev database, then ONE transaction: delete the Focus, write
//      it, read it back inside the transaction (row counts and the action ids in order),
//      commit. A failed write or a failed read-back rolls the transaction back, so the
//      previous rows stay.

use std::path::{Path, PathBuf};

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

use crate::conductor::focus_dev::devroot::DevRoot;
use crate::conductor::focus_dev::{shape, FocusDevError};
use crate::persistence::focus_composition_store as store;
use crate::providers::utils::connect_options_unencrypted;

/// A scratch directory under the system temp dir, removed on drop. (tempfile is a
/// dev-dependency only and the shipped lib must not gain a dependency for this.)
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Result<Self, FocusDevError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("qr-focus-dev-{}-{nanos}", std::process::id()));
        // Created with mode 0700 (owner only) and never recursively: an existing path is an
        // error, so the tool cannot adopt a directory someone else made.
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(&p).map_err(db_err)?;
        Ok(Self(p))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn db_err(e: impl std::fmt::Display) -> FocusDevError {
    FocusDevError::Database(e.to_string())
}

async fn open_pool(path: &Path) -> Result<SqlitePool, FocusDevError> {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(connect_options_unencrypted(path))
        .await
        .map_err(db_err)
}

/// What a validated Focus is made of: one line per row, `(action_id, kind)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidateReport {
    pub focus_id: String,
    pub rows: Vec<(String, String)>,
}

/// Writes `seed` into a database at `db_path` (already migrated) through the one writer and
/// loads it with the real loader.
async fn write_and_load(
    pool: &SqlitePool,
    seed: &store::FocusSeed,
) -> Result<ValidateReport, FocusDevError> {
    let mut conn = pool.acquire().await.map_err(db_err)?;
    store::delete_focus(&mut conn, &seed.focus.focus_id)
        .await
        .map_err(db_err)?;
    store::write_focus(&mut conn, seed).await.map_err(db_err)?;
    drop(conn);
    let def =
        crate::conductor::lifecycle::load_focus_definition_from_db(pool, &seed.focus.focus_id)
            .await
            .map_err(|e| FocusDevError::Spec(format!("the real loader refused it: {e}")))?
            .ok_or_else(|| {
                FocusDevError::Database("the Focus was written but not found".to_owned())
            })?;
    Ok(ValidateReport {
        focus_id: seed.focus.focus_id.clone(),
        rows: def
            .steps
            .iter()
            .map(|s| {
                let kind = match (&s.block_stable_id, &s.requirement) {
                    (Some(b), _) => format!("block {b}"),
                    (None, Some(_)) => "model block".to_owned(),
                    (None, None) if s.store_op.is_some() => "store writer".to_owned(),
                    (None, None) => "ordinary".to_owned(),
                };
                (s.action_id.clone(), kind)
            })
            .collect(),
    })
}

/// Shape checks, then the real loader against a scratch database.
pub async fn validate_seed(seed: &store::FocusSeed) -> Result<ValidateReport, FocusDevError> {
    shape::check(seed)?;
    let dir = ScratchDir::new()?;
    let db = dir.path().join("instance").join("shared.db");
    crate::persistence::migrations::migrate_shared_db_at(&db)
        .await
        .map_err(db_err)?;
    let pool = open_pool(&db).await?;
    let r = write_and_load(&pool, seed).await;
    pool.close().await;
    r
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReseedMode {
    DryRun,
    Apply,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReseedReport {
    pub focus_id: String,
    pub mode_applied: bool,
    /// The dev database exists (a dry run on a missing one reports false).
    pub database_exists: bool,
    pub old_actions: Vec<String>,
    pub new_actions: Vec<String>,
    pub rows: Vec<(String, String)>,
}

#[derive(Clone, Copy)]
enum Hook {
    None,
    #[cfg(test)]
    FailAfterWrite,
}

pub async fn reseed(
    dev: &DevRoot,
    seed: &store::FocusSeed,
    mode: ReseedMode,
) -> Result<ReseedReport, FocusDevError> {
    reseed_with(dev, seed, mode, Hook::None).await
}

async fn current_actions(pool: &SqlitePool, focus_id: &str) -> Vec<String> {
    store::list_compositions(pool, focus_id)
        .await
        .map(|rows| rows.into_iter().map(|r| r.action_id).collect())
        .unwrap_or_default()
}

async fn reseed_with(
    dev: &DevRoot,
    seed: &store::FocusSeed,
    mode: ReseedMode,
    hook: Hook,
) -> Result<ReseedReport, FocusDevError> {
    let focus_id = seed.focus.focus_id.clone();
    let validated = validate_seed(seed).await?;
    let db_path = dev.shared_db_path()?;
    let new_actions: Vec<String> = seed
        .compositions
        .iter()
        .map(|c| c.action_id.clone())
        .collect();
    let exists = db_path.is_file();

    // Step 3: the copy check, and the "old" side of the report. A dry run never migrates.
    let mut old_actions = Vec::new();
    if exists {
        // Verified again immediately before the dev database is opened.
        let db_path = dev.shared_db_path()?;
        let pool = open_pool(&db_path).await?;
        // The schema is current enough when every table the tool writes exists.
        let tables: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN
                ('focuses', 'focus_block_compositions', 'focus_block_customization',
                 'focus_launch_questions', 'focus_launch_options', 'focus_mode_profiles',
                 'focus_launch_display')",
        )
        .fetch_one(&pool)
        .await
        .map_err(db_err)?;
        let schema_ok = tables == 7;
        if schema_ok {
            old_actions = current_actions(&pool, &focus_id).await;
            let tmp = ScratchDir::new()?;
            let copy = tmp.path().join("check.db");
            let q = format!(
                "VACUUM INTO '{}'",
                copy.to_string_lossy().replace('\'', "''")
            );
            sqlx::query(&q).execute(&pool).await.map_err(db_err)?;
            pool.close().await;
            let cpool = open_pool(&copy).await?;
            let r = write_and_load(&cpool, seed).await;
            cpool.close().await;
            r?;
        } else {
            pool.close().await;
            if mode == ReseedMode::DryRun {
                return Err(FocusDevError::Database(
                    "the development database is behind the current schema: --apply migrates it first"
                        .to_owned(),
                ));
            }
        }
    }

    if mode == ReseedMode::DryRun {
        return Ok(ReseedReport {
            focus_id,
            mode_applied: false,
            database_exists: exists,
            old_actions,
            new_actions,
            rows: validated.rows,
        });
    }

    // Step 4.
    // Verified again immediately before the dev database is migrated, and once more before
    // it is opened for the transaction.
    let db_path = dev.shared_db_path()?;
    crate::persistence::migrations::migrate_shared_db_at(&db_path)
        .await
        .map_err(db_err)?;
    let db_path = dev.shared_db_path()?;
    let pool = open_pool(&db_path).await?;
    let result = apply_in_transaction(&pool, seed, &new_actions, hook).await;
    pool.close().await;
    result?;
    Ok(ReseedReport {
        focus_id,
        mode_applied: true,
        database_exists: true,
        old_actions,
        new_actions,
        rows: validated.rows,
    })
}

async fn apply_in_transaction(
    pool: &SqlitePool,
    seed: &store::FocusSeed,
    new_actions: &[String],
    hook: Hook,
) -> Result<(), FocusDevError> {
    let id = seed.focus.focus_id.as_str();
    let mut tx = pool.begin().await.map_err(db_err)?;
    store::delete_focus(&mut tx, id).await.map_err(db_err)?;
    store::write_focus(&mut tx, seed).await.map_err(db_err)?;
    #[cfg(test)]
    if matches!(hook, Hook::FailAfterWrite) {
        // Dropping the transaction without commit rolls it back.
        return Err(FocusDevError::Database(
            "injected failure after the write".to_owned(),
        ));
    }
    let _ = hook;
    // Read back inside the transaction.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT action_id FROM focus_block_compositions WHERE focus_id = ? ORDER BY sequence_index",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let got: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    let want: Vec<&str> = new_actions.iter().map(String::as_str).collect();
    let focus_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM focuses WHERE focus_id = ?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
    if got != want || focus_rows != 1 {
        return Err(FocusDevError::Database(
            "the read-back inside the transaction did not match the spec, rolled back".to_owned(),
        ));
    }
    tx.commit().await.map_err(db_err)
}

/// The numbers of the shared_NNN.sql files in `schema_dir` (emit-migration cross-checks
/// them against SCHEMA_FILES). The example passes the source tree's schema directory,
/// computed from CARGO_MANIFEST_DIR at compile time: a dev-only path, like
/// find_focus_file's (items.id=617).
pub fn schema_dir_versions(schema_dir: &Path) -> Result<Vec<u32>, FocusDevError> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(schema_dir).map_err(db_err)? {
        let name = e
            .map_err(db_err)?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if let Some(n) = name
            .strip_prefix("shared_")
            .and_then(|r| r.strip_suffix(".sql"))
            .and_then(|r| r.parse::<u32>().ok())
        {
            out.push(n);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// Convenience for the example: the registered shared versions.
pub fn registered_shared_versions() -> Vec<u32> {
    crate::persistence::migrations::shared_schema_versions()
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;

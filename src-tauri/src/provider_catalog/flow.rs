// src-tauri/src/provider_catalog/flow.rs
//
// The steps around apply.rs (items.id=713): fetch and stage a catalog, preview
// it, apply it, roll it back, and reconcile after a release migration. The
// Tauri commands (commands/provider_catalog.rs) are thin wrappers over these.
//
// THE ONLY NETWORK CALLER in the catalog code is `check_for_update`, and the
// only caller of that is the click command `check_catalog_update`. A
// source-scan test (provider_catalog/tests.rs) pins both.
//
// STAGING. A check writes three files under `<stage_dir>/<sha256>/`: the
// manifest, its signature, and the body. Apply does not trust that directory:
// it re-reads the three files, re-verifies the signature, re-hashes the body
// against the manifest and re-checks the version, so a file swapped on disk
// between the preview and the click is refused.

use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use crate::persistence::provider_catalog_store as store;
use crate::pinned_asset::{hex32, sha256_of, Expect, FetchError, PinnedFetcher};
use crate::provider_catalog::apply::{
    apply_catalog, rollback, Outcome, Reject, Undo, MAX_FUTURE_DAYS,
};
use crate::provider_catalog::codes;
use crate::provider_catalog::format::{
    parse_catalog, parse_manifest, CatalogDoc, Manifest, CATALOG_ASSET_NAME, FORMAT_VERSION,
    MAX_CATALOG_BYTES, MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES,
};
use crate::provider_catalog::verify::{verify_manifest, VerifyError};
use crate::provider_catalog::{baseline_version, version_date};

const MANIFEST_FILE: &str = "manifest.json";
const SIGNATURE_FILE: &str = "manifest.minisig";

/// A failed step: a stable code plus a short detail (ids and field names only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: &'static str,
    pub detail: String,
}

fn fail<T>(code: &'static str, detail: impl Into<String>) -> Result<T, Failure> {
    Err(Failure {
        code,
        detail: detail.into(),
    })
}

impl From<Reject> for Failure {
    fn from(r: Reject) -> Self {
        Failure {
            code: r.code,
            detail: r.detail,
        }
    }
}

impl From<sqlx::Error> for Failure {
    fn from(e: sqlx::Error) -> Self {
        log::warn!("provider_catalog: database error: {e}");
        Failure {
            code: codes::ERROR_INTERNAL,
            detail: String::new(),
        }
    }
}

fn verify_failure(e: VerifyError) -> Failure {
    Failure {
        code: match e {
            VerifyError::NoTrustedKey => codes::ERROR_NO_TRUSTED_KEY,
            VerifyError::BadSignature => codes::ERROR_SIGNATURE_INVALID,
        },
        detail: String::new(),
    }
}

/// A downloaded, verified catalog waiting for the user's decision.
#[derive(Debug, Clone)]
pub struct Staged {
    pub manifest: Manifest,
    pub doc: CatalogDoc,
    /// Hex SHA-256 of the body, lower case.
    pub sha256: String,
    /// The body text as downloaded (what is stored in the history).
    pub body: String,
}

#[derive(Debug, Clone)]
pub enum CheckResult {
    UpToDate,
    Available(Box<Staged>),
}

/// The version a catalog has to beat: the newest of what is applied, what was
/// rolled back, and what the release itself carries.
pub fn version_floor(state: &store::CatalogState) -> u64 {
    state
        .applied_version
        .max(state.declined_version)
        .max(baseline_version())
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Checks on a verified manifest that need no network. `Ok(true)` = newer than
/// the floor, `Ok(false)` = nothing to do.
pub(crate) fn judge_manifest(
    m: &Manifest,
    state: &store::CatalogState,
    schema_version: u32,
    today: NaiveDate,
) -> Result<bool, Failure> {
    if m.format != FORMAT_VERSION || m.asset_name != CATALOG_ASSET_NAME {
        return fail(codes::ERROR_MANIFEST_INVALID, "format");
    }
    if m.size == 0 || m.size > MAX_CATALOG_BYTES || !is_hex64(&m.sha256) {
        return fail(codes::ERROR_MANIFEST_INVALID, "asset");
    }
    let Some(date) = version_date(m.catalog_version) else {
        return fail(codes::ERROR_MANIFEST_INVALID, "version");
    };
    // A stolen key could otherwise issue a version far in the future and make
    // every honest catalog look old.
    if (date - today).num_days() > MAX_FUTURE_DAYS {
        return fail(codes::ERROR_VERSION_TOO_FAR_AHEAD, "");
    }
    if m.min_shared_schema > schema_version {
        return fail(codes::ERROR_NEEDS_NEWER_APP, "");
    }
    Ok(m.catalog_version > version_floor(state))
}

/// Verify a manifest + signature pair and return the parsed manifest. The
/// bytes are parsed only after the signature has verified.
fn verified_manifest(
    keys: &[&str],
    manifest_bytes: &[u8],
    signature_text: &str,
) -> Result<Manifest, Failure> {
    verify_manifest(keys, manifest_bytes, signature_text).map_err(verify_failure)?;
    parse_manifest(manifest_bytes).map_err(|_| Failure {
        code: codes::ERROR_MANIFEST_INVALID,
        detail: "parse".to_owned(),
    })
}

fn parse_body(manifest: &Manifest, body: &[u8]) -> Result<CatalogDoc, Failure> {
    let doc = parse_catalog(body).map_err(|_| Failure {
        code: codes::ERROR_CATALOG_INVALID,
        detail: "parse".to_owned(),
    })?;
    if doc.format != FORMAT_VERSION || doc.catalog_version != manifest.catalog_version {
        return fail(codes::ERROR_CATALOG_INVALID, "version");
    }
    Ok(doc)
}

fn fetch_failure(e: &FetchError) -> Failure {
    match e {
        FetchError::HashMismatch | FetchError::SizeMismatch { .. } | FetchError::TooLarge(_) => {
            Failure {
                code: codes::ERROR_ASSET_MISMATCH,
                detail: String::new(),
            }
        }
        _ => Failure {
            code: codes::ERROR_DOWNLOAD_FAILED,
            detail: String::new(),
        },
    }
}

/// CLICK-DRIVEN NETWORK STEP. Fetch the manifest and signature, verify, and
/// when a newer catalog exists fetch its body (pinned by the manifest), then
/// stage the three files. With no trusted key it returns before any request.
pub async fn check_for_update(
    fetcher: &PinnedFetcher,
    base_url: &str,
    keys: &[&str],
    state: &store::CatalogState,
    schema_version: u32,
    stage_dir: &Path,
    today: NaiveDate,
) -> Result<CheckResult, Failure> {
    if keys.is_empty() {
        return fail(codes::ERROR_NO_TRUSTED_KEY, "");
    }
    let manifest_bytes = fetcher
        .fetch_small(
            &format!("{base_url}/{}", crate::provider_catalog::MANIFEST_NAME),
            MAX_MANIFEST_BYTES,
        )
        .await
        .map_err(|e| fetch_failure(&e))?;
    let signature_bytes = fetcher
        .fetch_small(
            &format!("{base_url}/{}", crate::provider_catalog::SIGNATURE_NAME),
            MAX_SIGNATURE_BYTES,
        )
        .await
        .map_err(|e| fetch_failure(&e))?;
    let signature_text = String::from_utf8(signature_bytes).map_err(|_| Failure {
        code: codes::ERROR_SIGNATURE_INVALID,
        detail: String::new(),
    })?;
    let manifest = verified_manifest(keys, &manifest_bytes, &signature_text)?;
    if !judge_manifest(&manifest, state, schema_version, today)? {
        return Ok(CheckResult::UpToDate);
    }
    let expect = Expect::from_hex(manifest.size, &manifest.sha256).ok_or(Failure {
        code: codes::ERROR_MANIFEST_INVALID,
        detail: "sha256".to_owned(),
    })?;

    // Start from an empty staging area so old downloads never accumulate.
    let _ = tokio::fs::remove_dir_all(stage_dir).await;
    let dir = stage_dir.join(hex32(&expect.sha256));
    let body_path = dir.join(CATALOG_ASSET_NAME);
    let source = format!("{base_url}/{}", manifest.asset_name);
    fetcher
        .fetch_to_file(
            &[source.as_str()],
            &expect,
            &body_path,
            &CancellationToken::new(),
            &mut |_, _| {},
        )
        .await
        .map_err(|e| fetch_failure(&e))?;
    let body = tokio::fs::read(&body_path).await.map_err(|e| {
        log::warn!("provider_catalog: staged body unreadable: {e}");
        Failure {
            code: codes::ERROR_INTERNAL,
            detail: String::new(),
        }
    })?;
    let doc = parse_body(&manifest, &body)?;
    for (name, bytes) in [
        (MANIFEST_FILE, manifest_bytes.clone()),
        (SIGNATURE_FILE, signature_text.clone().into_bytes()),
    ] {
        tokio::fs::write(dir.join(name), bytes).await.map_err(|e| {
            log::warn!("provider_catalog: could not stage {name}: {e}");
            Failure {
                code: codes::ERROR_INTERNAL,
                detail: String::new(),
            }
        })?;
    }
    Ok(CheckResult::Available(Box::new(Staged {
        sha256: hex32(&expect.sha256),
        body: String::from_utf8_lossy(&body).into_owned(),
        manifest,
        doc,
    })))
}

/// Re-read and re-verify a staged catalog (local files only, no network).
pub async fn load_staged(keys: &[&str], stage_dir: &Path, sha256: &str) -> Result<Staged, Failure> {
    // The sha doubles as a directory name: hex only, so no path can be injected.
    if !is_hex64(sha256) {
        return fail(codes::ERROR_NOT_STAGED, "sha256");
    }
    let sha = sha256.to_ascii_lowercase();
    let dir: PathBuf = stage_dir.join(&sha);
    let read = |name: &'static str| {
        let p = dir.join(name);
        async move { tokio::fs::read(p).await }
    };
    let (Ok(manifest_bytes), Ok(sig_bytes), Ok(body)) = (
        read(MANIFEST_FILE).await,
        read(SIGNATURE_FILE).await,
        read(CATALOG_ASSET_NAME).await,
    ) else {
        return fail(codes::ERROR_NOT_STAGED, "files");
    };
    let signature_text = String::from_utf8(sig_bytes).map_err(|_| Failure {
        code: codes::ERROR_SIGNATURE_INVALID,
        detail: String::new(),
    })?;
    let manifest = verified_manifest(keys, &manifest_bytes, &signature_text)?;
    if !manifest.sha256.eq_ignore_ascii_case(&sha)
        || hex32(&sha256_of(&body)) != sha
        || body.len() as u64 != manifest.size
    {
        return fail(codes::ERROR_ASSET_MISMATCH, "");
    }
    let doc = parse_body(&manifest, &body)?;
    Ok(Staged {
        sha256: sha,
        body: String::from_utf8_lossy(&body).into_owned(),
        manifest,
        doc,
    })
}

// ---------------------------------------------------------------------------
// Preview, apply, rollback
// ---------------------------------------------------------------------------

/// What applying `doc` would do, computed by applying it in a transaction and
/// rolling the transaction back. Nothing is written.
pub async fn preview(
    pool: &SqlitePool,
    doc: &CatalogDoc,
    today: NaiveDate,
) -> Result<Outcome, Failure> {
    let mut tx = pool.begin().await?;
    let result = apply_catalog(&mut tx, doc, today).await;
    tx.rollback().await?;
    Ok(result?)
}

/// Apply a staged catalog in one transaction and record it.
pub async fn apply_staged(
    pool: &SqlitePool,
    staged: &Staged,
    today: NaiveDate,
    now: DateTime<Utc>,
) -> Result<Outcome, Failure> {
    let mut tx = pool.begin().await?;
    let state = store::get_state(&mut tx).await?;
    let schema = store::shared_schema_version(&mut tx).await?;
    if staged.manifest.min_shared_schema > schema {
        return fail(codes::ERROR_NEEDS_NEWER_APP, "");
    }
    if staged.manifest.catalog_version <= version_floor(&state) {
        return fail(codes::ERROR_NOT_NEWER, "");
    }
    let outcome = apply_catalog(&mut tx, &staged.doc, today).await?;
    let undo_json = serde_json::to_string(&outcome.undo).map_err(|_| Failure {
        code: codes::ERROR_INTERNAL,
        detail: String::new(),
    })?;
    store::record_apply(
        &mut tx,
        staged.manifest.catalog_version,
        &staged.sha256,
        &staged.body,
        &undo_json,
        &now.to_rfc3339(),
    )
    .await?;
    tx.commit().await?;
    Ok(outcome)
}

/// Roll back the newest applied catalog.
pub async fn rollback_latest(pool: &SqlitePool, now: DateTime<Utc>) -> Result<Outcome, Failure> {
    let mut tx = pool.begin().await?;
    let state = store::get_state(&mut tx).await?;
    let Some(latest) = store::latest_history(&mut tx).await? else {
        return fail(codes::ERROR_NOTHING_TO_ROLL_BACK, "");
    };
    if latest.version != state.applied_version {
        return fail(codes::ERROR_NOTHING_TO_ROLL_BACK, "");
    }
    let undo: Undo = serde_json::from_str(&latest.undo).map_err(|_| Failure {
        code: codes::ERROR_INTERNAL,
        detail: "undo".to_owned(),
    })?;
    let outcome = rollback(&mut tx, &undo).await?;
    store::record_rollback(&mut tx, latest.version, &now.to_rfc3339()).await?;
    tx.commit().await?;
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// After a release migration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciled {
    /// No catalog had been applied.
    Nothing,
    /// The release carries a catalog at least as new: the applied one is
    /// superseded and forgotten.
    Superseded,
    /// The applied catalog is newer than the release: it was applied again
    /// over whatever the migrations wrote. `changed` = fields that moved.
    Reapplied { changed: usize },
    /// The applied catalog could not be applied again; nothing was changed.
    Left,
}

/// Run once at startup, after the shared.db migrations. It never contacts the
/// network and never applies anything the user has not already applied.
pub async fn reconcile_after_migration(
    pool: &SqlitePool,
    today: NaiveDate,
) -> Result<Reconciled, Failure> {
    let mut tx = pool.begin().await?;
    let state = store::get_state(&mut tx).await?;
    if state.applied_version == 0 {
        return Ok(Reconciled::Nothing);
    }
    if state.applied_version <= baseline_version() {
        store::clear_applied(&mut tx).await?;
        tx.commit().await?;
        return Ok(Reconciled::Superseded);
    }
    let Some(row) = store::get_history(&mut tx, state.applied_version).await? else {
        return Ok(Reconciled::Left);
    };
    let Ok(doc) = parse_catalog(row.document.as_bytes()) else {
        return Ok(Reconciled::Left);
    };
    match apply_catalog(&mut tx, &doc, today).await {
        Ok(outcome) => {
            if outcome.is_noop() {
                return Ok(Reconciled::Reapplied { changed: 0 });
            }
            let undo_json = serde_json::to_string(&outcome.undo).map_err(|_| Failure {
                code: codes::ERROR_INTERNAL,
                detail: String::new(),
            })?;
            store::replace_undo(&mut tx, state.applied_version, &undo_json).await?;
            tx.commit().await?;
            Ok(Reconciled::Reapplied {
                changed: outcome.changes.len(),
            })
        }
        Err(r) => {
            log::warn!(
                "provider_catalog: applied catalog {} could not be applied again: {}",
                state.applied_version,
                r.code
            );
            Ok(Reconciled::Left)
        }
    }
}

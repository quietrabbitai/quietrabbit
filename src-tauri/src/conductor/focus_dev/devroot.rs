// src-tauri/src/conductor/focus_dev/devroot.rs
//
// The dev-root guard (decisions.id=951 ruling 4, amendment 2). focus_dev writes only to
// <dir>/instance/shared.db of a directory that can be SHOWN to be a development copy.
// `DevRoot` is the proof token: the only way to get one is `check_dev_root`, and the apply
// function takes nothing else. Compiled in debug and test builds only (see mod.rs).
//
// A directory is accepted only if
//   (a) it exists, and its canonical path is not, does not contain and does not sit inside
//       a real app data directory ($XDG_DATA_HOME or ~/.local/share, then ai.quietrabbit.app
//       or the legacy quietrabbit). With neither HOME nor XDG_DATA_HOME known the real
//       directories cannot be computed and everything is refused. If XDG_DATA_HOME in the
//       tool's own environment points at the dev root, the dev root is refused too: unset it;
//   (b) it holds the marker file .qr_dev_root written by `mark_dev_root`, whose content names
//       this very canonical path (a copied or moved directory loses it);
//   (c) instance/shared.db exists, or the directory holds nothing but the marker.
// Only Linux paths are known (items.id=700 and 701): any other OS is refused.
// The tool never opens users/ or any encrypted database.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::conductor::focus_dev::FocusDevError;

pub const MARKER_FILE: &str = ".qr_dev_root";
const MARKER_TAG: &str = "qr-dev-root-v1";
const APP_DIR_NAMES: [&str; 2] = ["ai.quietrabbit.app", "quietrabbit"];

#[derive(Serialize, Deserialize)]
struct Marker {
    tag: String,
    path: String,
}

/// Proof that a directory passed the guard. Not constructible elsewhere.
#[derive(Debug, Clone)]
pub struct DevRoot {
    root: PathBuf,
    real_dirs: Vec<PathBuf>,
}

impl DevRoot {
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The database path, VERIFIED NOW: neither instance/ nor shared.db is a symlink, and the
    /// canonical path stays inside the canonical root and outside every real data directory.
    /// Called by check_dev_root and again immediately before the database is opened or
    /// migrated, so a link swapped in after the check is still caught.
    pub fn shared_db_path(&self) -> Result<PathBuf, FocusDevError> {
        verify_db_location(&self.root, &self.real_dirs)
    }
}

fn is_symlink(p: &Path) -> Result<bool, FocusDevError> {
    match std::fs::symlink_metadata(p) {
        Ok(m) => Ok(m.file_type().is_symlink()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(refuse(format!("cannot inspect {}: {e}", p.display()))),
    }
}

/// instance/ and shared.db must be what they look like: a real directory and a regular
/// file, not links (symlink_metadata), and the canonical database path must stay inside the
/// canonical root and outside the real data directories. A missing instance/ or shared.db is
/// fine (a fresh dev root): the check then covers the deepest part that exists.
fn verify_db_location(root: &Path, real_dirs: &[PathBuf]) -> Result<PathBuf, FocusDevError> {
    let instance = root.join("instance");
    let db = instance.join("shared.db");
    if is_symlink(&instance)? {
        return Err(refuse(format!("{} is a symlink", instance.display())));
    }
    if is_symlink(&db)? {
        return Err(refuse(format!("{} is a symlink", db.display())));
    }
    if db.exists()
        && !std::fs::symlink_metadata(&db)
            .map(|m| m.is_file())
            .unwrap_or(false)
    {
        return Err(refuse(format!("{} is not a regular file", db.display())));
    }
    let probe = if db.exists() {
        &db
    } else if instance.exists() {
        &instance
    } else {
        root
    };
    let canon = std::fs::canonicalize(probe)
        .map_err(|e| refuse(format!("cannot resolve {}: {e}", probe.display())))?;
    if !canon.starts_with(root) {
        return Err(refuse(format!(
            "{} resolves outside the dev root to {}",
            probe.display(),
            canon.display()
        )));
    }
    for real in real_dirs {
        let real = canon_or_literal(real);
        if canon == real || canon.starts_with(&real) {
            return Err(refuse(format!(
                "{} resolves into the real app data directory",
                probe.display()
            )));
        }
    }
    Ok(db)
}

fn refuse(msg: impl std::fmt::Display) -> FocusDevError {
    FocusDevError::DevRoot(msg.to_string())
}

/// The real app data directories for a given environment (pure).
pub fn real_data_dirs(home: Option<&str>, xdg_data_home: Option<&str>) -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(x) = xdg_data_home.filter(|v| !v.is_empty()) {
        bases.push(PathBuf::from(x));
    }
    if let Some(h) = home.filter(|v| !v.is_empty()) {
        bases.push(PathBuf::from(h).join(".local").join("share"));
    }
    bases
        .iter()
        .flat_map(|b| APP_DIR_NAMES.iter().map(move |n| b.join(n)))
        .collect()
}

/// The real directories for the tool's own environment.
pub fn real_data_dirs_from_env() -> Vec<PathBuf> {
    real_data_dirs(
        std::env::var("HOME").ok().as_deref(),
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
    )
}

fn canon_or_literal(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Rules (a) and (c) plus existence, shared by check and mark. Returns the canonical path.
fn check_location(path: &Path, real_dirs: &[PathBuf]) -> Result<PathBuf, FocusDevError> {
    if !cfg!(target_os = "linux") {
        return Err(refuse(
            "only Linux data directories are known, this OS is refused (items.id=700, 701)",
        ));
    }
    if real_dirs.is_empty() {
        return Err(refuse(
            "the real app data directory cannot be computed (neither HOME nor XDG_DATA_HOME is set)",
        ));
    }
    let root = std::fs::canonicalize(path).map_err(|e| {
        refuse(format!(
            "{} does not exist or cannot be read: {e}",
            path.display()
        ))
    })?;
    if !root.is_dir() {
        return Err(refuse(format!("{} is not a directory", root.display())));
    }
    for real in real_dirs {
        let real = canon_or_literal(real);
        if root == real {
            return Err(refuse(format!(
                "{} is the real app data directory",
                root.display()
            )));
        }
        if root.starts_with(&real) {
            return Err(refuse(format!(
                "{} is inside the real app data directory {}",
                root.display(),
                real.display()
            )));
        }
        if real.starts_with(&root) {
            return Err(refuse(format!(
                "{} contains the real app data directory {}",
                root.display(),
                real.display()
            )));
        }
    }
    let has_db = root.join("instance").join("shared.db").is_file();
    if !has_db {
        let others = std::fs::read_dir(&root)
            .map_err(|e| refuse(format!("cannot list {}: {e}", root.display())))?
            .filter_map(Result::ok)
            .filter(|e| e.file_name() != MARKER_FILE)
            .count();
        if others > 0 {
            return Err(refuse(format!(
                "{} has no instance/shared.db and is not empty",
                root.display()
            )));
        }
    }
    Ok(root)
}

/// The guard: Ok(DevRoot) only for a marked directory that passes every rule.
pub fn check_dev_root(path: &Path, real_dirs: &[PathBuf]) -> Result<DevRoot, FocusDevError> {
    let root = check_location(path, real_dirs)?;
    let text = std::fs::read_to_string(root.join(MARKER_FILE)).map_err(|_| {
        refuse(format!(
            "{} has no {MARKER_FILE} marker (mark it once with reseed --mark-dev-root)",
            root.display()
        ))
    })?;
    let m: Marker = serde_json::from_str(&text)
        .map_err(|_| refuse(format!("the {MARKER_FILE} marker is not readable")))?;
    if m.tag != MARKER_TAG || Path::new(&m.path) != root {
        return Err(refuse(format!(
            "the {MARKER_FILE} marker does not name {} (a copied or moved directory)",
            root.display()
        )));
    }
    let dev = DevRoot {
        root,
        real_dirs: real_dirs.to_vec(),
    };
    dev.shared_db_path()?;
    Ok(dev)
}

/// Writes the marker, after the same location rules. The one explicit act that makes a
/// directory a valid target.
pub fn mark_dev_root(path: &Path, real_dirs: &[PathBuf]) -> Result<DevRoot, FocusDevError> {
    let root = check_location(path, real_dirs)?;
    verify_db_location(&root, real_dirs)?;
    let m = Marker {
        tag: MARKER_TAG.to_owned(),
        path: root.to_string_lossy().into_owned(),
    };
    std::fs::write(
        root.join(MARKER_FILE),
        serde_json::to_string(&m).map_err(refuse)?,
    )
    .map_err(|e| refuse(format!("cannot write the marker: {e}")))?;
    Ok(DevRoot {
        root,
        real_dirs: real_dirs.to_vec(),
    })
}

// src-tauri/src/privacy_model/paths.rs
//
// Where the Privacy Filter model lives and which path the filter may load
// (items.id=678). Pure resolution with every input a parameter, so each mode is
// unit-tested; `current_location` is the one place that reads the process.
//
// ORDER:
//   1. DEBUG BUILDS ONLY: QR_PRIVACY_FILTER_MODEL, a developer's own file. It is
//      not hashed and has no marker. Release builds never read it: before this
//      item the variable was honoured everywhere, which let any process
//      environment point the filter at any file.
//   2. <data_root>/models/privacy-filter-q8.gguf when a data root is set. This
//      is the only location a release build uses, and the filter loads it only
//      while its verified marker is valid (see `loadable_model_path`).
//   3. DEBUG BUILDS ONLY, and only when no data root is set (integration tests
//      and examples, which never run main.rs's setup): the older
//      $HOME/.local/share/quietrabbit/models/ path. Dev-only; tracked for
//      removal with the other dev-only paths in CLAUDE.md.

use std::path::{Path, PathBuf};

use super::pin;

/// The debug-only override variable.
pub const ENV_OVERRIDE: &str = "QR_PRIVACY_FILTER_MODEL";

const MODELS_DIR_NAME: &str = "models";
const MARKER_SUFFIX: &str = ".verified";

/// `<root>/models`. The directory also holds other files (scores.db), so
/// nothing here ever lists it: only the two exact names below are touched.
pub fn models_dir_in(root: &Path) -> PathBuf {
    root.join(MODELS_DIR_NAME)
}

/// `<root>/models/privacy-filter-q8.gguf`
pub fn model_path_in(root: &Path) -> PathBuf {
    models_dir_in(root).join(pin::FILE_NAME)
}

/// `<model>.verified`, next to the model.
pub fn marker_path_for(model: &Path) -> PathBuf {
    let mut name = model
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(MARKER_SUFFIX);
    model.with_file_name(name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationKind {
    /// A developer's file named by the override (debug builds). Unverified.
    DevOverride,
    /// The installed location under the data root. Needs a valid marker.
    Installed,
    /// The older home-directory dev path (debug builds, no data root).
    DevLegacy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelLocation {
    pub path: PathBuf,
    pub kind: LocationKind,
}

/// The pure resolution. `env_override` is the raw value of
/// QR_PRIVACY_FILTER_MODEL (an empty value counts as unset).
pub fn resolve(
    root: Option<&Path>,
    env_override: Option<&str>,
    home: Option<&Path>,
    debug: bool,
) -> Option<ModelLocation> {
    if debug {
        if let Some(path) = env_override.filter(|p| !p.is_empty()) {
            return Some(ModelLocation {
                path: PathBuf::from(path),
                kind: LocationKind::DevOverride,
            });
        }
    }
    if let Some(root) = root {
        return Some(ModelLocation {
            path: model_path_in(root),
            kind: LocationKind::Installed,
        });
    }
    if debug {
        if let Some(home) = home {
            return Some(ModelLocation {
                path: home
                    .join(".local/share/quietrabbit")
                    .join(MODELS_DIR_NAME)
                    .join(pin::FILE_NAME),
                kind: LocationKind::DevLegacy,
            });
        }
    }
    None
}

/// `resolve` over this process: the recorded data root, the environment and
/// the build profile.
pub fn current_location() -> Option<ModelLocation> {
    let root = crate::providers::utils::try_get_data_root();
    let env_override = std::env::var(ENV_OVERRIDE).ok();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve(
        root.as_deref(),
        env_override.as_deref(),
        home.as_deref(),
        cfg!(debug_assertions),
    )
}

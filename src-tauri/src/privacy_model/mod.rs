// src-tauri/src/privacy_model/mod.rs
//
// Privacy Filter model setup (items.id=678, decisions.id=864): the pinned
// model (pin), where it lives (paths), how it is verified (marker), and the
// task that fetches and activates it (install). The filter asks
// `loadable_model_path` which file it may open.
//
// The model is a required component, fetched in the background after login and
// re-checked at every login. It is shared by all accounts and independent of
// any account's data.

pub mod install;
pub mod marker;
pub mod paths;
pub mod pin;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use crate::pinned_asset::Expect;
use paths::{LocationKind, ModelLocation};

/// The path the Privacy Filter may load, or None while it must not load
/// anything. An installed model counts only while its verified marker is valid;
/// a developer's override or legacy file (debug builds) is returned as it is.
pub fn loadable_model_path() -> Option<PathBuf> {
    loadable(paths::current_location(), &install::pin_expect())
}

pub(crate) fn loadable(location: Option<ModelLocation>, expect: &Expect) -> Option<PathBuf> {
    let location = location?;
    match location.kind {
        LocationKind::DevOverride | LocationKind::DevLegacy => Some(location.path),
        LocationKind::Installed => {
            let marker_path = paths::marker_path_for(&location.path);
            (marker::check(&location.path, &marker_path, expect) == marker::Check::Valid)
                .then_some(location.path)
        }
    }
}

// src-tauri/src/commands/privacy_model.rs
//
// Privacy Filter model setup (items.id=678, decisions.id=864).
// Commands: retry_privacy_model_setup.
//
// The setup task itself starts at login (privacy_model::runtime::ensure_started)
// and reports on the `task-progress` event (kind `privacy_model`). This is the
// retry the visible "privacy check unavailable" state offers: after a failed
// attempt, or after the user placed the model file in the models directory
// while offline. It takes no session state: the model is shared by every
// account and the task needs no account data.

use crate::conductor::privacy::privacy_filter::{self, PrivacyFilterStatus};

/// Start the setup task if none is running, and return the status right after.
/// Idempotent: while a run is alive it only reports. Errs (with text for logs,
/// not for display) for a build without the filter or with a developer model.
#[tauri::command]
#[specta::specta]
pub async fn retry_privacy_model_setup() -> Result<PrivacyFilterStatus, String> {
    crate::privacy_model::runtime::retry().map_err(|refusal| refusal.to_string())?;
    Ok(privacy_filter::status().await)
}

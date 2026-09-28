// src-tauri/src/commands/model_install.rs
//
// Group 24 -- Local model install/management (items.id=436).
// Commands: list_local_models, install_local_model,
//   cancel_local_model_install, enable_local_model, disable_local_model,
//   delete_local_model.
//
// install_local_model kicks off a background pull (providers::
// ollama_install::run_install) and returns a task_id immediately; the
// frontend correlates that task_id against "task-progress" events
// (task_progress::TASK_PROGRESS_EVENT) rather than blocking the IPC call
// for the whole download.
//
// These are the ONLY IPC-reachable writes onto provider_store's local-
// model columns (installed/qr_disabled_by_user) -- see that module's
// header "EXCEPTION" note. Every other providers column stays release-
// bundled/curator-owned with no write path, unchanged by this file.

use serde::Serialize;
use specta::Type;
use tauri::Manager;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::persistence::provider_store::{self, Provider};
use crate::providers::ollama_client::OllamaClient;
use crate::providers::ollama_install;

/// CancellationTokens for in-flight local-model pulls, keyed by task_id.
/// Managed as Tauri state (`Mutex<PullCancellationRegistry>`, main.rs).
/// Lock is only ever held for a brief insert/remove/lookup, never across
/// the pull itself.
#[derive(Default)]
pub struct PullCancellationRegistry {
    tokens: std::collections::HashMap<String, CancellationToken>,
}

impl PullCancellationRegistry {
    fn insert(&mut self, task_id: String, token: CancellationToken) {
        self.tokens.insert(task_id, token);
    }

    fn remove(&mut self, task_id: &str) {
        self.tokens.remove(task_id);
    }

    fn cancel(&mut self, task_id: &str) -> bool {
        match self.tokens.remove(task_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }
}

/// IPC-facing view of a local-model `providers` row.
///
/// `hardware_requirement` is a JSON *string*, not `serde_json::Value` --
/// that type is self-referential and specta's TypeScript exporter recurses
/// through it without terminating (same constraint documented on
/// `commands::cloud_chat_pane::CloudChatProviderSummary::performance_profile`
/// and `commands::mod::PlaceholderPayload`). The frontend `JSON.parse()`s
/// this field if it needs the structured shape.
#[derive(Debug, Clone, Serialize, Type)]
pub struct LocalModelSummary {
    pub id: String,
    pub display_name: String,
    pub local_model_tag: Option<String>,
    pub installed: bool,
    pub qr_disabled_by_user: bool,
    pub focus_eligible: bool,
    pub cloud_chat_visible: bool,
    pub hardware_requirement: Option<String>,
}

impl From<Provider> for LocalModelSummary {
    fn from(p: Provider) -> Self {
        Self {
            id: p.id,
            display_name: p.display_name,
            local_model_tag: p.local_model_tag,
            installed: p.installed,
            qr_disabled_by_user: p.qr_disabled_by_user,
            focus_eligible: p.focus_eligible,
            cloud_chat_visible: p.cloud_chat_visible,
            hardware_requirement: p.hardware_requirement.map(|v| v.to_string()),
        }
    }
}

/// Looks up a `providers` row and validates it's actually a local model --
/// every command below shares this check so a cloud/API id can never reach
/// ollama_install or the install-state store functions (which have their
/// own `provider_type = 'local_model'` defensive scoping, but failing
/// early here gives a clearer error message than a silent 0-rows-affected
/// NotFound would).
async fn require_local_model(pool: &sqlx::SqlitePool, id: &str) -> Result<Provider, String> {
    let provider = provider_store::get_provider(pool, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no such provider: {id}"))?;
    if provider.provider_type != "local_model" {
        return Err(format!("provider '{id}' is not a local_model"));
    }
    Ok(provider)
}

#[tauri::command]
#[specta::specta]
pub async fn list_local_models(
    pool: tauri::State<'_, sqlx::SqlitePool>,
) -> Result<Vec<LocalModelSummary>, String> {
    let providers = provider_store::list_providers_by_type(&pool, "local_model")
        .await
        .map_err(|e| e.to_string())?;
    Ok(providers.into_iter().map(LocalModelSummary::from).collect())
}

/// Starts a background pull and returns a `task_id` immediately -- the
/// frontend listens for `"task-progress"` events carrying that `task_id`
/// rather than waiting on this call. Errors returned here are only the
/// up-front validation failures (unknown id, not a local model, already
/// installed, missing local_model_tag); failures during the pull itself
/// surface as a `"failed"` progress event, not as this command's result.
///
/// The spawned task re-fetches `SqlitePool`/`OllamaClient`/the
/// cancellation registry from the cloned `AppHandle` rather than trying to
/// move the borrowed `tauri::State<'_, _>` parameters into a `'static`
/// task -- same pattern main.rs's own spawned background tasks
/// (`ollama_detection`, the group-folder-sync sweep) already use.
#[tauri::command]
#[specta::specta]
pub async fn install_local_model(
    pool: tauri::State<'_, sqlx::SqlitePool>,
    registry: tauri::State<'_, Mutex<PullCancellationRegistry>>,
    app: tauri::AppHandle,
    id: String,
) -> Result<String, String> {
    let provider = require_local_model(&pool, &id).await?;
    if provider.installed {
        return Err(format!("'{id}' is already installed"));
    }
    let local_model_tag = provider
        .local_model_tag
        .ok_or_else(|| format!("provider '{id}' has no local_model_tag"))?;

    let task_id = uuid::Uuid::new_v4().to_string();
    let cancel_token = CancellationToken::new();
    registry
        .lock()
        .await
        .insert(task_id.clone(), cancel_token.clone());

    let app_for_task = app.clone();
    let task_id_spawned = task_id.clone();

    tauri::async_runtime::spawn(async move {
        let pool_state = app_for_task.state::<sqlx::SqlitePool>();
        let client_state = app_for_task.state::<OllamaClient>();

        let result = ollama_install::run_install(
            &pool_state,
            &app_for_task,
            &client_state,
            &task_id_spawned,
            &id,
            &local_model_tag,
            cancel_token,
        )
        .await;

        if let Err(e) = result {
            log::warn!("commands::model_install: install '{id}' failed: {e}");
        }

        let registry_state = app_for_task.state::<Mutex<PullCancellationRegistry>>();
        registry_state.lock().await.remove(&task_id_spawned);
    });

    Ok(task_id)
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_local_model_install(
    registry: tauri::State<'_, Mutex<PullCancellationRegistry>>,
    task_id: String,
) -> Result<(), String> {
    registry.lock().await.cancel(&task_id);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn enable_local_model(
    pool: tauri::State<'_, sqlx::SqlitePool>,
    id: String,
) -> Result<(), String> {
    require_local_model(&pool, &id).await?;
    provider_store::set_local_model_disabled(&pool, &id, false)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn disable_local_model(
    pool: tauri::State<'_, sqlx::SqlitePool>,
    id: String,
) -> Result<(), String> {
    require_local_model(&pool, &id).await?;
    provider_store::set_local_model_disabled(&pool, &id, true)
        .await
        .map_err(|e| e.to_string())
}

/// Deletes the model's on-disk weights and marks it uninstalled. The
/// "this removes shared on-disk storage another application might depend
/// on" warning is the frontend's responsibility to show *before* calling
/// this -- see providers::ollama_install::run_delete's own doc comment.
#[tauri::command]
#[specta::specta]
pub async fn delete_local_model(
    pool: tauri::State<'_, sqlx::SqlitePool>,
    client: tauri::State<'_, OllamaClient>,
    id: String,
) -> Result<(), String> {
    let provider = require_local_model(&pool, &id).await?;
    let local_model_tag = provider
        .local_model_tag
        .ok_or_else(|| format!("provider '{id}' has no local_model_tag"))?;

    ollama_install::run_delete(&pool, &client, &id, &local_model_tag)
        .await
        .map_err(|e| e.to_string())
}

// src-tauri/src/providers/ollama_install.rs
//
// items.id=436: local Ollama model install/delete orchestration. Sits
// beside ollama_client.rs, which stays transport-focused (raw HTTP calls,
// NDJSON decoding) -- this module drives the pull/delete workflow: talks
// to the client, throttle-emits task_progress events, and updates
// provider_store's install-state columns. commands::model_install is the
// IPC-facing caller; nothing here is a #[tauri::command] itself.

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::persistence::provider_store::{self, ProviderStoreError};
use crate::providers::ollama_client::{OllamaClient, OllamaModelError};
use crate::task_progress::{emit_progress, ProgressThrottle, TaskProgressPayload};

/// Tauri event `kind` for every payload this module emits.
const PROGRESS_KIND: &str = "ollama_pull";

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error(transparent)]
    Ollama(#[from] OllamaModelError),
    #[error(transparent)]
    Store(#[from] ProviderStoreError),
}

fn progress(
    task_id: &str,
    phase: &str,
    current: Option<u64>,
    total: Option<u64>,
    message: Option<String>,
) -> TaskProgressPayload {
    TaskProgressPayload {
        task_id: task_id.to_owned(),
        kind: PROGRESS_KIND.to_owned(),
        phase: phase.to_owned(),
        current,
        total,
        message,
    }
}

/// Pulls `local_model_tag` and, on success, marks `provider_id` installed.
///
/// Emits `task-progress` events throughout via `task_id` (the frontend
/// correlates events to this call using the `task_id`
/// `commands::model_install::install_local_model` returned). Racing
/// `cancel_token.cancelled()` against the NDJSON stream is what makes an
/// in-flight pull cancellable — confirmed live (this item's own scoping
/// session) that closing the underlying HTTP request cancels an in-flight
/// Ollama pull server-side, so cancellation here needs no explicit
/// "abort" call to Ollama beyond dropping the stream/response.
///
/// Cancellation is not treated as an error: the row is simply left
/// `installed: false` and a final `"cancelled"` progress event is emitted.
/// Ollama's pulls are resumable (decisions.id=840), so a later retry
/// re-pulls from wherever the partial blob left off — no extra bookkeeping
/// needed here.
pub async fn run_install(
    pool: &sqlx::SqlitePool,
    handle: &tauri::AppHandle,
    client: &OllamaClient,
    task_id: &str,
    provider_id: &str,
    local_model_tag: &str,
    cancel_token: CancellationToken,
) -> Result<(), InstallError> {
    emit_progress(
        handle,
        &progress(
            task_id,
            "starting",
            None,
            None,
            Some(format!("Starting install of {local_model_tag}")),
        ),
    );

    let mut stream = Box::pin(client.pull_model(local_model_tag).await?);
    let mut throttle = ProgressThrottle::new();

    loop {
        tokio::select! {
            biased;
            () = cancel_token.cancelled() => {
                log::info!("ollama_install: install of '{local_model_tag}' cancelled (task {task_id})");
                emit_progress(handle, &progress(task_id, "cancelled", None, None, None));
                return Ok(());
            }
            next = stream.next() => {
                match next {
                    None => break,
                    Some(Err(e)) => {
                        log::warn!("ollama_install: pull of '{local_model_tag}' failed: {e}");
                        emit_progress(handle, &progress(task_id, "failed", None, None, Some(e.to_string())));
                        return Err(e.into());
                    }
                    Some(Ok(line)) => {
                        if let Some(err_msg) = line.error {
                            log::warn!(
                                "ollama_install: Ollama reported a pull failure for \
                                 '{local_model_tag}': {err_msg}"
                            );
                            emit_progress(handle, &progress(task_id, "failed", None, None, Some(err_msg.clone())));
                            return Err(OllamaModelError::Reported(err_msg).into());
                        }

                        let current = line.completed.unwrap_or(0);
                        let total = line.total.unwrap_or(0);
                        if throttle.should_emit(task_id, current, total) {
                            emit_progress(
                                handle,
                                &progress(task_id, "downloading", line.completed, line.total, Some(line.status.clone())),
                            );
                        }
                    }
                }
            }
        }
    }

    // Re-query /api/tags for the model's own reported digest rather than
    // trusting any digest seen mid-stream (see get_model_digest's doc
    // comment) -- None is acceptable here, this column is bug-report/
    // support tracing only, never read by routing logic.
    let digest = client.get_model_digest(local_model_tag).await;
    provider_store::set_local_model_installed(pool, provider_id, digest.as_deref()).await?;

    throttle.clear(task_id);
    emit_progress(handle, &progress(task_id, "complete", None, None, None));
    log::info!("ollama_install: '{local_model_tag}' installed (provider '{provider_id}')");
    Ok(())
}

/// Deletes `local_model_tag`'s weights via Ollama and marks `provider_id`
/// uninstalled. QR's model directory is QR-private (decisions.id=840), so
/// no other application's storage is affected. Any delete confirmation is a
/// frontend responsibility (a dialog shown before this is ever called) —
/// this function executes unconditionally once invoked, matching how other
/// destructive IPC-driven operations in this codebase are structured (the
/// confirmation lives in the UI layer, not re-litigated in the
/// command/orchestration layer).
pub async fn run_delete(
    pool: &sqlx::SqlitePool,
    client: &OllamaClient,
    provider_id: &str,
    local_model_tag: &str,
) -> Result<(), InstallError> {
    client.delete_model(local_model_tag).await?;
    provider_store::set_local_model_uninstalled(pool, provider_id).await?;
    log::info!("ollama_install: '{local_model_tag}' deleted (provider '{provider_id}')");
    Ok(())
}

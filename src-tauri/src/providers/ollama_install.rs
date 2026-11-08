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

/// Emits the terminal `"failed"` event carrying `err`'s Display text, the same
/// text the in-stream failure events use.
fn emit_failed<R: tauri::Runtime>(
    handle: &tauri::AppHandle<R>,
    task_id: &str,
    err: &dyn std::fmt::Display,
) {
    emit_progress(
        handle,
        &progress(task_id, "failed", None, None, Some(err.to_string())),
    );
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
pub async fn run_install<R: tauri::Runtime>(
    pool: &sqlx::SqlitePool,
    handle: &tauri::AppHandle<R>,
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

    // An early refusal or transport failure (Ollama not running, a refused
    // tag, an untrusted sidecar, a non-2xx status) ends the install before any
    // stream exists: report it on the feed like every later failure, or the
    // frontend stays on "starting" (items.id=863).
    let mut stream = match client.pull_model(local_model_tag).await {
        Ok(s) => Box::pin(s),
        Err(e) => {
            log::warn!("ollama_install: pull of '{local_model_tag}' could not start: {e}");
            emit_failed(handle, task_id, &e);
            return Err(e.into());
        }
    };
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
    if let Err(e) =
        provider_store::set_local_model_installed(pool, provider_id, digest.as_deref()).await
    {
        log::warn!(
            "ollama_install: '{local_model_tag}' pulled but provider '{provider_id}' could not \
             be marked installed: {e}"
        );
        emit_failed(handle, task_id, &e);
        return Err(e.into());
    }

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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tauri::Listener;

    use super::*;
    use crate::providers::curated_models::CURATED_LOCAL_MODEL_TAGS;
    use crate::providers::loopback_test_server::{Canned, LoopbackServer};
    use crate::task_progress::TASK_PROGRESS_EVENT;

    const TASK: &str = "task-863";

    /// Forces the sidecar trust gate to `value` and resets it to false on
    /// drop, including on a panic.
    struct TrustGuard;
    impl TrustGuard {
        fn set(value: bool) -> Self {
            crate::ollama_sidecar::force_trust_for_test(value);
            TrustGuard
        }
    }
    impl Drop for TrustGuard {
        fn drop(&mut self) {
            crate::ollama_sidecar::force_trust_for_test(false);
        }
    }

    fn loopback_port(server: &LoopbackServer) -> u16 {
        server
            .base()
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .expect("loopback base ends with a port")
    }

    /// A port nothing listens on: bound, read, then released.
    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    async fn memory_pool() -> sqlx::SqlitePool {
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory pool")
    }

    /// Collects every `task-progress` payload the mock app emits.
    fn collect_events(
        app: &tauri::App<tauri::test::MockRuntime>,
    ) -> Arc<Mutex<Vec<serde_json::Value>>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        app.handle().listen(TASK_PROGRESS_EVENT, move |event| {
            let value = serde_json::from_str(event.payload()).expect("payload is JSON");
            sink.lock().unwrap().push(value);
        });
        seen
    }

    async fn run(
        app: &tauri::App<tauri::test::MockRuntime>,
        pool: &sqlx::SqlitePool,
        tag: &str,
    ) -> Result<(), InstallError> {
        run_install(
            pool,
            app.handle(),
            &OllamaClient::new(),
            TASK,
            "provider-863",
            tag,
            CancellationToken::new(),
        )
        .await
    }

    /// The feed is exactly `starting` then `failed`, and the failed event
    /// carries the returned error's own Display text.
    fn assert_failed_feed(events: &Arc<Mutex<Vec<serde_json::Value>>>, err: &InstallError) {
        let events = events.lock().unwrap();
        let phases: Vec<&str> = events
            .iter()
            .map(|e| e["phase"].as_str().expect("phase"))
            .collect();
        assert_eq!(phases, ["starting", "failed"], "{events:?}");
        let failed = &events[1];
        assert_eq!(failed["task_id"], TASK);
        assert_eq!(failed["kind"], PROGRESS_KIND);
        assert_eq!(failed["message"], err.to_string());
    }

    #[tokio::test]
    async fn cloud_model_refused_emits_failed() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _trust = TrustGuard::set(true);
        let app = tauri::test::mock_app();
        let events = collect_events(&app);
        let pool = memory_pool().await;

        let err = run(&app, &pool, "gpt-oss:cloud").await.unwrap_err();
        assert!(
            matches!(
                err,
                InstallError::Ollama(OllamaModelError::CloudModelRefused)
            ),
            "{err:?}"
        );
        assert_failed_feed(&events, &err);
    }

    #[tokio::test]
    async fn model_not_curated_emits_failed() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _trust = TrustGuard::set(true);
        let app = tauri::test::mock_app();
        let events = collect_events(&app);
        let pool = memory_pool().await;

        let err = run(&app, &pool, "llama3.3:70b").await.unwrap_err();
        assert!(
            matches!(err, InstallError::Ollama(OllamaModelError::ModelNotCurated)),
            "{err:?}"
        );
        assert_failed_feed(&events, &err);
    }

    #[tokio::test]
    async fn sidecar_untrusted_emits_failed() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _trust = TrustGuard::set(false);
        let app = tauri::test::mock_app();
        let events = collect_events(&app);
        let pool = memory_pool().await;

        let err = run(&app, &pool, CURATED_LOCAL_MODEL_TAGS[0])
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                InstallError::Ollama(OllamaModelError::SidecarUntrusted)
            ),
            "{err:?}"
        );
        assert_failed_feed(&events, &err);
    }

    #[tokio::test]
    async fn unexpected_status_emits_failed() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _trust = TrustGuard::set(true);
        let server = LoopbackServer::start(vec![Canned::json(500, "pull exploded")]);
        let _port = crate::ollama_sidecar::ForcedPortGuard::set(loopback_port(&server));
        let app = tauri::test::mock_app();
        let events = collect_events(&app);
        let pool = memory_pool().await;

        let err = run(&app, &pool, CURATED_LOCAL_MODEL_TAGS[0])
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                InstallError::Ollama(OllamaModelError::UnexpectedStatus { .. })
            ),
            "{err:?}"
        );
        assert_failed_feed(&events, &err);
    }

    /// Ollama not running: the connection is refused, so `.send()` fails.
    #[tokio::test]
    async fn transport_error_emits_failed() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _trust = TrustGuard::set(true);
        let _port = crate::ollama_sidecar::ForcedPortGuard::set(closed_port());
        let app = tauri::test::mock_app();
        let events = collect_events(&app);
        let pool = memory_pool().await;

        let err = run(&app, &pool, CURATED_LOCAL_MODEL_TAGS[0])
            .await
            .unwrap_err();
        assert!(
            matches!(err, InstallError::Ollama(OllamaModelError::Http(_))),
            "{err:?}"
        );
        assert_failed_feed(&events, &err);
    }

    /// The pull succeeds but the database write that marks the row installed
    /// fails: the feed must end on `failed`, never `complete`.
    #[tokio::test]
    async fn store_error_after_a_successful_pull_emits_failed() {
        let _lock = crate::test_support::ENV_MUTEX.lock().await;
        let _trust = TrustGuard::set(true);
        let server = LoopbackServer::start(vec![Canned::json(200, "{\"status\":\"success\"}\n")]);
        let _port = crate::ollama_sidecar::ForcedPortGuard::set(loopback_port(&server));
        let app = tauri::test::mock_app();
        let events = collect_events(&app);
        let pool = memory_pool().await;
        pool.close().await;

        let err = run(&app, &pool, CURATED_LOCAL_MODEL_TAGS[0])
            .await
            .unwrap_err();
        assert!(
            matches!(err, InstallError::Store(ProviderStoreError::Database(_))),
            "{err:?}"
        );
        let events_guard = events.lock().unwrap();
        assert!(
            events_guard.iter().all(|e| e["phase"] != "complete"),
            "{events_guard:?}"
        );
        drop(events_guard);
        assert_failed_feed_after_pull(&events, &err);
    }

    /// Like `assert_failed_feed`, for a pull that streamed first: the feed may
    /// hold `downloading` events between `starting` and the final `failed`.
    fn assert_failed_feed_after_pull(
        events: &Arc<Mutex<Vec<serde_json::Value>>>,
        err: &InstallError,
    ) {
        let events = events.lock().unwrap();
        assert_eq!(events.first().unwrap()["phase"], "starting");
        let last = events.last().unwrap();
        assert_eq!(last["phase"], "failed");
        assert_eq!(last["task_id"], TASK);
        assert_eq!(last["kind"], PROGRESS_KIND);
        assert_eq!(last["message"], err.to_string());
    }
}

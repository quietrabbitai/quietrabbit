// src-tauri/src/task_supervision.rs
// Panic-safe supervision for long-running background tasks (items.id=476).
//
// tauri::async_runtime::spawn / tokio::spawn already isolate a panicking
// task from the rest of the runtime -- but every background task in this
// codebase was fire-and-forget: nobody ever awaited the returned
// JoinHandle, so a panic (e.g. inside the persona/group sync sweep or
// idle-timeout loop in main.rs) produced no log entry and no restart --
// the task just silently stopped existing. spawn_supervised nests the real
// work in its own inner tokio task and awaits *that* JoinHandle from an
// outer supervisor task, so a panic is always logged loudly via
// log::error!, and -- when `restart` is true -- the task is simply spawned
// again rather than left dead.

use std::future::Future;

/// Runs `make_fut()` under a nested tokio task, logging loudly if it
/// panics. `name` identifies the task in the log line. If `restart` is
/// true, a panicked task is respawned by calling `make_fut()` again --
/// appropriate for the infinite-loop sweep tasks; one-shot background
/// tasks should pass `restart: false` since there is nothing meaningful to
/// restart into. A normal (non-panicking) return always stops the
/// supervisor.
pub fn spawn_supervised<F, Fut>(name: &'static str, restart: bool, mut make_fut: F)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        loop {
            match tokio::spawn(make_fut()).await {
                Ok(()) => break,
                Err(join_err) if join_err.is_panic() => {
                    log::error!("task_supervision: background task '{name}' panicked: {join_err}");
                    if !restart {
                        break;
                    }
                }
                Err(join_err) => {
                    // Cancelled (e.g. runtime shutting down) -- nothing to
                    // restart into.
                    log::error!(
                        "task_supervision: background task '{name}' was cancelled: {join_err}"
                    );
                    break;
                }
            }
        }
    });
}

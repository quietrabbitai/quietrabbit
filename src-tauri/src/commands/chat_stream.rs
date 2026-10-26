// src-tauri/src/commands/chat_stream.rs
//
// Group 29 -- Streamed chat reply cancellation (items.id=715, decisions.id=
// 874, 875, 883). Command: cancel_chat_stream.
//
// Stops a streamed reply's HTTP read. The owner is the logged-in session's
// user, read from the KeyRegistry - never an id the front end supplies - so a
// caller can only ever stop its own stream. An unknown run and another user's
// run give the same error and trip nothing; cancelling twice, or a stream
// that just finished, is Ok. This is a Stop of the reply's stream; it does not
// cancel the run. `cancel_run` (commands/execution.rs, items.id=816) is the
// cancel of the whole run: it trips the run's lease stop token.

use tauri::State;

use crate::auth::registry::KeyRegistry;
use crate::chat_stream::{CancelError, ChatStreamRegistry};

/// Error text for an unknown or foreign run.
pub const STREAM_NOT_FOUND: &str = "stream_not_found";

pub(crate) fn cancel_for_user(
    registry: &ChatStreamRegistry,
    session_user: &str,
    run_id: &str,
) -> Result<(), String> {
    registry
        .cancel(session_user, run_id)
        .map_err(|CancelError::NotFound| STREAM_NOT_FOUND.to_owned())
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_chat_stream(
    registry: State<'_, ChatStreamRegistry>,
    key_registry: State<'_, KeyRegistry>,
    run_id: String,
) -> Result<(), String> {
    let user = key_registry
        .with_key(|k| k.user_id.clone())
        .await
        .ok_or_else(|| "not logged in".to_owned())?;
    cancel_for_user(&registry, &user, &run_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_the_registry_outcomes() {
        let reg = ChatStreamRegistry::default();
        let s = reg.register("u1", "run-1");
        assert_eq!(cancel_for_user(&reg, "u1", "run-1"), Ok(()));
        assert!(s.token().is_cancelled());
        assert_eq!(
            cancel_for_user(&reg, "u2", "run-1"),
            Err(STREAM_NOT_FOUND.to_owned())
        );
        assert_eq!(
            cancel_for_user(&reg, "u1", "unknown"),
            Err(STREAM_NOT_FOUND.to_owned())
        );
    }

    #[test]
    fn a_foreign_cancel_does_not_stop_the_stream() {
        let reg = ChatStreamRegistry::default();
        let s = reg.register("owner", "run-1");
        let _ = cancel_for_user(&reg, "someone-else", "run-1");
        assert!(!s.token().is_cancelled());
    }
}

// src-tauri/src/commands/library.rs
//
// Group 7 — Library.
// Commands: list_outputs, get_output, delete_output.
//
// list_outputs: wired to output_store::list_outputs() (items.id=91 part 1,
//   fixed 2026-07-26). Supports optional focus_id/topic_id/output_type
//   filters, joined through focus_runs.
// get_output: wired to output_store::get_output().
// delete_output: wired to output_store::delete_output() (items.id=91 part 2,
//   complete 2026-07-26). Soft-delete only -- see output_store::delete_output
//   for the deletion sequence and deep_purge's deliberately-unimplemented
//   status.
//
// Focus profile visibility enforcement (items.id=230, fixed 2026-08-09):
//   list_outputs/get_output now enforce focus_settings.focus_profile
//   (open/organized/protected, D6-294) on top of output_store's existing
//   status='active' + per-scope DB isolation. An output whose owning Focus
//   is 'protected' is excluded from list_outputs and get_output returns
//   the same "not_found" error a genuinely missing id would -- existence
//   of a Protected output is not distinguishable from a nonexistent one.
//
//   NOTE: this is NOT conductor::visibility::evaluate_object_visibility()
//   (decisions.id=513) -- that's a separate, unrelated system (entity-level
//   redact_identification/hide_from_shared_surfaces flags, object type
//   registry that does not register an "output" type). An earlier version
//   of this comment conflated the two; focus_profile is an independent,
//   independently-enforced field (see the friction gate in
//   commands::persona::update_focus_settings) with no relationship to
//   evaluate_object_visibility(). See items.id=230 for the investigation.
//
//   'organized' has no confirmed Library-gating rule yet -- deliberately
//   left unenforced (falls through to visible/fetchable, same as 'open')
//   pending an actual design decision. Do not infer one here.
//
// key_hex/user_id/persona_id via IPC: intentional for Release 1 (no auth
//   layer yet). Layer 8 will move session key management into tauri::State.

use std::collections::HashMap;

use serde::Serialize;
use specta::Type;

use crate::persistence::focus_settings_store;
use crate::persistence::output_store;

/// Whether the Focus owning `focus_id` is in 'protected' profile, per
/// focus_settings.focus_profile (D6-294). A missing focus_settings row
/// is treated as not-protected -- the confirmed rule only suppresses a
/// *confirmed* 'protected', it does not invent behavior for the unknown
/// case (items.id=230).
async fn is_protected(persona_id: &str, focus_id: &str) -> Result<bool, String> {
    Ok(focus_settings_store::get_focus_settings(persona_id, focus_id)
        .await
        .map_err(|e| e.to_string())?
        .map(|s| s.focus_profile == "protected")
        .unwrap_or(false))
}

// ---------------------------------------------------------------------------
// Response structs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Type)]
pub struct OutputInfo {
    pub id: String,
    pub focus_run_id: String,
    pub output_type: String,
    pub content: String,
    pub sensitivity: String,
    pub status: String,
    pub created_at: String,
}

fn to_output_info(record: output_store::OutputRecord) -> OutputInfo {
    OutputInfo {
        id: record.id,
        focus_run_id: record.focus_run_id,
        output_type: record.output_type,
        content: record.content,
        sensitivity: record.sensitivity,
        status: record.status,
        created_at: record.created_at,
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Lists active outputs, optionally filtered by focus_id, topic_id, and/or
/// output_type. Wired to output_store::list_outputs() (items.id=91, part 1).
///
/// Enforces focus_settings.focus_profile visibility on top of output_store's
/// results -- outputs owned by a 'protected' Focus are excluded (items.id=230).
/// See module header.
#[tauri::command]
#[specta::specta]
pub async fn list_outputs(
    user_id: String,
    persona_id: String,
    key_hex: String,
    focus_id: Option<String>,
    topic_id: Option<String>,
    output_type: Option<String>,
) -> Result<Vec<OutputInfo>, String> {
    let records = output_store::list_outputs(
        &user_id,
        &persona_id,
        &key_hex,
        focus_id.as_deref(),
        topic_id.as_deref(),
        output_type.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())?;

    // Cache focus_settings lookups per focus_id within this call -- a single
    // Library listing typically spans a handful of Focuses, not one lookup
    // per output.
    let mut protected_cache: HashMap<String, bool> = HashMap::new();
    let mut visible = Vec::with_capacity(records.len());
    for record in records {
        let protected = match protected_cache.get(&record.focus_id) {
            Some(v) => *v,
            None => {
                let v = is_protected(&persona_id, &record.focus_id).await?;
                protected_cache.insert(record.focus_id.clone(), v);
                v
            }
        };
        if !protected {
            visible.push(to_output_info(record));
        }
    }

    Ok(visible)
}

/// Enforces focus_settings.focus_profile visibility -- an output owned by a
/// 'protected' Focus returns the same "not_found" error a genuinely missing
/// id would (items.id=230); a Protected output must not be distinguishable
/// from a nonexistent one by its error.
#[tauri::command]
#[specta::specta]
pub async fn get_output(
    output_id: String,
    user_id: String,
    persona_id: String,
    key_hex: String,
) -> Result<OutputInfo, String> {
    let record = output_store::get_output(&user_id, &persona_id, &key_hex, &output_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "not_found".to_string())?;

    if is_protected(&persona_id, &record.focus_id).await? {
        return Err("not_found".to_string());
    }

    Ok(to_output_info(record))
}

/// Deletes an output (items.id=91, part 2, complete 2026-07-26). Soft-delete
/// only -- content is zeroed and status set to 'deleted'; the row is never
/// hard-deleted (architecture Section 3.4, output_store::delete_output).
///
/// deep_purge: accepted for command-contract stability but NOT implemented.
/// Passing Some(true) returns Err("deep_purge_not_implemented"). See
/// output_store::delete_output's doc comment for why this is deliberately
/// out of scope for R1.
#[tauri::command]
#[specta::specta]
pub async fn delete_output(
    output_id: String,
    user_id: String,
    persona_id: String,
    key_hex: String,
    deep_purge: Option<bool>,
) -> Result<(), String> {
    output_store::delete_output(&user_id, &persona_id, &key_hex, &output_id, deep_purge)
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{persona_store, output_store};
    use crate::test_support::ENV_MUTEX;

    const USER_ID: &str = "user-lib-test";
    const PERSONA_ID: &str = "persona-lib-test";
    const KEY_HEX: &str = "deadbeef00112233445566778899aabbccddeeff00112233445566778899aa";

    struct TestEnv {
        _tempdir: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
        saved_root: Option<String>,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            match &self.saved_root {
                Some(v) => std::env::set_var("QR_DATA_ROOT", v),
                None => std::env::remove_var("QR_DATA_ROOT"),
            }
        }
    }

    /// Real shared.db + real encrypted outputs.db via the actual migration
    /// path, plus a persona row -- mirrors auth.rs/tier2.rs's setup()
    /// pattern. Exercises the real store queries end to end, not mocks.
    async fn setup() -> TestEnv {
        let lock = ENV_MUTEX.lock().unwrap();
        let saved_root = std::env::var("QR_DATA_ROOT").ok();

        let tempdir = tempfile::tempdir().expect("failed to create tempdir");
        std::env::set_var("QR_DATA_ROOT", tempdir.path());

        crate::persistence::migrations::migrate_shared_db()
            .await
            .expect("shared.db migration must succeed in test setup");
        crate::persistence::migrations::migrate_outputs_db(USER_ID, PERSONA_ID, KEY_HEX)
            .await
            .expect("outputs.db migration must succeed in test setup");
        // user_personas.user_id REFERENCES users(id) -- create_persona's
        // FK requires a real users row first.
        crate::auth::user_store::create_user(
            USER_ID,
            "Lib Test User",
            "user",
            false,
            &[0u8; crate::auth::kdf::SALT_LEN],
            crate::auth::kdf::DEFAULT_ARGON2_MEMORY_KIB,
            crate::auth::kdf::DEFAULT_ARGON2_ITERATIONS,
            crate::auth::kdf::DEFAULT_ARGON2_PARALLELISM,
        )
        .await
        .expect("create_user must succeed in test setup");
        persona_store::create_persona(PERSONA_ID, "Lib Test Persona", "personal", USER_ID)
            .await
            .expect("create_persona must succeed in test setup");

        TestEnv {
            _tempdir: tempdir,
            _lock: lock,
            saved_root,
        }
    }

    /// Seeds a focus_settings row for `focus_id` with the given focus_profile
    /// (open/organized/protected), plus a matching focus_run and one active
    /// output. Returns the output id.
    async fn seed_output_for_focus(focus_id: &str, focus_profile: &str, content: &str) -> String {
        focus_settings_store::create_focus_settings(
            PERSONA_ID,
            focus_id,
            "bidirectional",
            "shared",
            2,
            2,
            focus_profile,
            None,
        )
        .await
        .expect("create_focus_settings must succeed in test setup");

        let focus_run_id = format!("run-{focus_id}");
        output_store::test_seed_focus_run(USER_ID, PERSONA_ID, KEY_HEX, &focus_run_id, focus_id)
            .await
            .expect("test_seed_focus_run must succeed in test setup");

        output_store::save_output(
            USER_ID,
            PERSONA_ID,
            KEY_HEX,
            &focus_run_id,
            "note",
            content,
            "general",
            None,
        )
        .await
        .expect("save_output must succeed in test setup")
    }

    #[tokio::test]
    async fn list_outputs_includes_open_focus_output() {
        let _env = setup().await;
        seed_output_for_focus("focus-open", "open", "visible content").await;

        let results = list_outputs(
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            KEY_HEX.to_owned(),
            None,
            None,
            None,
        )
        .await
        .expect("list_outputs must succeed");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "visible content");
    }

    #[tokio::test]
    async fn list_outputs_excludes_protected_focus_output() {
        let _env = setup().await;
        seed_output_for_focus("focus-open", "open", "open content").await;
        seed_output_for_focus("focus-protected", "protected", "protected content").await;

        let results = list_outputs(
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            KEY_HEX.to_owned(),
            None,
            None,
            None,
        )
        .await
        .expect("list_outputs must succeed");

        assert_eq!(results.len(), 1, "protected output must be filtered out");
        assert_eq!(results[0].content, "open content");
    }

    #[tokio::test]
    async fn get_output_returns_open_focus_output() {
        let _env = setup().await;
        let output_id = seed_output_for_focus("focus-open", "open", "visible content").await;

        let result = get_output(
            output_id,
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            KEY_HEX.to_owned(),
        )
        .await
        .expect("get_output must succeed for an open-profile output");

        assert_eq!(result.content, "visible content");
    }

    #[tokio::test]
    async fn get_output_blocks_protected_focus_output_as_not_found() {
        let _env = setup().await;
        let output_id =
            seed_output_for_focus("focus-protected", "protected", "protected content").await;

        let result = get_output(
            output_id,
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            KEY_HEX.to_owned(),
        )
        .await;

        assert_eq!(
            result.unwrap_err(),
            "not_found",
            "a Protected output must be indistinguishable from a nonexistent one"
        );
    }

    #[tokio::test]
    async fn get_output_still_returns_not_found_for_a_genuinely_missing_id() {
        let _env = setup().await;

        let result = get_output(
            "does-not-exist".to_owned(),
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            KEY_HEX.to_owned(),
        )
        .await;

        assert_eq!(result.unwrap_err(), "not_found");
    }

    #[tokio::test]
    async fn list_outputs_still_honors_focus_id_filter_alongside_visibility() {
        let _env = setup().await;
        seed_output_for_focus("focus-a", "open", "a content").await;
        seed_output_for_focus("focus-b", "open", "b content").await;

        let results = list_outputs(
            USER_ID.to_owned(),
            PERSONA_ID.to_owned(),
            KEY_HEX.to_owned(),
            Some("focus-a".to_owned()),
            None,
            None,
        )
        .await
        .expect("list_outputs must succeed");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "a content");
    }
}

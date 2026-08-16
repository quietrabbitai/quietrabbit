// src-tauri/src/test_support.rs
//
// Shared test-only infrastructure (items.id=205, 2026-08-01).
//
// ROOT CAUSE THIS FIXES: providers::utils, auth::user_store, and
// commands::auth each independently declared their own `static ENV_MUTEX:
// Mutex<()>` to serialize tests that mutate the process-global QR_DATA_ROOT
// env var. These were three textually-identical but STRUCTURALLY
// INDEPENDENT statics -- same name, same type, zero shared identity. Under
// cargo test's default parallel execution, a test in one module could
// freely interleave QR_DATA_ROOT mutations with a test in another module,
// since neither module's mutex had any awareness of the other's existence.
// Confirmed this session: the full suite failed 15 tests when run in
// parallel, and passed 438/438 when forced serial via --test-threads=1 --
// that gap is the signature of exactly this race, not a logic bug in any
// of the tests or the code they exercise.
//
// FIX: one true mutex, here, imported by every module that needs to
// serialize QR_DATA_ROOT-mutating tests, rather than three independent
// copies of the same pattern.

#[cfg(test)]
pub static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---------------------------------------------------------------------------
// KeyRegistry test harness (items.id=268)
//
// SAME ROOT CAUSE AS ABOVE, caught before it spread further: tier2.rs and
// system.rs had each independently written a byte-identical
// mock_app_with_registry()/populate_registry() pair for standing up a
// KeyRegistry-backed #[tauri::command] test. items.id=268 added
// key_registry: State<'_, KeyRegistry> to four more command modules
// (persona.rs, library.rs, messages.rs, consent.rs) that need the exact
// same harness -- rather than let that become a fifth/sixth/seventh
// textually-identical copy, it's promoted here once, matching the ENV_MUTEX
// precedent above.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub fn mock_app_with_registry() -> tauri::App<tauri::test::MockRuntime> {
    use tauri::Manager;

    let app = tauri::test::mock_app();
    app.manage(crate::auth::registry::KeyRegistry::default());
    app
}

/// Populates an already-managed KeyRegistry. Separate from
/// mock_app_with_registry() (which only constructs the app) because
/// populating requires an .await, and #[tokio::test] already runs in a
/// Tokio runtime: tauri::async_runtime::block_on inside that context panics
/// ("cannot start a runtime from within a runtime") -- see tier2.rs's
/// original version of this comment for the full history.
#[cfg(test)]
pub async fn populate_registry(
    registry: &tauri::State<'_, crate::auth::registry::KeyRegistry>,
    user_id: &str,
    master_key: [u8; crate::auth::kdf::MASTER_KEY_LEN],
) {
    let (sharing_private_key, _) =
        crate::auth::sharing_keypair::derive_sharing_keypair(&master_key, user_id);
    registry
        .replace(crate::auth::registry::UnlockedKey {
            user_id: user_id.to_owned(),
            master_key,
            sharing_private_key: sharing_private_key.to_bytes(),
            unlocked_at: crate::providers::utils::now(),
        })
        .await;
}

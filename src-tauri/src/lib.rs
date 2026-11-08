// src-tauri/src/lib.rs
// Quiet Rabbit -- library root.

pub mod api_chat;
pub mod auth;
pub mod chat_stream;
pub mod cloud_chat_gpu_pane;
pub mod commands;
pub mod conductor;
pub mod doc_parse;
pub mod final_failure;
pub mod group_sync;
pub mod hardware_probe;
pub mod ipc;
pub mod ollama_ownership;
pub mod ollama_sidecar;
pub mod ollama_spawn_thread;
pub mod persistence;
pub mod persona_sync;
pub mod persona_view_sync;
pub mod pinned_asset;
pub mod privacy_model;
pub mod provider_catalog;
pub mod provider_credentials;
pub mod providers;
pub mod recommendation;
pub mod retry_wait;
#[cfg(debug_assertions)]
pub mod rig_hooks;
pub mod spend;
pub mod task_progress;
pub mod task_supervision;
#[cfg(all(test, target_os = "linux"))]
mod test_harness;
#[cfg(test)]
pub mod test_support;
pub mod write_failure;

/// items.id=860: the debug-only rig hooks (`rig_hooks.rs`) must not exist in a release build. The
/// module and its one call site are behind `#[cfg(debug_assertions)]` (`tests/rig_gate.rs` pins
/// that in the source); this checks the result: this release test executable links the same crate
/// code, so if the module were compiled in, its flag text would be in the file. The needle is
/// assembled at run time so this test does not contain it itself. Run by name:
/// `cargo test --release --lib -- release_` (CLAUDE.md, "Key commands").
#[cfg(all(test, not(debug_assertions)))]
mod rig_release_tests {
    #[test]
    fn release_has_no_rig_hooks() {
        let needle = ["qr-", "rig-"].concat();
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        assert!(
            exe.len() > 1_000_000,
            "read the wrong file: {} bytes",
            exe.len()
        );
        let found = exe.windows(needle.len()).any(|w| w == needle.as_bytes());
        assert!(!found, "rig hook text is present in a release build");
    }
}

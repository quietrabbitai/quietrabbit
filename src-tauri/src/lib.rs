// src-tauri/src/lib.rs
// Quiet Rabbit -- library root.

pub mod api_chat;
pub mod auth;
pub mod chat_stream;
pub mod cloud_chat_gpu_pane;
pub mod commands;
pub mod conductor;
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
pub mod provider_catalog;
pub mod provider_credentials;
pub mod providers;
pub mod recommendation;
pub mod retry_wait;
pub mod spend;
pub mod task_progress;
pub mod task_supervision;
#[cfg(test)]
pub mod test_support;
pub mod write_failure;

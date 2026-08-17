// src-tauri/src/lib.rs
// Quiet Rabbit -- library root.

pub mod auth;
pub mod commands;
pub mod conductor;
pub mod group_sync;
pub mod ipc;
pub mod ollama_sidecar;
pub mod persistence;
pub mod providers;
#[cfg(test)]
pub mod test_support;
pub mod tier3_pane;

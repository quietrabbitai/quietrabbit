// src-tauri/src/commands/mod.rs
//
// IPC command surface — 33 Tauri commands across 12 functional groups.
// Push events (run_status_update, consent_request, floor_consent_request,
// notification_available) are NOT registered here — they fire from FocusRun
// via AppHandle::emit() in conductor/lifecycle.rs.
//
// Registration: tauri::generate_handler![] in main.rs.
// Type contract: all command argument and return structs derive specta::Type (D6-345).
// IPC handlers translate internal errors into frontend-safe responses.

pub mod execution;
pub mod consent;
pub mod onboarding;
pub mod persona;
pub mod active_board;
pub mod personal;
pub mod library;
pub mod focus_builder;
pub mod tier2;
pub mod notifications;
pub mod auth;
pub mod system;

// src-tauri/src/commands/mod.rs
//
// IPC command surface — 39 Tauri commands across 13 functional groups.
// Push events (run_status_update, consent_request, floor_consent_request,
// notification_available) are NOT registered here — they fire from FocusRun
// via AppHandle::emit() in conductor/lifecycle.rs.
//
// Registration: quietrabbit_lib::ipc::specta_builder() (collect_commands!),
//   invoked from main.rs. Every command carries BOTH #[tauri::command] and
//   #[specta::specta]; a command missing the latter cannot be collected.
// Type contract: all command argument and return structs derive specta::Type (D6-345),
//   and are exported to TypeScript by the ipc::tests::export_typescript_bindings test.
// IPC handlers translate internal errors into frontend-safe responses.

use serde::{Deserialize, Serialize};
use specta::Type;

/// Placeholder request/response shape for commands that are declared but not
/// yet built.
///
/// Why this exists (items.id=27): five stub commands
/// (onboarding::get_onboarding_focus_suggestions,
/// onboarding::submit_onboarding_focus_selection,
/// focus_builder::get_focus_builder_session,
/// focus_builder::submit_focus_builder_step, tier2::get_tier2_config)
/// previously used serde_json::Value as an argument or return type.
/// serde_json::Value is self-referential (Array(Vec<Value>),
/// Object(Map<String, Value>)), and specta's TypeScript exporter recurses
/// through it without terminating -- confirmed empirically: excluding exactly
/// those five commands makes the export succeed, and a 64 MB stack does not
/// help.
///
/// These commands all return Err("not_implemented") today and have no designed
/// payload shape, so serde_json::Value was a placeholder rather than a decided
/// contract. This type says that explicitly instead. Each of the five should
/// be given its real request/response struct when its feature is designed --
/// this is deliberately not a contract to build against.
///
/// UPDATE (items.id=185, 2026-08-02): tier2::get_tier2_config now has a real
/// return type (commands::tier2::Tier2Config) and no longer uses this
/// placeholder -- four of the original five remain unbuilt.
#[derive(Debug, Serialize, Deserialize, Type)]
pub struct NotImplementedPlaceholder {}

pub mod active_board;
pub mod auth;
pub mod consent;
pub mod execution;
pub mod focus_builder;
pub mod library;
pub mod notifications;
pub mod onboarding;
pub mod persona;
pub mod personal;
pub mod system;
pub mod tier2;
pub mod tier3_pane;

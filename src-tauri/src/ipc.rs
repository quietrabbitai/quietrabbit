// src-tauri/src/ipc.rs
//
// tauri-specta command surface and TypeScript binding export (items.id=27).
//
// Why this exists:
//   tauri-specta 2.0.0-rc.25 and specta-typescript were already declared
//   dependencies and 24 IPC structs already derived specta::Type, but nothing
//   ever exported them -- main.rs used plain tauri::generate_handler!, so the
//   frontend had no machine-checked contract for any command. This module is
//   that contract.
//
// Why it lives in the lib, not main.rs:
//   main.rs is a binary; tests there need `cargo test --bins`. Putting the
//   builder here keeps the drift test (below) inside `cargo test --lib`, which
//   is the suite everything else in this repo is verified with.
//
// Relationship to decisions.id=639 (cross-Persona confirmation flow):
//   The exported bindings are the framework-independent half of that flow.
//   get_pending_cross_persona_confirmations and
//   SubmitFocusRunRequest.confirmed_cross_persona_fact_ids become typed and
//   drift-checked here, before any SPA framework is chosen. The confirmation
//   UI itself is NOT built and cannot be until frontend_stack is decided --
//   there is no frontend source tree in this repo and tauri.conf.json's
//   frontendDist ("../dist") does not exist.

use tauri_specta::{collect_commands, Builder};

use crate::commands;

/// Path the generated bindings are written to, relative to src-tauri/.
///
/// Deliberately inside src-tauri/ rather than a frontend source directory:
/// no frontend tree exists yet, so there is no correct home under one. Move
/// this when the SPA framework is chosen and a frontend tree exists.
pub const BINDINGS_PATH: &str = "bindings.ts";

/// The full IPC command surface, typed for TypeScript export.
///
/// Every command listed here must carry BOTH #[tauri::command] and
/// #[specta::specta]. Adding a command to one list and not the other is the
/// failure mode the drift test below is here to catch.
pub fn specta_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new().commands(collect_commands![
        // Group 1 -- Focus execution
        commands::execution::submit_focus_run,
        commands::execution::get_run_output,
        commands::execution::cancel_run,
        commands::execution::resume_run,
        // Group 2 -- Consent and privacy gates
        commands::consent::submit_consent_decision,
        commands::consent::submit_floor_consent_decision,
        commands::consent::submit_element_consent_decision,
        commands::consent::submit_extract_confirm,
        commands::consent::get_pending_cross_persona_confirmations,
        // Group 3 -- Onboarding
        commands::onboarding::get_onboarding_focus_suggestions,
        commands::onboarding::submit_onboarding_persona_selection,
        commands::onboarding::submit_onboarding_focus_selection,
        // Group 4 -- Persona and Focus management
        commands::persona::list_personas,
        commands::persona::create_persona,
        commands::persona::list_focuses,
        commands::persona::get_focus_settings,
        commands::persona::update_focus_settings,
        // Group 5 -- Active Board
        commands::active_board::get_active_board,
        commands::active_board::get_topic_list,
        commands::active_board::update_topic_state,
        // Group 6 -- Personal context
        commands::personal::get_personal_fields,
        commands::personal::update_personal_field,
        commands::personal::get_voice_profile,
        // Group 7 -- Library
        commands::library::list_outputs,
        commands::library::get_output,
        commands::library::delete_output,
        // Group 8 -- Focus Builder (stubs)
        commands::focus_builder::get_focus_builder_session,
        commands::focus_builder::submit_focus_builder_step,
        // Group 9 -- Tier 2 configuration (stubs)
        commands::tier2::get_tier2_config,
        commands::tier2::set_tier2_provider,
        // Group 10 -- Notifications (stub)
        commands::notifications::dismiss_notification,
        // Group 11 -- Auth (stubs)
        commands::auth::login,
        commands::auth::logout,
        commands::auth::get_recovery_key_display,
        // Group 12 -- System
        commands::system::get_health,
        commands::system::get_capability_profile,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regenerate bindings.ts from the live command surface.
    ///
    /// This is an export, not an assertion: running the suite rewrites the
    /// file. Drift shows up as an uncommitted change to bindings.ts, which
    /// the session-boundary commit surfaces -- rather than as a silently
    /// stale contract the frontend would compile against.
    #[test]
    fn export_typescript_bindings() {
        specta_builder()
            .export(specta_typescript::Typescript::default(), BINDINGS_PATH)
            .expect("failed to export TypeScript bindings");
    }
}

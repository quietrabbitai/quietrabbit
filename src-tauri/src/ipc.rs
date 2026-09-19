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
//   drift-checked here. The confirmation UI itself (items.id=27's remaining
//   scope) is not yet built -- frontend_stack is now decided (React + Vite,
//   decisions.id=640) and a frontend tree exists (items.id=3), but building
//   that UI is separate, later scope, not done by this module.

use tauri_specta::{collect_commands, Builder};

use crate::commands;

/// Path the generated bindings are written to, relative to src-tauri/.
///
/// Points into the frontend's own src/ tree (React + Vite, decisions.id=640),
/// matching the standard tauri-specta convention (e.g. "../src/bindings.ts"
/// in the crate's own docs) now that frontend/ exists. Previously
/// "bindings.ts" (written inside src-tauri/ itself) when no frontend tree
/// existed yet -- see items.id=3.
pub const BINDINGS_PATH: &str = "../frontend/src/bindings.ts";

/// The full IPC command surface, typed for TypeScript export.
///
/// Every command listed here must carry BOTH #[tauri::command] and
/// #[specta::specta]. Adding a command to one list and not the other is the
/// failure mode the drift test below is here to catch.
///
/// DIAG_329 (items.id=329): two full bodies, not one list plus a #[cfg] on a
/// single line, because tauri_specta::collect_commands! is a macro_rules!
/// macro whose fragment matcher (`$b:ident $(:: ...)* ),*`) does not accept
/// attributes on individual entries the way the underlying
/// tauri::generate_handler! proc-macro does -- `#[cfg(...)] path::to::cmd,`
/// inside collect_commands![...] fails to parse. Duplicating the shared list
/// is the only way to guarantee dev_seed_tier3_draft_message is textually
/// absent from a release build's command surface, not merely unreachable.
/// Keep both lists in sync when adding/removing a real (non-dev-only)
/// command; collapse back to one function once every dev-only command using
/// this pattern (currently dev_seed_tier3_draft_message, items.id=329, and
/// dev_bypass_tier3_gate3_review, items.id=356) is removed.
#[cfg(debug_assertions)]
pub fn specta_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new().commands(collect_commands![
        // DIAG_329 (items.id=329) -- dev-only test scaffolding, debug builds
        // only. See commands::messages::dev_seed_tier3_draft_message's own
        // doc comment.
        commands::messages::dev_seed_tier3_draft_message,
        // DIAG_356 (items.id=356) -- dev-only test scaffolding, debug builds
        // only. See commands::consent::dev_bypass_tier3_gate3_review's own
        // doc comment.
        commands::consent::dev_bypass_tier3_gate3_review,
        // Group 1 -- Focus execution
        commands::execution::submit_focus_run,
        commands::execution::get_run_output,
        commands::execution::cancel_run,
        commands::execution::resume_run,
        // Group 2 -- Consent and privacy gates
        commands::consent::submit_consent_decision,
        commands::consent::submit_floor_consent_decision,
        commands::consent::submit_element_consent_decision,
        commands::consent::submit_friction_gate_decision,
        commands::consent::submit_extract_confirm,
        commands::consent::get_pending_cross_persona_confirmations,
        commands::consent::request_tier3_gate3_review,
        commands::consent::resolve_tier3_gate3_review,
        commands::consent::recheck_tier3_provider_selection,
        commands::consent::request_chat_copy_gate3_review,
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
        commands::library::copy_output_to_clipboard,
        // Group 8 -- Focus Builder (stubs)
        commands::focus_builder::get_focus_builder_session,
        commands::focus_builder::submit_focus_builder_step,
        // Group 9 -- Tier 1.5 (qr_hosted) configuration (items.id=185, 2026-08-02)
        commands::tier2::get_tier2_config,
        commands::tier2::set_tier2_provider,
        commands::tier2::set_tier2_provider_preference,
        commands::tier2::get_tier2_provider_preferences,
        // Group 10 -- Notifications (stub)
        commands::notifications::dismiss_notification,
        // Group 11 -- Auth (items.id=205, 2026-08-01; get_recovery_key_display
        // wired items.id=229, 2026-08-09; get_session wired items.id=267,
        // 2026-08-15; record_activity wired items.id=311, 2026-08-22 --
        // none of the five are stubs now)
        commands::auth::login,
        commands::auth::logout,
        commands::auth::get_recovery_key_display,
        commands::auth::get_session,
        commands::auth::record_activity,
        // Group 12 -- System
        commands::system::get_health,
        commands::system::get_capability_profile,
        // Group 13 -- Cloud Chat pane lifecycle & provider catalog
        // (items.id=202 piece 5 / items.id=223, 2026-08-04)
        commands::cloud_chat_pane::list_active_providers,
        commands::cloud_chat_pane::open_tier3_panes,
        commands::cloud_chat_pane::close_tier3_pane,
        commands::cloud_chat_pane::set_pane_layout,
        // items.id=359 pieces 4/5 -- rail+content-pane single-active-pane model
        commands::cloud_chat_pane::set_active_pane,
        // items.id=257 Path B -- DOM-based pane click routing
        commands::cloud_chat_pane::forward_pane_mouse_click,
        commands::cloud_chat_pane::forward_pane_mouse_move,
        commands::cloud_chat_pane::forward_pane_mouse_wheel,
        commands::cloud_chat_pane::forward_pane_key,
        commands::cloud_chat_pane::adjust_pane_zoom,
        // items.id=234 -- host-owned popup subsystem, OAuth popup click routing
        commands::cloud_chat_pane::forward_popup_mouse_click,
        commands::cloud_chat_pane::forward_popup_mouse_move,
        commands::cloud_chat_pane::forward_popup_mouse_wheel,
        commands::cloud_chat_pane::forward_popup_key,
        // Group 14 -- Messages/transcript (ChatPane.tsx backing)
        commands::messages::send_message,
        commands::messages::list_messages,
        // Group 15 -- Group folder sync (items.id=287, group.db 266e)
        commands::group::set_group_sync_folder,
        commands::group::get_group_sync_folder,
        // Group 16 -- Group membership (items.id=288, group.db 266f)
        commands::group::remove_group_member,
        // Group 17 -- Group creation (items.id=291)
        commands::group::create_group,
        // Group 18 -- Group facts opt-in (items.id=296, group.db 266h)
        commands::group::set_group_fact_source_opt_in,
        commands::group::get_group_fact_sources,
        // Group 23 -- History screen's Group row (items.id=404)
        commands::group::list_persona_group_ids,
        // Group 19 -- Persona share folder sync (items.id=303, decisions.id=722)
        commands::persona_sync::set_persona_share_sync_folder,
        commands::persona_sync::get_persona_share_sync_folder,
        // Group 20 -- VIEW-ONLY persona share folder sync (items.id=304,
        // decisions.id=723)
        commands::persona_view_sync::set_persona_view_share_sync_folder,
        commands::persona_view_sync::get_persona_view_share_sync_folder,
        // Group 21 -- Document ingestion storage model (items.id=383,
        // decisions.id=486). NOT the future ingest_document (decisions.id=
        // 491) -- see commands::ingest's own module header for why.
        commands::ingest::store_ingested_document,
        commands::ingest::get_ingested_document_bytes,
        // Group 22 -- Persona-scoped chat history (items.id=384 slice 6,
        // decisions.id=739/740)
        commands::chats::create_chat,
        commands::chats::list_chats,
        commands::chats::archive_chat,
    ])
}

/// Release-build counterpart of the function above -- identical except it
/// has no dev_seed_tier3_draft_message or dev_bypass_tier3_gate3_review
/// entry (neither item exists in a release build; see their respective
/// #[cfg(debug_assertions)] in commands/messages.rs and commands/consent.rs).
/// See the debug-build specta_builder's own doc comment for why this is a
/// second full function rather than one shared
/// list with a per-entry #[cfg].
#[cfg(not(debug_assertions))]
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
        commands::consent::submit_friction_gate_decision,
        commands::consent::submit_extract_confirm,
        commands::consent::get_pending_cross_persona_confirmations,
        commands::consent::request_tier3_gate3_review,
        commands::consent::resolve_tier3_gate3_review,
        commands::consent::recheck_tier3_provider_selection,
        commands::consent::request_chat_copy_gate3_review,
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
        commands::library::copy_output_to_clipboard,
        // Group 8 -- Focus Builder (stubs)
        commands::focus_builder::get_focus_builder_session,
        commands::focus_builder::submit_focus_builder_step,
        // Group 9 -- Tier 1.5 (qr_hosted) configuration (items.id=185, 2026-08-02)
        commands::tier2::get_tier2_config,
        commands::tier2::set_tier2_provider,
        commands::tier2::set_tier2_provider_preference,
        commands::tier2::get_tier2_provider_preferences,
        // Group 10 -- Notifications (stub)
        commands::notifications::dismiss_notification,
        // Group 11 -- Auth (items.id=205, 2026-08-01; get_recovery_key_display
        // wired items.id=229, 2026-08-09; get_session wired items.id=267,
        // 2026-08-15; record_activity wired items.id=311, 2026-08-22 --
        // none of the five are stubs now)
        commands::auth::login,
        commands::auth::logout,
        commands::auth::get_recovery_key_display,
        commands::auth::get_session,
        commands::auth::record_activity,
        // Group 12 -- System
        commands::system::get_health,
        commands::system::get_capability_profile,
        // Group 13 -- Cloud Chat pane lifecycle & provider catalog
        // (items.id=202 piece 5 / items.id=223, 2026-08-04)
        commands::cloud_chat_pane::list_active_providers,
        commands::cloud_chat_pane::open_tier3_panes,
        commands::cloud_chat_pane::close_tier3_pane,
        commands::cloud_chat_pane::set_pane_layout,
        // items.id=359 pieces 4/5 -- rail+content-pane single-active-pane model
        commands::cloud_chat_pane::set_active_pane,
        // items.id=257 Path B -- DOM-based pane click routing
        commands::cloud_chat_pane::forward_pane_mouse_click,
        commands::cloud_chat_pane::forward_pane_mouse_move,
        commands::cloud_chat_pane::forward_pane_mouse_wheel,
        commands::cloud_chat_pane::forward_pane_key,
        commands::cloud_chat_pane::adjust_pane_zoom,
        // items.id=234 -- host-owned popup subsystem, OAuth popup click routing
        commands::cloud_chat_pane::forward_popup_mouse_click,
        commands::cloud_chat_pane::forward_popup_mouse_move,
        commands::cloud_chat_pane::forward_popup_mouse_wheel,
        commands::cloud_chat_pane::forward_popup_key,
        // Group 14 -- Messages/transcript (ChatPane.tsx backing)
        commands::messages::send_message,
        commands::messages::list_messages,
        // Group 15 -- Group folder sync (items.id=287, group.db 266e)
        commands::group::set_group_sync_folder,
        commands::group::get_group_sync_folder,
        // Group 16 -- Group membership (items.id=288, group.db 266f)
        commands::group::remove_group_member,
        // Group 17 -- Group creation (items.id=291)
        commands::group::create_group,
        // Group 18 -- Group facts opt-in (items.id=296, group.db 266h)
        commands::group::set_group_fact_source_opt_in,
        commands::group::get_group_fact_sources,
        // Group 23 -- History screen's Group row (items.id=404)
        commands::group::list_persona_group_ids,
        // Group 19 -- Persona share folder sync (items.id=303, decisions.id=722)
        commands::persona_sync::set_persona_share_sync_folder,
        commands::persona_sync::get_persona_share_sync_folder,
        // Group 20 -- VIEW-ONLY persona share folder sync (items.id=304,
        // decisions.id=723)
        commands::persona_view_sync::set_persona_view_share_sync_folder,
        commands::persona_view_sync::get_persona_view_share_sync_folder,
        // Group 21 -- Document ingestion storage model (items.id=383,
        // decisions.id=486). NOT the future ingest_document (decisions.id=
        // 491) -- see commands::ingest's own module header for why.
        commands::ingest::store_ingested_document,
        commands::ingest::get_ingested_document_bytes,
        // Group 22 -- Persona-scoped chat history (items.id=384 slice 6,
        // decisions.id=739/740)
        commands::chats::create_chat,
        commands::chats::list_chats,
        commands::chats::archive_chat,
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
    ///
    /// items.id=372: `specta_builder()` itself is `#[cfg(debug_assertions)]`
    /// gated (two full bodies above -- see that doc for why) specifically so
    /// dev-only commands like dev_seed_tier3_draft_message are textually
    /// absent from a release build's command surface. That same cfg means
    /// `cargo test --release` (debug_assertions off) silently picks the
    /// *release* body here and regenerates bindings.ts missing every
    /// dev-only command -- no error, just a quietly stale bindings.ts
    /// committed as if it were the real drift check. Confirmed recurring
    /// three times, most recently via a routine `cargo build --release`
    /// commit-verification pass. The assertion below turns that silent
    /// truncation into a loud failure instead of catching it by hand again.
    #[test]
    fn export_typescript_bindings() {
        assert!(
            cfg!(debug_assertions),
            "export_typescript_bindings must run under a debug/dev build -- \
             invoked under release (debug_assertions off), which would silently \
             regenerate {BINDINGS_PATH} from the release-profile command list \
             (specta_builder's #[cfg(not(debug_assertions))] body above), \
             stripping every dev-only command (dev_seed_tier3_draft_message, \
             dev_bypass_tier3_gate3_review, ...). Run `cargo test --lib` \
             (debug profile) instead of `cargo test --release`."
        );
        specta_builder()
            .export(specta_typescript::Typescript::default(), BINDINGS_PATH)
            .expect("failed to export TypeScript bindings");
    }
}

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use quietrabbit_lib::commands;
use quietrabbit_lib::conductor::concurrency::ConductorScheduler;
use quietrabbit_lib::providers::ollama_client::OllamaClient;

#[tokio::main]
async fn main() {
    let scheduler = Arc::new(ConductorScheduler::new());
    // OllamaClient is stateless (three reqwest::Client instances, no mutable
    // state). Tauri State provides shared immutable access -- no Arc wrapping
    // needed beyond what Tauri already does internally.
    let ollama_client = OllamaClient::new();

    tauri::Builder::default()
        .manage(scheduler)
        .manage(ollama_client)
        .invoke_handler(tauri::generate_handler![
            // Group 1 -- Focus execution
            commands::execution::submit_focus_run,
            commands::execution::get_run_output,
            commands::execution::cancel_run,
            commands::execution::resume_run,
            // Group 2 -- Consent and privacy gates
            commands::consent::submit_consent_decision,
            commands::consent::submit_floor_consent_decision,
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
        .run(tauri::generate_context!())
        .expect("error while running Quiet Rabbit");
}

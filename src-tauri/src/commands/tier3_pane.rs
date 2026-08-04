// src-tauri/src/commands/tier3_pane.rs
//
// Group 13 -- Tier 2/Tier 3 pane lifecycle & provider catalog.
// Commands: list_active_providers, open_tier3_panes, close_tier3_pane.
//
// items.id=202 piece 5 / items.id=223 connective tissue: neither item's own
// description enumerates an IPC command, but on-demand pane creation
// (items.id=223's whole point) needs something to actually call
// tier3_pane::sync_window::PaneWindow::request_open/close from the frontend
// side -- this module is that something.
//
// list_active_providers wraps persistence::provider_store::list_active_providers()
// (commit 4e5147f) in a frontend-facing DTO rather than exposing
// provider_store::Provider directly -- that type isn't specta::Type (a
// persistence-layer type shouldn't carry an IPC-serialization derive just
// for this one caller, same reasoning as tier2.rs's Tier2Config being a
// distinct non-secret DTO rather than the full stored credential type) and
// carries several fields (documentation_gate, review bookkeeping) this
// screen has no use for.
//
// open_tier3_panes/close_tier3_pane send PaneCommand::Open/Close through
// the EventLoopProxy managed as Tauri state in main.rs -- the proxy is
// Send + Sync (winit's own guarantee), unlike PaneWindow/PaneManager
// themselves, which is what lets these async command handlers reach the
// winit-thread-affine pane machinery at all. launch_url is resolved
// server-side from provider_store, not accepted from the frontend -- the
// frontend only ever knows provider IDs.

use tauri::State;
use winit::event_loop::EventLoopProxy;

use crate::persistence::provider_store::{self, ProviderTier};
use crate::tier3_pane::sync_window::PaneCommand;

// ---------------------------------------------------------------------------
// IPC types
// ---------------------------------------------------------------------------

/// Selector-screen-facing provider summary. `lane` mirrors
/// `provider_store::ProviderTier`'s own serde rendering ("tier2"/"tier3")
/// and the frontend's `ProviderLane` string type (tier3AccessConfig.ts)
/// verbatim -- no further transformation needed on the TypeScript side.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct Tier3ProviderSummary {
    pub id: String,
    pub display_name: String,
    pub lane: String,
}

fn lane_str(tier: ProviderTier) -> &'static str {
    match tier {
        ProviderTier::Tier2 => "tier2",
        ProviderTier::Tier3 => "tier3",
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// The selector screen's primary read path (TIER3_ACCESS_MODEL.md State 3,
/// items.id=202 piece 1's remaining wiring) -- replaces
/// tier3AccessConfig.ts's PLACEHOLDER_PROVIDERS stand-in array.
#[tauri::command]
#[specta::specta]
pub async fn list_active_providers() -> Result<Vec<Tier3ProviderSummary>, String> {
    let providers = provider_store::list_active_providers()
        .await
        .map_err(|e| e.to_string())?;

    Ok(providers
        .into_iter()
        .map(|p| Tier3ProviderSummary {
            id: p.id,
            display_name: p.display_name,
            lane: lane_str(p.tier).to_string(),
        })
        .collect())
}

/// Opens one pane per confirmed provider selection (items.id=223's actual
/// trigger -- nothing in tier3_pane/ creates a pane except in response to
/// this). `launch_url` is looked up server-side; the frontend only ever
/// passes provider IDs. Best-effort across the batch: the first provider
/// that fails to resolve or send aborts the remaining opens rather than
/// silently skipping them, since a partial open would leave the selector's
/// own "confirmed" state and the actual open panes disagreeing about what's
/// open.
///
/// FOUND THE HARD WAY (2026-08-04, manual verification): sending
/// `PaneCommand::Open` is not enough by itself. main.rs's heartbeat thread
/// only calls `run_on_main_thread` (the only thing that forces tao's GTK
/// loop to fire `MainEventsCleared`, per the freeze-bug root cause) while
/// `open_pane_count > 0` -- but a pane can only become open by processing
/// this very `Open` command, which requires `MainEventsCleared` to fire
/// first. With zero panes open, the heartbeat provides no help at all, and
/// nothing else guarantees the main window stays busy enough to dispatch
/// the queued event promptly (confirmed empirically: a real click on this
/// command's own trigger button did not itself produce a dispatch within
/// several seconds). Fix: force exactly one wake here, synchronously,
/// right after queuing the command -- the same `run_on_main_thread`
/// mechanism the heartbeat uses, just called from the command that
/// actually needs the wake instead of waited for. This does not reintroduce
/// the always-on cost items.id=223 exists to avoid: it is one wake per
/// open/close call, not a resumed continuous cadence.
#[tauri::command]
#[specta::specta]
pub async fn open_tier3_panes(
    provider_ids: Vec<String>,
    proxy: State<'_, EventLoopProxy<PaneCommand>>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    for provider_id in provider_ids {
        let provider = provider_store::get_provider(&provider_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("provider '{provider_id}' not found"))?;
        let Some(launch_url) = provider.launch_url else {
            return Err(format!(
                "provider '{provider_id}' has no launch_url -- cannot open a pane for it"
            ));
        };

        proxy
            .send_event(PaneCommand::Open {
                key: provider_id,
                url: launch_url,
            })
            .map_err(|_| "tier3_pane event loop is no longer running".to_string())?;
        let _ = app_handle.run_on_main_thread(|| {});
    }
    Ok(())
}

/// Closes one pane by provider ID. A no-op (not an error) if that provider
/// has no open pane -- PaneManager::close_pane already tolerates this
/// (sync_window.rs), and a caller racing a close against an already-closed
/// pane is a normal condition, not a failure. See open_tier3_panes' doc for
/// why the explicit wake below is required, not optional.
#[tauri::command]
#[specta::specta]
pub async fn close_tier3_pane(
    provider_id: String,
    proxy: State<'_, EventLoopProxy<PaneCommand>>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    proxy
        .send_event(PaneCommand::Close { key: provider_id })
        .map_err(|_| "tier3_pane event loop is no longer running".to_string())?;
    let _ = app_handle.run_on_main_thread(|| {});
    Ok(())
}

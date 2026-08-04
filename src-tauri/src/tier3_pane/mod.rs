//! Tier 2/Tier 3 external session embedding -- CEF off-screen rendering.
//!
//! Traces to: items.id=3 (Frontend SPA build), States 4-5 (split screen /
//! pane compaction) in 03_ProjectDocs/Specifications/TIER3_ACCESS_MODEL.md.
//! Mechanism decided by decisions.id=699 (Option 3b, CEF OSR) after the
//! research track documented in full in
//! Working/ADR_TIER3_EXTERNAL_SESSION_MODEL_20260730.md.
//!
//! # Phase A scope (this module, as of 2026-08-01)
//! Get a single CEF OSR pane rendering and coexisting correctly alongside
//! the Tauri app window. No compaction, no multi-pane, no popups, no IPC
//! command surface yet -- those are later phases (B/C) per the plan agreed
//! with Jason this session. Phase A's own exit criteria (agreed before any
//! code was written):
//!   - Application starts without deadlock.
//!   - Tauri/React UI remains responsive.
//!   - One CEF pane renders continuously.
//!   - Keyboard/mouse input reaches the pane.
//!   - Tauri UI remains interactive while the pane is active.
//!   - Cookie-jar isolation and persistence reconfirmed via the same
//!     independent SQLite ground-truth method items.id=195 used -- not
//!     just CEF's own API self-report.
//!   - No crashes across repeated move/resize of the Tauri window.
//!   - Message pump continues servicing both CEF and Tauri without one
//!     starving the other, checked over a sustained run (minutes, not
//!     seconds).
//!
//! # Architecture (decided 2026-08-01, see handoff for full writeup)
//! Two questions were resolved before any code was written here:
//!
//! 1. **Process/thread model**: CEF and Tauri's underlying GUI toolkit
//!    (GTK/Cocoa/Win32) both require their own work to happen on the OS
//!    main thread. This is satisfied by interleaving both into ONE shared
//!    main-thread loop, not by each owning a separate loop or process.
//!    CEF's `external_message_pump` + `on_schedule_message_pump_work()`
//!    (reused from the proven spike pattern, items.id=200) is interleaved
//!    into Tauri's own `RunEvent::MainEventsCleared`. This assumption is
//!    NOT yet verified inside an actual Tauri process -- Phase A is where
//!    that gets tested, not assumed from primary-source docs alone.
//!
//! 2. **Windowing/compositing**: no supported path was identified for
//!    compositing CEF's OSR output directly into Tauri's own webview
//!    surface (wry/Tauri expose no hook for injecting arbitrary GPU
//!    content into the system webview's compositor surface). Native
//!    OS-level child-window reparenting is unavailable on Wayland (the
//!    project's primary dev/test platform) in both `winit`
//!    (`with_parent_window` docs: "Wayland: Unsupported") and CEF's own
//!    windowed mode (chromiumembedded/cef#2804, an unshipped, years-old
//!    embedding proposal). Windows/X11 reparenting gives positional
//!    confinement only, not auto-sync on move/resize. macOS's native
//!    `NSWindow addChildWindow:` gives genuine OS-driven auto-sync, but
//!    `winit` does not expose it (open request since 2017, issue #220).
//!
//!    Decision: build ONE manually-synced separate window architecture
//!    across all platforms, rather than forking into a "good path" for
//!    some platforms and a workaround for others -- because except for
//!    macOS, no platform actually gets a free ride from native
//!    reparenting (Windows/X11 still need the same manual sync logic to
//!    avoid lag). Native macOS `addChildWindow:` is a tracked future
//!    optimization opportunity, not an architectural dependency of this
//!    phase.
//!
//! # Known limitations carried forward, not solved this phase
//! - Sync window will lag on rapid resize (manual sync, not eliminated --
//!   consistent with the ADR's own accepted risk for this class of
//!   approach).
//! - macOS: same manual-sync behavior as Windows/Linux for now.
//! - Popups/dropdowns, IME, multi-pane, compaction: out of scope.
//! - Windows/macOS: entirely untested by any prior spike in this track:
//!   every proven result (isolation, Wayland, NVIDIA/GBM) is Linux-only.

pub mod bootstrap;
pub mod render;
pub mod sync_window;

pub use bootstrap::dispatch_cef_subprocess;

/// Identifies one pane across `sync_window`/`render`/CEF `RequestContext`
/// wiring. Deliberately the provider's own `provider_store::Provider::id`
/// (a stable, meaningful string already unique across the selector's cap-of-3
/// distinct providers, decisions.id=681) -- not a positional index (fragile:
/// which pane is "index 1" once a different pane closes?) and not a UUID
/// (adds indirection with no payoff, since providers are already uniquely
/// identified and multi-account was resolved as logout/login-managed, not
/// simultaneous same-provider panes -- decided 2026-08-04, handoff id=148).
pub type PaneKey = String;

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
//!    Decision (2026-08-01, SUPERSEDED 2026-08-07, see below): build ONE
//!    manually-synced separate window architecture across all platforms,
//!    rather than forking into a "good path" for some platforms and a
//!    workaround for others -- because except for macOS, no platform
//!    actually gets a free ride from native reparenting (Windows/X11 still
//!    need the same manual sync logic to avoid lag). Native macOS
//!    `addChildWindow:` is a tracked future optimization opportunity, not
//!    an architectural dependency of that phase.
//!
//! # Real positioning fix, Linux (2026-08-07) -- supersedes the "separate
//! # window" half of the decision above
//! The separate-window design's `window.set_outer_position()`/
//! `request_inner_size()` sync depended on
//! `tauri::Window::inner_position()`/`inner_size()` to locate the main
//! window on screen -- confirmed broken on Wayland by manual verification
//! (pinned, wrong `x=0,y=0` plus an oversized rect on this dev machine's
//! KDE/Wayland session, consistent with the real upstream issues
//! tauri-apps/tauri#12411 and tauri-apps/tao#566). A same-night spike
//! proved the real fix: wgpu can present into a `gtk::GLArea` overlaid on
//! Tauri's own webview, in Tauri's own window, via wgpu-hal's
//! external-GL-context interop (`wgpu_hal::gles::Adapter::new_external`) --
//! confirmed with matched `glReadPixels` values across 19 frames and direct
//! visual confirmation of a wgpu-rendered pane composited over a real
//! webview. See `pane_host.rs`'s own module doc for the full architecture
//! and what it replaces.
//!
//! **This reverses the "one architecture across all platforms" policy
//! above, for Linux specifically.** `gtk::GLArea` has no equivalent on
//! Windows (WebView2/HWND) or macOS (WKWebView/NSWindow) -- consistent with
//! every other result in this track being Linux-only (isolation, native
//! Wayland, NVIDIA/GBM, popup delegation), this fix ships for Linux now;
//! Windows/macOS need their own equivalent single-window host mechanism,
//! not designed here.
//!
//! `winit` is dropped from this module entirely as a consequence (see
//! `pane_host.rs`) -- CEF's panes were always off-screen-rendered, so the
//! per-pane `winit::Window` this decision originally required only ever
//! existed to give wgpu a presentation surface and a window to
//! position-sync; neither is needed once wgpu presents into GTK's own
//! framebuffer instead.
//!
//! # Known limitations carried forward, not solved this phase
//! - **RESOLVED on Linux (2026-08-07):** sync lag on rapid resize -- no
//!   longer a *separate OS window* being resized (there isn't one), just
//!   ordinary GTK widget layout.
//! - macOS/Windows: this fix is Linux-only (see above) -- both platforms
//!   still need their own single-window host mechanism designed, not just
//!   the old manual-sync behavior this phase originally left them with.
//! - Popups/dropdowns, IME: still out of scope, unchanged.
//! - Mouse/keyboard/focus input forwarding into CEF: **not implemented at
//!   any layer**, old or new design -- a real, pre-existing gap, flagged
//!   explicitly rather than discovered by surprise later. Panes render but
//!   cannot currently be clicked or typed into.
//! - Windows/macOS: entirely untested by any prior spike in this track:
//!   every proven result (isolation, Wayland, NVIDIA/GBM, single-window
//!   compositing) is Linux-only.

pub mod bootstrap;
pub mod gl_loader;
pub mod pane_host;
pub mod render;

pub use bootstrap::dispatch_cef_subprocess;

/// Identifies one pane across `pane_host`/`render`/CEF `RequestContext`
/// wiring. Deliberately the provider's own `provider_store::Provider::id`
/// (a stable, meaningful string already unique across the selector's cap-of-3
/// distinct providers, decisions.id=681) -- not a positional index (fragile:
/// which pane is "index 1" once a different pane closes?) and not a UUID
/// (adds indirection with no payoff, since providers are already uniquely
/// identified and multi-account was resolved as logout/login-managed, not
/// simultaneous same-provider panes -- decided 2026-08-04, handoff id=148).
pub type PaneKey = String;
